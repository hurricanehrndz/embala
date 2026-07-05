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
        }
        Ok(())
    }
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
        assert_eq!(config.msi.unwrap().arch, MsiArch::X86_64);
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
}
