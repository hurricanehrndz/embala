//! Install-mode resolution + default directory logic (spec R13) — pure logic.
//!
//! The manifest carries a *default* mode (`per-user` | `per-machine` |
//! `user-choice`); a `/mode=` CLI flag overrides it. This module resolves the
//! two into the binary [`ResolvedMode`] the engine acts on and computes the
//! default install directory for each. It takes the environment values
//! (`%LOCALAPPDATA%`, `%ProgramFiles%`) as *parameters* so it is fully
//! host-testable; the Win32 caller reads the env and passes them in.

use std::path::PathBuf;

use crate::log::Hive;

/// The default mode as declared in the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestMode {
    PerUser,
    PerMachine,
    UserChoice,
}

impl ManifestMode {
    /// Parse the manifest's `install_mode` string.
    pub fn parse(s: &str) -> Option<ManifestMode> {
        match s {
            "per-user" => Some(ManifestMode::PerUser),
            "per-machine" => Some(ManifestMode::PerMachine),
            "user-choice" => Some(ManifestMode::UserChoice),
            _ => None,
        }
    }
}

/// The mode the engine actually installs in: no `user-choice` here — it has been
/// resolved to one of the two concrete modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedMode {
    PerUser,
    PerMachine,
}

impl ResolvedMode {
    /// The registry hive ARP + registry writes land in for this mode.
    pub fn hive(self) -> Hive {
        match self {
            ResolvedMode::PerUser => Hive::Hkcu,
            ResolvedMode::PerMachine => Hive::Hklm,
        }
    }

    /// The `embala.mode` string exposed to Lua.
    pub fn as_str(self) -> &'static str {
        match self {
            ResolvedMode::PerUser => "per-user",
            ResolvedMode::PerMachine => "per-machine",
        }
    }
}

/// Resolve the effective install mode (spec R13).
///
/// A `/mode=` flag always wins. Otherwise the manifest default applies, except
/// `user-choice` — which in this headless phase resolves to **per-user** (the
/// safe, no-elevation default). Phase 5 replaces this with the mode-selection
/// wizard page; the flag path is unchanged then.
pub fn resolve(manifest: ManifestMode, flag: Option<ResolvedMode>) -> ResolvedMode {
    if let Some(m) = flag {
        return m;
    }
    match manifest {
        ManifestMode::PerMachine => ResolvedMode::PerMachine,
        // Phase 5 TODO: user-choice shows the mode page; headless picks per-user.
        ManifestMode::PerUser | ManifestMode::UserChoice => ResolvedMode::PerUser,
    }
}

/// Default install directory for the resolved mode (spec R13):
/// per-user → `%LOCALAPPDATA%\Programs\<name>`, per-machine →
/// `%ProgramFiles%\<name>`. `local_app_data`/`program_files` are the expanded
/// env values (passed in so this is host-testable).
pub fn default_install_dir(
    mode: ResolvedMode,
    package_name: &str,
    local_app_data: &str,
    program_files: &str,
) -> PathBuf {
    match mode {
        ResolvedMode::PerUser => PathBuf::from(local_app_data)
            .join("Programs")
            .join(package_name),
        ResolvedMode::PerMachine => PathBuf::from(program_files).join(package_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_overrides_manifest_default() {
        // Why: `/mode=` is the operator's explicit choice and must win over the
        // packaged default in every combination.
        assert_eq!(
            resolve(ManifestMode::PerUser, Some(ResolvedMode::PerMachine)),
            ResolvedMode::PerMachine
        );
        assert_eq!(
            resolve(ManifestMode::PerMachine, Some(ResolvedMode::PerUser)),
            ResolvedMode::PerUser
        );
    }

    #[test]
    fn user_choice_without_flag_is_per_user_in_headless_phase() {
        // Why: with no wizard yet (Phase 5) and no flag, user-choice must pick
        // the non-elevating default rather than silently requiring UAC.
        assert_eq!(
            resolve(ManifestMode::UserChoice, None),
            ResolvedMode::PerUser
        );
        assert_eq!(resolve(ManifestMode::PerUser, None), ResolvedMode::PerUser);
        assert_eq!(
            resolve(ManifestMode::PerMachine, None),
            ResolvedMode::PerMachine
        );
    }

    #[test]
    fn default_dirs_follow_mode() {
        // Why: per-user must never land in Program Files (needs elevation) and
        // per-machine must not hide in LOCALAPPDATA; the hive pairs with the dir.
        // Expected paths are built with `join` so the assertion is
        // separator-agnostic (host `/` vs the real target's `\`); the logic is
        // the same either way.
        const LOCAL: &str = r"C:\Users\me\AppData\Local";
        const PROGRAMS: &str = r"C:\Program Files";

        let user = default_install_dir(ResolvedMode::PerUser, "hello", LOCAL, PROGRAMS);
        assert_eq!(user, PathBuf::from(LOCAL).join("Programs").join("hello"));
        assert_eq!(ResolvedMode::PerUser.hive(), Hive::Hkcu);

        let machine = default_install_dir(ResolvedMode::PerMachine, "hello", LOCAL, PROGRAMS);
        assert_eq!(machine, PathBuf::from(PROGRAMS).join("hello"));
        assert_eq!(ResolvedMode::PerMachine.hive(), Hive::Hklm);
    }

    #[test]
    fn manifest_mode_parses_the_spec_vocabulary() {
        assert_eq!(ManifestMode::parse("per-user"), Some(ManifestMode::PerUser));
        assert_eq!(
            ManifestMode::parse("per-machine"),
            Some(ManifestMode::PerMachine)
        );
        assert_eq!(
            ManifestMode::parse("user-choice"),
            Some(ManifestMode::UserChoice)
        );
        assert_eq!(ManifestMode::parse("bogus"), None);
    }
}
