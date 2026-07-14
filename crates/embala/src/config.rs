//! `embala.toml` schema and validation.

// Section fields go live as each backend consumes them; until every backend
// exists the compiler sees some as unread.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    pub package: Package,
    pub msi: Option<MsiSection>,
    pub app: Option<AppSection>,
    pub pkg: Option<PkgSection>,
    pub nupkg: Option<NupkgSection>,
    pub setup: Option<SetupSection>,
    pub sign: Option<SignSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Package {
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub identifier: String,
    pub publisher: String,
    pub description: String,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub copyright: Option<String>,
    pub tags: Option<Vec<String>>,
    pub icon: Option<PathBuf>,
    #[serde(default)]
    pub require_license_acceptance: bool,
}

/// A payload file: `src` is resolved relative to the config file's directory,
/// `dest` is the format-specific install-relative path.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub src: PathBuf,
    pub dest: String,
}

/// Windows Installer only supports 64-bit targets in embala; 32-bit configs
/// are rejected at parse time (unknown variant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum MsiArch {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "aarch64")]
    Aarch64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct MsiSection {
    pub arch: MsiArch,
    pub files: Vec<FileEntry>,
    pub main_executable: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct AppSection {
    pub executable: PathBuf,
    pub icon: Option<PathBuf>,
    pub minimum_system_version: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PkgSection {
    pub files: Vec<FileEntry>,
    pub install_location: String,
    /// Marks the package installable into the user's home directory
    /// (`installer -target CurrentUserHomeDirectory`) — the sudo-free test hook.
    #[serde(default)]
    pub enable_user_home: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NupkgStyle {
    Embedded,
    Download,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct NupkgSection {
    pub style: NupkgStyle,
    #[serde(default)]
    pub files: Vec<FileEntry>,
    pub url: Option<String>,
    pub checksum: Option<String>,
    pub project_source_url: Option<String>,
    pub package_source_url: Option<String>,
    pub release_notes: Option<String>,
    pub icon_url: Option<String>,
    pub license_url: Option<String>,
}

/// Windows setup.exe selects the embedded stub by arch; only 64-bit targets
/// exist (unknown variants are rejected at parse time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
pub enum SetupArch {
    #[serde(rename = "x86_64")]
    #[value(name = "x86_64")]
    X86_64,
    #[serde(rename = "aarch64")]
    #[value(name = "aarch64")]
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum InstallMode {
    #[default]
    PerUser,
    PerMachine,
    UserChoice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ShortcutLocation {
    #[default]
    StartMenu,
    Desktop,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Component {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub files: Vec<FileEntry>,
}

/// A declarative uninstall option (spec R2): a labelled checkbox the uninstaller
/// confirm dialog shows, readable from `uninstall.lua` via `ui.selected(id)`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct UninstallOptionEntry {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Shortcut {
    pub name: String,
    /// Dest (relative to install dir) the shortcut targets.
    pub target: String,
    pub args: Option<String>,
    pub icon: Option<String>,
    #[serde(default)]
    pub location: ShortcutLocation,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SetupSection {
    pub arch: SetupArch,
    pub files: Vec<FileEntry>,
    /// Dest of the shortcut target + ARP DisplayIcon, relative to install dir.
    pub main_executable: Option<String>,
    /// Stub icon (PNG/ICO), relative to the config dir; falls back to
    /// `[package].icon` (the `[app]` convention) when unset (spec R7).
    pub icon: Option<PathBuf>,
    /// Wizard banner (BMP, magic `BM`), relative to the config dir, patched as
    /// the `EMBALA_BANNER` resource (spec R7).
    pub banner: Option<PathBuf>,
    #[serde(default)]
    pub install_mode: InstallMode,
    /// Path (relative to the config dir) to a license text file.
    pub license: Option<PathBuf>,
    #[serde(default)]
    pub components: Vec<Component>,
    #[serde(default)]
    pub shortcuts: Vec<Shortcut>,
    /// Declarative uninstall options (spec R2). Allowed with both declarative and
    /// raw-`script` configs (they concern uninstall, not install lowering).
    #[serde(default)]
    pub uninstall_options: Vec<UninstallOptionEntry>,
    /// Escape-hatch path to a raw `install.lua` (relative to the config dir).
    pub script: Option<PathBuf>,
    pub uninstall_script: Option<PathBuf>,
}

impl SetupSection {
    /// Every dest a completed install would carry (top-level + component files).
    /// Used to check shortcut/main-executable targets resolve (spec R20).
    fn installed_dests(&self) -> Vec<&str> {
        self.files
            .iter()
            .chain(self.components.iter().flat_map(|c| c.files.iter()))
            .map(|f| f.dest.as_str())
            .collect()
    }
}

/// User-supplied signing commands (spec R5/R6). embala never implements a
/// signature format — it only shells out to `command` with `$f` = the
/// artifact's absolute path.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SignSection {
    pub windows: Option<WindowsSign>,
    pub macos: Option<MacosSign>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct WindowsSign {
    pub command: String,
    /// Path (relative to the config dir) to a pre-signed bare uninstall stub;
    /// when set, embala embeds it instead of signing the embedded stub itself.
    pub signed_stub: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct MacosSign {
    pub command: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let p = &self.package;
        if p.name.is_empty()
            || !p
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            bail!("package.name must match [a-z0-9-]+ (got {:?})", p.name);
        }
        let version_parts: Vec<&str> = p.version.split('.').collect();
        if version_parts.len() != 3
            || version_parts
                .iter()
                .any(|s| s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()))
        {
            bail!(
                "package.version must be numeric x.y.z — MSI requires it (got {:?})",
                p.version
            );
        }
        let id_segments: Vec<&str> = p.identifier.split('.').collect();
        if id_segments.len() < 2
            || id_segments
                .iter()
                .any(|s| s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        {
            bail!(
                "package.identifier must be reverse-DNS-like, e.g. com.example.app (got {:?})",
                p.identifier
            );
        }
        // Tags are space-joined into the nuspec <tags> element, so whitespace
        // inside one tag would silently split it.
        for tag in p.tags.iter().flatten() {
            if tag.is_empty() || tag.chars().any(char::is_whitespace) {
                bail!(
                    "package.tags entries must be non-empty and contain no whitespace (got {:?})",
                    tag
                );
            }
        }
        if let Some(nupkg) = &self.nupkg {
            match nupkg.style {
                NupkgStyle::Download if nupkg.url.is_none() || nupkg.checksum.is_none() => {
                    bail!("nupkg: style \"download\" requires both url and checksum");
                }
                NupkgStyle::Embedded if nupkg.url.is_some() || nupkg.checksum.is_some() => {
                    bail!("nupkg: url/checksum are only valid with style \"download\"");
                }
                _ => {}
            }
            if p.require_license_acceptance && nupkg.license_url.is_none() {
                bail!(
                    "package.require-license-acceptance = true requires nupkg.license-url \
                     (Chocolatey rejects the package otherwise)"
                );
            }
        }
        if let Some(setup) = &self.setup {
            validate_setup(setup)?;
        }
        if let Some(sign) = &self.sign {
            // signed-stub existence is a build-time check (needs the config dir);
            // it lives in `build_setup`, mirroring `validate_setup_paths`.
            for (platform, command) in [
                ("windows", sign.windows.as_ref().map(|w| &w.command)),
                ("macos", sign.macos.as_ref().map(|m| &m.command)),
            ] {
                if let Some(command) = command {
                    if command.is_empty() || !command.contains("$f") {
                        bail!(
                            "sign.{platform}: command must be non-empty and contain $f (got {:?})",
                            command
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

/// Structural `[setup]` validation (spec R20), no filesystem access. The `src`-
/// existence and `license`-path checks need the config-dir base and so live in
/// `build_setup` (`main.rs`), which resolves it.
fn validate_setup(setup: &SetupSection) -> Result<()> {
    // A raw script owns install behavior; the declarative sugar it would
    // conflict with must not also be present (surface it, don't silently drop).
    if setup.script.is_some()
        && (!setup.components.is_empty()
            || !setup.shortcuts.is_empty()
            || setup.main_executable.is_some())
    {
        bail!(
            "setup: `script` cannot be combined with `components`, `shortcuts`, or \
             `main-executable` — the raw script owns that behavior"
        );
    }
    let mut ids = std::collections::BTreeSet::new();
    for component in &setup.components {
        if !ids.insert(component.id.as_str()) {
            bail!("setup: duplicate component id {:?}", component.id);
        }
    }
    // Uninstall options: non-empty unique ids, non-empty labels (spec R2). The
    // ids reach `ui.selected(id)` at uninstall, so a blank/dup id is a hard error.
    let mut opt_ids = std::collections::BTreeSet::new();
    for option in &setup.uninstall_options {
        if option.id.is_empty() {
            bail!("setup: uninstall-option id must be non-empty");
        }
        if option.label.is_empty() {
            bail!(
                "setup: uninstall-option {:?} label must be non-empty",
                option.id
            );
        }
        if !opt_ids.insert(option.id.as_str()) {
            bail!("setup: duplicate uninstall-option id {:?}", option.id);
        }
    }
    let dests = setup.installed_dests();
    if let Some(main) = &setup.main_executable {
        if !dests.contains(&main.as_str()) {
            bail!(
                "setup: main-executable {:?} is not among the installed file dests",
                main
            );
        }
    }
    for shortcut in &setup.shortcuts {
        if !dests.contains(&shortcut.target.as_str()) {
            bail!(
                "setup: shortcut {:?} target {:?} is not among the installed file dests",
                shortcut.name,
                shortcut.target
            );
        }
    }
    Ok(())
}

/// Filesystem `[setup]` validation (spec R20): every `src` and the optional
/// `license`/`script` paths must exist, resolved relative to `base` (the config
/// file's directory). Called from `build_setup`, which knows `base`.
pub fn validate_setup_paths(setup: &SetupSection, base: &Path) -> Result<()> {
    let srcs = setup
        .files
        .iter()
        .chain(setup.components.iter().flat_map(|c| c.files.iter()))
        .map(|f| &f.src);
    for src in srcs {
        let resolved = base.join(src);
        if !resolved.exists() {
            bail!("setup: payload src {} does not exist", resolved.display());
        }
    }
    for (field, path) in [
        ("license", &setup.license),
        ("script", &setup.script),
        ("uninstall-script", &setup.uninstall_script),
        ("icon", &setup.icon),
        ("banner", &setup.banner),
    ] {
        if let Some(path) = path {
            let resolved = base.join(path);
            if !resolved.exists() {
                bail!("setup: {field} path {} does not exist", resolved.display());
            }
        }
    }
    // The banner is patched as a raw bitmap resource, so reject anything that is
    // not a BMP up front rather than shipping a broken wizard (spec R7).
    if let Some(banner) = &setup.banner {
        let resolved = base.join(banner);
        let head = std::fs::read(&resolved)?;
        if !head.starts_with(b"BM") {
            bail!(
                "setup: banner {} must be a BMP (magic \"BM\")",
                resolved.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_toml() -> String {
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/hello/embala.toml"
        ))
        .expect("fixture config")
    }

    fn parse(text: &str) -> Result<Config> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn fixture_config_parses_with_all_sections() {
        let config = parse(&fixture_toml()).expect("fixture must be valid");
        assert_eq!(config.package.name, "hello");
        assert_eq!(config.package.identifier, "ca.hrndz.embala.hello");
        assert!(config.msi.is_some());
        assert!(config.app.is_some());
        assert!(config.pkg.is_some());
        assert!(config.nupkg.is_some());
        assert!(config.setup.is_some());
        // Phase 4: the fixture carries the nupkg metadata end-to-end. Keep the
        // [package].icon fallback path exercised — the fixture no longer sets
        // [app].icon, so the .app builder must fall back to [package].icon.
        assert_eq!(
            config.package.copyright.as_deref(),
            Some("© 2026 Carlos Hernandez")
        );
        assert_eq!(
            config.package.tags.as_deref(),
            Some(&["cli".to_string(), "embala".to_string(), "demo".to_string()][..])
        );
        assert_eq!(config.package.icon.as_deref(), Some(Path::new("icon.png")));
        // require-license-acceptance stays unset so the choco-install gate is
        // acceptance-prompt-free.
        assert!(!config.package.require_license_acceptance);
        assert!(config.app.as_ref().unwrap().icon.is_none());
        let nupkg = config.nupkg.as_ref().unwrap();
        assert_eq!(
            nupkg.license_url.as_deref(),
            Some("https://github.com/hurricanehrndz/embala/blob/main/LICENSE")
        );
        assert_eq!(
            nupkg.icon_url.as_deref(),
            Some("https://github.com/hurricanehrndz/embala/raw/main/fixtures/hello/icon.png")
        );
        assert_eq!(config.msi.unwrap().arch, MsiArch::X86_64);
        let setup = config.setup.unwrap();
        assert_eq!(setup.arch, SetupArch::X86_64);
        // install-mode defaults to per-user when omitted (spec R18).
        assert_eq!(setup.install_mode, InstallMode::PerUser);
        // The fixture drives the declarative (lowered) path, not the raw-script
        // escape hatch: main-executable + license + a component, no `script`.
        assert_eq!(setup.main_executable.as_deref(), Some("hello.exe"));
        assert_eq!(setup.license.as_deref(), Some(Path::new("license.txt")));
        assert!(setup.script.is_none());
        assert_eq!(setup.components.len(), 1);
        assert_eq!(setup.components[0].id, "docs");
        assert!(setup.components[0].default);
        assert_eq!(setup.components[0].files[0].dest, "readme.txt");
    }

    /// The fixture with its Phase-4 metadata keys stripped, so the mutation
    /// tests below can splice their own variants without colliding
    /// (duplicate-key) with the metadata the fixture now carries end-to-end.
    fn fixture_toml_without_metadata() -> String {
        const STRIPPED: &[&str] = &[
            "copyright =",
            "tags =",
            "icon =",
            "project-source-url =",
            "package-source-url =",
            "release-notes =",
            "icon-url =",
            "license-url =",
        ];
        fixture_toml()
            .lines()
            .filter(|line| {
                let key = line.trim_start();
                !STRIPPED.iter().any(|prefix| key.starts_with(prefix))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Splice extra keys into the fixture's `[package]` table (after
    /// `license`) and its `[nupkg]` table (after `style`).
    fn with_metadata(package_keys: &str, nupkg_keys: &str) -> String {
        fixture_toml_without_metadata()
            .replace(
                "license = \"MIT\"",
                &format!("license = \"MIT\"\n{package_keys}"),
            )
            .replace(
                "style = \"embedded\"",
                &format!("style = \"embedded\"\n{nupkg_keys}"),
            )
    }

    #[test]
    fn new_metadata_keys_parse() {
        let config = parse(&with_metadata(
            "copyright = \"© 2026 Carlos Hernandez\"\n\
             tags = [\"cli\", \"demo\"]\n\
             icon = \"icon.png\"\n\
             require-license-acceptance = true",
            "project-source-url = \"https://github.com/hurricanehrndz/embala\"\n\
             package-source-url = \"https://github.com/hurricanehrndz/embala-packages\"\n\
             release-notes = \"First release.\"\n\
             icon-url = \"https://example.com/icon.png\"\n\
             license-url = \"https://example.com/LICENSE\"",
        ))
        .expect("new metadata keys must parse");
        let p = &config.package;
        assert_eq!(p.copyright.as_deref(), Some("© 2026 Carlos Hernandez"));
        assert_eq!(
            p.tags.as_deref(),
            Some(&["cli".to_string(), "demo".to_string()][..])
        );
        assert_eq!(p.icon.as_deref(), Some(Path::new("icon.png")));
        assert!(p.require_license_acceptance);
        let n = config.nupkg.unwrap();
        assert_eq!(
            n.project_source_url.as_deref(),
            Some("https://github.com/hurricanehrndz/embala")
        );
        assert_eq!(
            n.package_source_url.as_deref(),
            Some("https://github.com/hurricanehrndz/embala-packages")
        );
        assert_eq!(n.release_notes.as_deref(), Some("First release."));
        assert_eq!(n.icon_url.as_deref(), Some("https://example.com/icon.png"));
        assert_eq!(
            n.license_url.as_deref(),
            Some("https://example.com/LICENSE")
        );
    }

    #[test]
    fn require_license_acceptance_needs_a_license_url() {
        let err = parse(&with_metadata("require-license-acceptance = true", ""))
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires nupkg.license-url"), "{err}");
        parse(&with_metadata(
            "require-license-acceptance = true",
            "license-url = \"https://example.com/LICENSE\"",
        ))
        .expect("license-url satisfies the coupling rule");
    }

    #[test]
    fn whitespace_in_a_tag_is_rejected() {
        let err = parse(&with_metadata("tags = [\"cli tool\"]", ""))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no whitespace"), "{err}");
    }

    #[test]
    fn non_numeric_version_is_rejected() {
        let text = fixture_toml().replace("version = \"0.1.0\"", "version = \"0.1.0-beta\"");
        let err = parse(&text).unwrap_err().to_string();
        assert!(err.contains("numeric x.y.z"), "unexpected error: {err}");
    }

    #[test]
    fn unknown_key_is_rejected() {
        let text = format!("{}\n[msi2]\narch = \"x86_64\"\n", fixture_toml());
        assert!(parse(&text).is_err());
    }

    #[test]
    fn thirty_two_bit_arch_is_rejected() {
        let text = fixture_toml().replace("arch = \"x86_64\"", "arch = \"x86\"");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn download_style_requires_url_and_checksum() {
        let text = fixture_toml().replace("style = \"embedded\"", "style = \"download\"");
        let err = parse(&text).unwrap_err().to_string();
        assert!(
            err.contains("requires both url and checksum"),
            "unexpected error: {err}"
        );
    }

    /// A `[setup]` block with the full R18 surface for the validation tests.
    const SETUP_FULL: &str = r#"
[setup]
arch = "x86_64"
main-executable = "hello.exe"
install-mode = "user-choice"
files = [{ src = "dist/hello.exe", dest = "hello.exe" }]
components = [{ id = "core", label = "Core", default = true, files = [{ src = "dist/hello.exe", dest = "extra.exe" }] }]
shortcuts = [{ name = "Hello", target = "hello.exe", location = "desktop" }]
"#;

    fn with_setup(block: &str) -> String {
        // Drop the fixture's own [setup] section, then append the test block.
        let base = fixture_toml();
        let cut = base.find("\n[setup]").unwrap_or(base.len());
        format!("{}{}", &base[..cut], block)
    }

    #[test]
    fn full_setup_section_parses() {
        let config = parse(&with_setup(SETUP_FULL)).expect("full setup section must be valid");
        let setup = config.setup.unwrap();
        assert_eq!(setup.install_mode, InstallMode::UserChoice);
        assert_eq!(setup.components.len(), 1);
        assert_eq!(setup.shortcuts[0].location, ShortcutLocation::Desktop);
    }

    #[test]
    fn unknown_setup_arch_is_rejected() {
        let text = with_setup(SETUP_FULL).replace("arch = \"x86_64\"", "arch = \"x86\"");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn main_executable_not_in_dests_is_rejected() {
        let text = with_setup(SETUP_FULL).replace(
            "main-executable = \"hello.exe\"",
            "main-executable = \"nope.exe\"",
        );
        let err = parse(&text).unwrap_err().to_string();
        assert!(err.contains("not among the installed file dests"), "{err}");
    }

    #[test]
    fn duplicate_component_ids_are_rejected() {
        let block = r#"
[setup]
arch = "x86_64"
files = [{ src = "dist/hello.exe", dest = "hello.exe" }]
components = [
  { id = "core", label = "A" },
  { id = "core", label = "B" },
]
"#;
        let err = parse(&with_setup(block)).unwrap_err().to_string();
        assert!(err.contains("duplicate component id"), "{err}");
    }

    #[test]
    fn uninstall_options_parse() {
        let block = r#"
[setup]
arch = "x86_64"
main-executable = "hello.exe"
files = [{ src = "dist/hello.exe", dest = "hello.exe" }]
uninstall-options = [
  { id = "purge-data", label = "Delete all data", default = false },
  { id = "keep-logs", label = "Keep logs" },
]
"#;
        let config = parse(&with_setup(block)).expect("uninstall-options must parse");
        let setup = config.setup.unwrap();
        assert_eq!(setup.uninstall_options.len(), 2);
        assert_eq!(setup.uninstall_options[0].id, "purge-data");
        assert!(!setup.uninstall_options[0].default);
        assert_eq!(setup.uninstall_options[1].label, "Keep logs");
    }

    #[test]
    fn duplicate_uninstall_option_ids_are_rejected() {
        let block = r#"
[setup]
arch = "x86_64"
files = [{ src = "dist/hello.exe", dest = "hello.exe" }]
uninstall-options = [
  { id = "purge-data", label = "A" },
  { id = "purge-data", label = "B" },
]
"#;
        let err = parse(&with_setup(block)).unwrap_err().to_string();
        assert!(err.contains("duplicate uninstall-option id"), "{err}");
    }

    #[test]
    fn empty_uninstall_option_id_or_label_is_rejected() {
        let empty_id = r#"
[setup]
arch = "x86_64"
files = [{ src = "dist/hello.exe", dest = "hello.exe" }]
uninstall-options = [{ id = "", label = "A" }]
"#;
        assert!(
            parse(&with_setup(empty_id))
                .unwrap_err()
                .to_string()
                .contains("id must be non-empty")
        );
        let empty_label = r#"
[setup]
arch = "x86_64"
files = [{ src = "dist/hello.exe", dest = "hello.exe" }]
uninstall-options = [{ id = "purge-data", label = "" }]
"#;
        assert!(
            parse(&with_setup(empty_label))
                .unwrap_err()
                .to_string()
                .contains("label must be non-empty")
        );
    }

    #[test]
    fn banner_without_bm_magic_is_rejected() {
        // Why: the banner is patched into the stub as a raw bitmap resource, so a
        // non-BMP would ship a broken wizard; reject it at validation (spec R7).
        let dir = std::env::temp_dir().join("embala-banner-magic-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::write(dir.join("dist/hello.exe"), b"MZ").unwrap();
        std::fs::write(dir.join("bad.bmp"), b"NOTBMP").unwrap();
        std::fs::write(dir.join("good.bmp"), b"BM and pixels").unwrap();

        let block = r#"
[setup]
arch = "x86_64"
banner = "bad.bmp"
files = [{ src = "dist/hello.exe", dest = "hello.exe" }]
"#;
        let config: Config = toml::from_str(&with_setup(block)).unwrap();
        let err = validate_setup_paths(config.setup.as_ref().unwrap(), &dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be a BMP"), "{err}");

        let good = with_setup(block).replace("bad.bmp", "good.bmp");
        let config: Config = toml::from_str(&good).unwrap();
        validate_setup_paths(config.setup.as_ref().unwrap(), &dir).expect("BM magic accepted");
    }

    #[test]
    fn script_with_sugar_is_rejected() {
        let block = r#"
[setup]
arch = "x86_64"
script = "install.lua"
files = [{ src = "dist/hello.exe", dest = "hello.exe" }]
shortcuts = [{ name = "Hello", target = "hello.exe" }]
"#;
        let err = parse(&with_setup(block)).unwrap_err().to_string();
        assert!(err.contains("cannot be combined with"), "{err}");
    }

    #[test]
    fn sign_section_parses() {
        let text = format!(
            "{}\n[sign.windows]\ncommand = \"osslsigncode sign -in $f -out $f\"\n\
             signed-stub = \"signed-stub.exe\"\n[sign.macos]\ncommand = \"rcodesign sign $f\"\n",
            fixture_toml()
        );
        let config = parse(&text).expect("sign section must parse");
        let sign = config.sign.unwrap();
        assert_eq!(
            sign.windows.as_ref().unwrap().signed_stub.as_deref(),
            Some(Path::new("signed-stub.exe"))
        );
        assert_eq!(sign.macos.unwrap().command, "rcodesign sign $f");
    }

    #[test]
    fn sign_command_without_placeholder_is_rejected() {
        let text = format!(
            "{}\n[sign.windows]\ncommand = \"osslsigncode sign cert.pfx\"\n",
            fixture_toml()
        );
        let err = parse(&text).unwrap_err().to_string();
        assert!(err.contains("must be non-empty and contain $f"), "{err}");
    }
}
