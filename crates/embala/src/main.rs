mod app;
mod config;
mod lower;
mod sign;

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use config::Config;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build installer packages from prebuilt artifacts
    Build {
        /// Path to the embala config file
        #[arg(long, default_value = "embala.toml")]
        config: PathBuf,
        /// Formats to build, comma-separated (default: every format with a
        /// section in the config)
        #[arg(long, value_delimiter = ',')]
        formats: Vec<Format>,
        /// Directory the artifacts are written into
        #[arg(long, default_value = "dist")]
        out_dir: PathBuf,
    },
    /// Write the embedded bare setup stub (for external signing flows)
    DumpStub {
        #[arg(long, default_value = "x86_64")]
        arch: config::SetupArch,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Msi,
    App,
    Pkg,
    Nupkg,
    Setup,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Format::Msi => "msi",
            Format::App => "app",
            Format::Pkg => "pkg",
            Format::Nupkg => "nupkg",
            Format::Setup => "setup",
        })
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Build {
            config,
            formats,
            out_dir,
        } => build(&config, &formats, &out_dir),
        Command::DumpStub { arch, out } => dump_stub(arch, &out),
    }
}

fn dump_stub(arch: config::SetupArch, out: &Path) -> Result<()> {
    let arch = match arch {
        config::SetupArch::X86_64 => embala_setup::SetupArch::X86_64,
        config::SetupArch::Aarch64 => embala_setup::SetupArch::Aarch64,
    };
    std::fs::write(out, embala_setup::stub_bytes(arch))?;
    println!("dump-stub: wrote {}", out.display());
    Ok(())
}

fn build(config_path: &Path, formats: &[Format], out_dir: &Path) -> Result<()> {
    let config = Config::load(config_path)?;

    let formats: Vec<Format> = if formats.is_empty() {
        let configured = [
            (Format::Msi, config.msi.is_some()),
            (Format::App, config.app.is_some()),
            (Format::Pkg, config.pkg.is_some()),
            (Format::Nupkg, config.nupkg.is_some()),
            (Format::Setup, config.setup.is_some()),
        ];
        let present: Vec<Format> = configured
            .iter()
            .filter(|(_, some)| *some)
            .map(|(format, _)| *format)
            .collect();
        if present.is_empty() {
            bail!(
                "no format sections ([msi], [app], [pkg], [nupkg], [setup]) in {}",
                config_path.display()
            );
        }
        present
    } else {
        formats.to_vec()
    };

    for format in formats {
        let section_present = match format {
            Format::Msi => config.msi.is_some(),
            Format::App => config.app.is_some(),
            Format::Pkg => config.pkg.is_some(),
            Format::Nupkg => config.nupkg.is_some(),
            Format::Setup => config.setup.is_some(),
        };
        if !section_present {
            bail!(
                "{format} requested but the config has no [{format}] section ({})",
                config_path.display()
            );
        }
        build_format(format, &config, config_path, out_dir)?;
    }
    Ok(())
}

fn build_format(format: Format, config: &Config, config_path: &Path, out_dir: &Path) -> Result<()> {
    match format {
        Format::Msi => build_msi(config, config_path, out_dir),
        Format::App => build_app(config, config_path, out_dir),
        Format::Pkg => build_pkg(config, config_path, out_dir),
        Format::Nupkg => build_nupkg(config, config_path, out_dir),
        Format::Setup => build_setup(config, config_path, out_dir),
    }
}

fn build_app(config: &Config, config_path: &Path, out_dir: &Path) -> Result<()> {
    let section = config.app.as_ref().expect("caller checked [app] presence");
    // Section paths are relative to the config file's directory.
    let base = config_path.parent().unwrap_or(Path::new("."));
    let out = app::build(&config.package, section, base, out_dir)?;
    println!("app: wrote {}", out.display());
    if let Some(macos) = macos_sign(config) {
        // `$f` is the `.app` directory path (spec R6).
        sign::run(&macos.command, &out)?;
    }
    Ok(())
}

fn build_pkg(config: &Config, config_path: &Path, out_dir: &Path) -> Result<()> {
    let section = config.pkg.as_ref().expect("caller checked [pkg] presence");
    // FileEntry.src is relative to the config file's directory.
    let base = config_path.parent().unwrap_or(Path::new("."));
    let package = &config.package;
    let spec = embala_pkg::PkgSpec {
        name: package.name.clone(),
        display_name: package.display_name.clone(),
        identifier: package.identifier.clone(),
        version: package.version.clone(),
        install_location: section.install_location.clone(),
        enable_user_home: section.enable_user_home,
        files: section
            .files
            .iter()
            .map(|f| embala_pkg::FileSpec {
                src: base.join(&f.src),
                dest: f.dest.clone(),
            })
            .collect(),
    };
    let out = out_dir.join(format!("{}-{}.pkg", package.name, package.version));
    std::fs::create_dir_all(out_dir)?;
    embala_pkg::build(&spec, &out)?;
    println!("pkg: wrote {}", out.display());
    if let Some(macos) = macos_sign(config) {
        sign::run(&macos.command, &out)?;
    }
    Ok(())
}

