//! Deterministic MSI GUIDs, derived from the package identity via UUIDv5.
//!
//! Deterministic GUIDs keep `.msi` builds reproducible (an identical spec
//! produces an identical installer) and give a stable `UpgradeCode` across
//! versions so newer installers can detect and replace older ones (Phase 3).
//
// Portions ported from deno desktop (cli/tools/desktop.rs),
// Copyright 2018-2026 the Deno authors, MIT license.

use uuid::Uuid;

/// Fixed namespace for every embala-derived MSI GUID. Changing this breaks
/// UpgradeCode continuity for every published package — never change it.
const NAMESPACE: Uuid = Uuid::from_u128(0xf3a2c9d4_8b1e_4d7a_9c5f_2e6b0a1d8f47);

/// Format a UUID as an MSI registry-format GUID: braced, uppercase, hyphenated
/// (38 chars, no lowercase), as the `Guid` column category requires.
fn braced(uuid: Uuid) -> String {
    format!("{{{}}}", uuid.hyphenated().to_string().to_uppercase())
}

fn v5(name: String) -> Uuid {
    Uuid::new_v5(&NAMESPACE, name.as_bytes())
}

/// Stable across versions: identifies the product line for major upgrades.
pub(crate) fn upgrade_code(identifier: &str) -> String {
    braced(v5(identifier.to_string()))
}

/// Unique per version: identifies this product release.
pub(crate) fn product_code(identifier: &str, version: &str) -> String {
    braced(v5(format!("{identifier}{version}")))
}

/// Identifies this exact package build (summary-info revision / PackageCode).
/// Raw `Uuid` because `SummaryInfo::set_uuid` does the MSI formatting itself.
pub(crate) fn package_uuid(identifier: &str, version: &str) -> Uuid {
    v5(format!("{identifier}{version}package"))
}

/// One GUID per file component, keyed by the file's install-relative dest.
pub(crate) fn component_guid(identifier: &str, dest: &str) -> String {
    braced(v5(format!("{identifier}{dest}")))
}

/// The Start-menu shortcut component. The NUL separator keeps this outside
/// the file-component namespace (no valid dest contains a NUL).
pub(crate) fn shortcut_component_guid(identifier: &str) -> String {
    braced(v5(format!("{identifier}\0shortcut")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Determinism lock: these exact values are what shipped installers carry.
    // If any of these assertions fail, GUID derivation changed and every
    // existing install's UpgradeCode/ComponentId continuity is broken.
    const IDENT: &str = "ca.hrndz.embala.hello";

    #[test]
    fn upgrade_code_is_locked() {
        assert_eq!(
            upgrade_code(IDENT),
            "{AEEE1525-B29B-5F4D-AA97-3E85DD62F6D2}"
        );
    }

    #[test]
    fn product_code_is_locked() {
        assert_eq!(
            product_code(IDENT, "0.1.0"),
            "{6CB00C44-06B1-575C-904F-14223F55E843}"
        );
    }

    #[test]
    fn package_uuid_is_locked() {
        assert_eq!(
            package_uuid(IDENT, "0.1.0").to_string(),
            "e86ffe24-f676-5bd2-a01c-fef7fd22f9c6"
        );
    }

    #[test]
    fn component_guid_is_locked() {
        assert_eq!(
            component_guid(IDENT, "hello.exe"),
            "{6DA375E3-B14D-5240-901F-1623A09F9F7A}"
        );
    }

    #[test]
    fn shortcut_component_guid_is_locked() {
        assert_eq!(
            shortcut_component_guid(IDENT),
            "{DD0EC1DA-82DF-50D1-BDEC-1F8547ADE512}"
        );
    }
}
