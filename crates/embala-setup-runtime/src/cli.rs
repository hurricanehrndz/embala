//! Command-line parsing (spec R16) — pure logic, host-testable.
//!
//! Recognized flags (NSIS-compatible spellings):
//! - `/S` — silent/headless: no window, console reporting only, auto-accept.
//! - `/D=<dir>` — install-directory override (the unquoted remainder of the
//!   token; a path with spaces must be quoted by the caller so it stays one
//!   argv token).
//! - `/mode=per-user|per-machine` — override the manifest's default mode.
//! - `/components=<ids>` — parsed and stored for Phase 5; nothing consumes it
//!   yet (no component UI in this phase).
//!
//! Unknown arguments are **ignored** (installer CLIs are conventionally lenient;
//! failing hard would break future/forward flags). A malformed `/mode=` value is
//! the one hard error — a silent misread there would install to the wrong hive.

use std::path::PathBuf;

use crate::mode::ResolvedMode;

/// Parsed installer arguments.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub silent: bool,
    pub dir: Option<PathBuf>,
    pub mode: Option<ResolvedMode>,
    /// `/components=` value, split on commas. Stored only; unused this phase.
    pub components: Vec<String>,
    /// `/options=` value (uninstall options, spec R4): `None` = flag absent,
    /// `Some([])` = given but empty. Split on commas like `/components=`. Parsed
    /// at install time too but only consumed by the uninstaller.
    pub options: Option<Vec<String>>,
}

/// Parse installer args from an argv iterator (excluding argv[0]).
pub fn parse<I, S>(args: I) -> Result<Args, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Args::default();
    for arg in args {
        let a = arg.as_ref();
        if a.eq_ignore_ascii_case("/S") {
            out.silent = true;
        } else if let Some(dir) = a.strip_prefix("/D=") {
            if !dir.is_empty() {
                out.dir = Some(PathBuf::from(dir));
            }
        } else if let Some(m) = a.strip_prefix("/mode=") {
            out.mode = Some(parse_mode(m)?);
        } else if let Some(ids) = a.strip_prefix("/components=") {
            out.components = ids
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        } else if let Some(ids) = a.strip_prefix("/options=") {
            // `Some(_)` even when empty: the flag's *presence* means "options were
            // resolved by the parent" (spec R4/R6), distinct from `None` = absent.
            out.options = Some(
                ids.split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
            );
        }
        // else: unknown arg, ignored by design.
    }
    Ok(out)
}

/// `/mode=` accepts only the two concrete modes (spec R13 CLI vocabulary);
/// `user-choice` is not a CLI value.
fn parse_mode(value: &str) -> Result<ResolvedMode, String> {
    match value {
        "per-user" => Ok(ResolvedMode::PerUser),
        "per-machine" => Ok(ResolvedMode::PerMachine),
        other => Err(format!(
            "unknown /mode={other:?} (expected per-user or per-machine)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_full_flag_set() {
        let args = parse([
            "/S",
            r"/D=C:\Program Files\Hello",
            "/mode=per-machine",
            "/components=core,docs",
            "/options=purge-data",
        ])
        .unwrap();
        assert!(args.silent);
        assert_eq!(args.dir, Some(PathBuf::from(r"C:\Program Files\Hello")));
        assert_eq!(args.mode, Some(ResolvedMode::PerMachine));
        assert_eq!(args.components, vec!["core", "docs"]);
        assert_eq!(args.options, Some(vec!["purge-data".to_string()]));
    }

    #[test]
    fn options_flag_absent_vs_empty() {
        // Why: `/S` uninstall must distinguish "no /options=" (use defaults) from
        // "/options=" given empty (parent resolved to no options) — spec R4/R6.
        assert_eq!(parse(["/S"]).unwrap().options, None);
        assert_eq!(parse(["/options="]).unwrap().options, Some(vec![]));
        assert_eq!(
            parse(["/options=a,b"]).unwrap().options,
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn silent_flag_is_case_insensitive() {
        // Why: callers type `/s` and `/S`; NSIS treats them the same, and a
        // missed silent flag would pop a window in CI/test-rig headless runs.
        assert!(parse(["/s"]).unwrap().silent);
    }

    #[test]
    fn unknown_args_are_ignored_but_bad_mode_errors() {
        // Why: lenient toward forward flags, strict on /mode — a misread mode
        // installs to the wrong hive/dir, which must fail loud (spec R12/R13).
        let args = parse(["/whatever", "extra"]).unwrap();
        assert_eq!(args, Args::default());
        assert!(parse(["/mode=bogus"]).is_err());
    }

    #[test]
    fn empty_dir_override_is_dropped() {
        assert_eq!(parse(["/D="]).unwrap().dir, None);
    }
}
