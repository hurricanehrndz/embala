//! Invoke a user-supplied signing command on a build artifact (spec R6).
//!
//! embala never implements a signature format — it only shells out. This module
//! is a dumb invoker: no retries, no output parsing. `$f` in the command is
//! replaced with the artifact's shell-quoted absolute path and the command runs
//! via the host shell with inherited stdio so the tool's output is visible.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Substitute every `$f` with `artifact`'s absolute (shell-quoted) path, run the
/// command via the host shell, and fail the build on a nonzero exit.
pub fn run(command: &str, artifact: &Path) -> Result<()> {
    let abs = std::path::absolute(artifact)
        .with_context(|| format!("resolving {}", artifact.display()))?;
    let cmd = command.replace("$f", &quote(&abs.to_string_lossy()));

    let status = if cfg!(windows) {
        Command::new("cmd").args(["/C", &cmd]).status()
    } else {
        Command::new("sh").args(["-c", &cmd]).status()
    }
    .with_context(|| format!("spawning sign command: {cmd}"))?;

    if !status.success() {
        let code = status
            .code()
            .map_or_else(|| "signal".to_string(), |c| c.to_string());
        bail!(
            "sign command failed for {} (exit {code}): {command}",
            artifact.display()
        );
    }
    println!("sign: {}", artifact.display());
    Ok(())
}

/// Single-quote wrapping so paths with spaces (or other shell metacharacters)
/// reach the tool intact; embedded quotes use the `'\''` idiom.
#[cfg(unix)]
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(windows)]
fn quote(s: &str) -> String {
    format!("\"{s}\"")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("embala-sign-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn substitutes_f_with_quoted_path_including_spaces() {
        let artifact = scratch("subst").join("with space.txt");
        std::fs::write(&artifact, b"x").unwrap();
        // The shell appending to $f only works if the space-bearing path was
        // quoted correctly during substitution.
        run("printf M >> $f", &artifact).unwrap();
        assert_eq!(std::fs::read(&artifact).unwrap(), b"xM");
    }

    #[test]
    fn noop_command_succeeds() {
        run("true", Path::new("/nonexistent/artifact")).unwrap();
    }

    #[test]
    fn failing_command_surfaces_an_error() {
        let err = run("false", Path::new("/nonexistent/artifact"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("sign command failed"), "{err}");
    }
}