fn build_nupkg(config: &Config, config_path: &Path, out_dir: &Path) -> Result<()> {
    let section = config
        .nupkg
        .as_ref()
        .expect("caller checked [nupkg] presence");
    // FileEntry.src is relative to the config file's directory.
    let base = config_path.parent().unwrap_or(Path::new("."));
    let package = &config.package;
    let style = match section.style {
        config::NupkgStyle::Embedded => embala_nupkg::Style::Embedded {
            files: section
                .files
                .iter()
                .map(|f| embala_nupkg::FileSpec {
                    src: base.join(&f.src),
                    dest: f.dest.clone(),
                })
                .collect(),
        },
        config::NupkgStyle::Download => {
            // For download style the files list names the downloaded
            // payload's dest; one URL downloads one file.
            let [file] = section.files.as_slice() else {
                bail!(
                    "nupkg: style \"download\" needs exactly one files entry (its dest \
                     names the downloaded payload), got {}",
                    section.files.len()
                );
            };
            embala_nupkg::Style::Download {
                url: section.url.clone().expect("config validation requires url"),
                checksum: section
                    .checksum
                    .clone()
                    .expect("config validation requires checksum"),
                dest: file.dest.clone(),
            }
        }
    };
    let spec = embala_nupkg::NupkgSpec {
        name: package.name.clone(),
        display_name: package.display_name.clone(),
        version: package.version.clone(),
        identifier: package.identifier.clone(),
        publisher: package.publisher.clone(),
        description: package.description.clone(),
        homepage: package.homepage.clone(),
        license: package.license.clone(),
        copyright: package.copyright.clone(),
        tags: package.tags.clone().unwrap_or_default(),
        release_notes: section.release_notes.clone(),
        require_license_acceptance: package.require_license_acceptance,
        project_source_url: section.project_source_url.clone(),
        package_source_url: section.package_source_url.clone(),
        license_url: section.license_url.clone(),
        icon: package.icon.as_ref().map(|p| base.join(p)),
        icon_url: section.icon_url.clone(),
        style,
    };
    let out = out_dir.join(format!("{}-{}.nupkg", package.name, package.version));
    std::fs::create_dir_all(out_dir)?;
    embala_nupkg::build(&spec, &out)?;
    println!("nupkg: wrote {}", out.display());
    Ok(())
}

fn build_msi(config: &Config, config_path: &Path, out_dir: &Path) -> Result<()> {
    let section = config.msi.as_ref().expect("caller checked [msi] presence");
    // FileEntry.src is relative to the config file's directory.
    let base = config_path.parent().unwrap_or(Path::new("."));
    let package = &config.package;
    let spec = embala_msi::MsiSpec {
        name: package.name.clone(),
        display_name: package.display_name.clone(),
        version: package.version.clone(),
        identifier: package.identifier.clone(),
        publisher: package.publisher.clone(),
        description: package.description.clone(),
        homepage: package.homepage.clone(),
        arch: match section.arch {
            config::MsiArch::X86_64 => embala_msi::MsiArch::X86_64,
            config::MsiArch::Aarch64 => embala_msi::MsiArch::Aarch64,
        },
        main_executable: section.main_executable.clone(),
        files: section
            .files
            .iter()
            .map(|f| embala_msi::FileSpec {
                src: base.join(&f.src),
                dest: f.dest.clone(),
            })
            .collect(),
    };
    let out = out_dir.join(format!(
        "{}-{}-{}.msi",
        package.name,
        package.version,
        spec.arch.as_str()
    ));
    std::fs::create_dir_all(out_dir)?;
    embala_msi::build(&spec, &out)?;
    println!("msi: wrote {}", out.display());
    if let Some(windows) = windows_sign(config) {
        sign::run(&windows.command, &out)?;
    }
    Ok(())
}

/// `[sign.windows]` if configured — the guard that keeps unsigned builds on the
/// exact Phase-2 code path (spec R7).
fn windows_sign(config: &Config) -> Option<&config::WindowsSign> {
    config.sign.as_ref().and_then(|s| s.windows.as_ref())
}

/// `[sign.macos]` if configured.
fn macos_sign(config: &Config) -> Option<&config::MacosSign> {
    config.sign.as_ref().and_then(|s| s.macos.as_ref())
}

