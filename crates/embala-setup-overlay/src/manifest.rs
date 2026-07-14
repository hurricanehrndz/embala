//! Overlay manifest (spec R8): product identity + resolved install mode.
//!
//! Like the [`crate::Trailer`], the manifest is overlay-format shared by both
//! sides: the package-time writer (`embala-setup`) serializes it into the
//! manifest section, and the install-time runtime (`embala-setup-runtime`)
//! deserializes it to build the `embala.package` table and resolve the install
//! mode. Serialization is `serde_json` over structs with a fixed field order, so
//! the bytes are deterministic (no map reordering, no timestamps) — the
//! reproducibility invariant that makes signing/caching viable later.

use serde::{Deserialize, Serialize};

/// The manifest embedded in the overlay's manifest section.
///
/// `arch` and `install_mode` are plain strings (`"x86_64"`/`"aarch64"`,
/// `"per-user"`/`"per-machine"`/`"user-choice"`) rather than enums so this
/// crate stays dependency-light and the writer/runtime each keep their own
/// domain enums without a shared vocabulary type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Echoes [`crate::FORMAT_VERSION`]; a cross-check against the trailer.
    pub format_version: u32,
    /// `"x86_64"` | `"aarch64"` — the stub arch this overlay was built for.
    pub arch: String,
    /// Resolved default install mode: `"per-user"` | `"per-machine"` |
    /// `"user-choice"`. A `/mode=` CLI flag overrides it at install time.
    pub install_mode: String,
    /// Product identity mirrored from `[package]`.
    pub package: Package,
    /// Declarative uninstall options (spec R2), shown as checkboxes by the
    /// uninstaller's confirm dialog. `#[serde(default)]`: an older sidecar
    /// without the field parses to an empty list (no `FORMAT_VERSION` bump).
    #[serde(default)]
    pub uninstall_options: Vec<UninstallOption>,
}

/// One declarative uninstall option (spec R2). `default` pre-ticks its checkbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UninstallOption {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub default: bool,
}

/// Product identity, mirrored from the embala `[package]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

impl Manifest {
    /// Serialize to deterministic JSON bytes (the writer appends these).
    pub fn to_bytes(&self) -> Vec<u8> {
        // serde_json over a struct emits fields in declaration order with no
        // trailing whitespace — deterministic by construction.
        serde_json::to_vec(self).expect("manifest serializes")
    }

    /// Parse the manifest section bytes (the runtime reads these).
    pub fn parse(bytes: &[u8]) -> Result<Manifest, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            format_version: crate::FORMAT_VERSION,
            arch: "x86_64".to_string(),
            install_mode: "per-user".to_string(),
            package: Package {
                name: "hello".to_string(),
                display_name: "Embala Hello".to_string(),
                version: "0.1.0".to_string(),
                identifier: "ca.hrndz.embala.hello".to_string(),
                publisher: "Carlos Hernandez".to_string(),
                description: "test".to_string(),
                homepage: Some("https://example.com".to_string()),
                license: None,
            },
            uninstall_options: vec![UninstallOption {
                id: "purge-data".to_string(),
                label: "Delete all data".to_string(),
                default: false,
            }],
        }
    }

    #[test]
    fn round_trips_through_bytes() {
        // Why: the writer serializes and the runtime deserializes *one* type; a
        // field that does not survive the round trip would leave the installer
        // with a wrong DisplayName/version/identifier in the ARP entry.
        let m = sample();
        let parsed = Manifest::parse(&m.to_bytes()).expect("valid manifest");
        assert_eq!(parsed, m);
    }

    #[test]
    fn serialization_is_deterministic() {
        // Why: byte-reproducibility of the whole setup.exe depends on the
        // manifest bytes being a pure function of the struct — no map ordering.
        assert_eq!(sample().to_bytes(), sample().to_bytes());
    }

    #[test]
    fn json_without_uninstall_options_parses() {
        // Why: a sidecar written by a pre-Phase-2 installer has no
        // `uninstall_options` field; `#[serde(default)]` must let it parse to an
        // empty list so old uninstall.exe sidecars keep working (spec R2 compat).
        let json = br#"{"format_version":2,"arch":"x86_64","install_mode":"per-user","package":{"name":"hello","display_name":"Embala Hello","version":"0.1.0","identifier":"ca.hrndz.embala.hello","publisher":"Carlos Hernandez","description":"test","homepage":null,"license":null}}"#;
        let parsed = Manifest::parse(json).expect("old sidecar must still parse");
        assert!(parsed.uninstall_options.is_empty());
    }
}
