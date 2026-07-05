mod config;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Msi,
    App,
    Pkg,
    Nupkg,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Format::Msi => "msi",
            Format::App => "app",
            Format::Pkg => "pkg",
            Format::Nupkg => "nupkg",
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
    }
}

fn build(config_path: &Path, formats: &[Format], out_dir: &Path) -> Result<()> {
    let config = Config::load(config_path)?;

    let formats: Vec<Format> = if formats.is_empty() {
        let configured = [
            (Format::Msi, config.msi.is_some()),
            (Format::App, config.app.is_some()),
            (Format::Pkg, config.pkg.is_some()),
            (Format::Nupkg, config.nupkg.is_some()),
        ];
        let present: Vec<Format> = configured
            .iter()
            .filter(|(_, some)| *some)
            .map(|(format, _)| *format)
            .collect();
        if present.is_empty() {
            bail!(
                "no format sections ([msi], [app], [pkg], [nupkg]) in {}",
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
        Format::App => bail!("app: not implemented yet"),
        Format::Pkg => bail!("pkg: not implemented yet"),
        Format::Nupkg => bail!("nupkg: not implemented yet"),
    }
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
    Ok(())
}