/// Obtain the (signed) uninstall stub for the setup.exe overlay when
/// `[sign.windows]` is configured (spec R6): the pre-signed `signed-stub` file
/// if given, else the embedded stub signed in place via the user's command.
fn resolve_signed_stub(
    windows: &config::WindowsSign,
    arch: embala_setup::SetupArch,
    base: &Path,
    out_dir: &Path,
) -> Result<Vec<u8>> {
    if let Some(rel) = &windows.signed_stub {
        let path = base.join(rel);
        if !path.exists() {
            bail!(
                "sign.windows: signed-stub {} does not exist",
                path.display()
            );
        }
        return Ok(std::fs::read(&path)?);
    }
    std::fs::create_dir_all(out_dir)?;
    let work = out_dir.join(format!(".embala-stub-{}.exe", arch.as_str()));
    std::fs::write(&work, embala_setup::stub_bytes(arch))?;
    sign::run(&windows.command, &work)?;
    let bytes = std::fs::read(&work)?;
    std::fs::remove_file(&work)?;
    Ok(bytes)
}

fn build_setup(config: &Config, config_path: &Path, out_dir: &Path) -> Result<()> {
    let section = config
        .setup
        .as_ref()
        .expect("caller checked [setup] presence");
    // FileEntry.src and license/script paths are relative to the config dir.
    let base = config_path.parent().unwrap_or(Path::new("."));
    config::validate_setup_paths(section, base)?;
    let package = &config.package;
    // Two front-ends, one substrate (spec R19): a raw `script` ships verbatim
    // (spec R7); otherwise the declarative `[setup]` is lowered to a generated
    // install.lua. `uninstall-script` ships verbatim on either path.
    let read_script = |path: &Option<PathBuf>| -> Result<Option<Vec<u8>>> {
        path.as_ref()
            .map(|p| std::fs::read(base.join(p)))
            .transpose()
            .map_err(Into::into)
    };
    let install_lua = match &section.script {
        Some(script) => Some(std::fs::read(base.join(script))?),
        None => {
            // Embed the license text (config-dir-relative) read at build time so
            // the generated script is self-contained (spec R7).
            let license_text = section
                .license
                .as_ref()
                .map(|p| std::fs::read_to_string(base.join(p)))
                .transpose()?;
            Some(lower::lower(package, section, license_text.as_deref()).into_bytes())
        }
    };
    // Ship top-level AND component payload files (Phase 2/3 shipped top-level
    // only); the declarative path installs components under a selected() gate.
    // Duplicate-dest collisions error in the writer.
    let files = section
        .files
        .iter()
        .chain(section.components.iter().flat_map(|c| c.files.iter()))
        .map(|f| embala_setup::FileSpec {
            src: base.join(&f.src),
            dest: f.dest.clone(),
        })
        .collect();
    let arch = match section.arch {
        config::SetupArch::X86_64 => embala_setup::SetupArch::X86_64,
        config::SetupArch::Aarch64 => embala_setup::SetupArch::Aarch64,
    };
    // Resolve the signed uninstall stub BEFORE assembling setup.exe (spec R6);
    // no `[sign.windows]` ships a None stub — the exact Phase-2 path (spec R7).
    let windows = windows_sign(config);
    let signed_stub = match windows {
        Some(w) => Some(resolve_signed_stub(w, arch, base, out_dir)?),
        None => None,
    };
    let spec = embala_setup::SetupSpec {
        arch,
        install_mode: match section.install_mode {
            config::InstallMode::PerUser => embala_setup::InstallMode::PerUser,
            config::InstallMode::PerMachine => embala_setup::InstallMode::PerMachine,
            config::InstallMode::UserChoice => embala_setup::InstallMode::UserChoice,
        },
        product: embala_setup::ProductInfo {
            name: package.name.clone(),
            display_name: package.display_name.clone(),
            version: package.version.clone(),
            identifier: package.identifier.clone(),
            publisher: package.publisher.clone(),
            description: package.description.clone(),
            homepage: package.homepage.clone(),
            license: package.license.clone(),
        },
        uninstall_options: section
            .uninstall_options
            .iter()
            .map(|o| embala_setup::UninstallOptionSpec {
                id: o.id.clone(),
                label: o.label.clone(),
                default: o.default,
            })
            .collect(),
        files,
        install_lua,
        uninstall_lua: read_script(&section.uninstall_script)?,
        signed_stub,
    };
    let out = out_dir.join(format!(
        "{}-{}-{}-setup.exe",
        package.name,
        package.version,
        spec.arch.as_str()
    ));
    std::fs::create_dir_all(out_dir)?;
    embala_setup::build(&spec, &out)?;
    println!("setup: wrote {}", out.display());
    if let Some(w) = windows {
        sign::run(&w.command, &out)?;
    }
    Ok(())
}
