//! Uninstall + bare-stub path (spec R9/R14).
//!
//! Reached when the running exe has **no overlay**. Three cases by what sits next
//! to it:
//! - `install.log` and/or `uninstall.lua` present → **uninstall**: run the
//!   optional `uninstall.lua` for custom teardown, then *always* replay the
//!   install log LIFO through the shared reversal engine (which removes the ARP
//!   entry, shortcuts, registry/env writes, files and empty dirs — idempotently),
//!   and finally self-delete.
//! - neither present → a truly bare stub → the Phase-2 "no overlay" report.
//!
//! The uninstaller runs `uninstall.lua` *and* replays the log (not either/or):
//! the log replay is the engine's guarantee that every logged mutation is
//! reversed and the ARP entry always removed, while `uninstall.lua` is an
//! additive hook for teardown the log cannot express. Both are idempotent.

use std::cell::RefCell;
use std::os::windows::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::rc::Rc;

use embala_setup_overlay::Manifest;

use crate::api::{self, Engine};
use crate::app::report;
use crate::cli::Args;
use crate::host;
use crate::log;
use crate::mode::{self, ManifestMode};

/// `CREATE_NO_WINDOW`: the helper runs a hidden console of its own, so it
/// survives this process exiting and never flashes a window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn run(exe: &Path, args: &Args, attached: bool) -> ExitCode {
    let dir = exe.parent().unwrap_or(Path::new("."));
    let log_path = dir.join("install.log");
    let uninstall_lua = dir.join("uninstall.lua");

    if !log_path.exists() && !uninstall_lua.exists() {
        // Truly bare stub: no install to reverse.
        report(
            &format!("{}\n\noverlay: none (bare stub)", lua_smoke()),
            attached,
            args.silent,
        );
        return ExitCode::SUCCESS;
    }

    // Custom teardown first (best-effort), then the guaranteed log replay.
    if uninstall_lua.exists() {
        if let Err(e) = run_uninstall_script(dir, &uninstall_lua, attached) {
            report(
                &format!("setup: uninstall.lua error (continuing): {e}"),
                attached,
                args.silent,
            );
        }
    }

    if let Ok(text) = std::fs::read_to_string(&log_path) {
        match log::decode(&text) {
            Ok(records) => api::reverse_all(&records),
            Err(e) => report(
                &format!("setup: install.log unreadable (skipping replay): {e}"),
                attached,
                args.silent,
            ),
        }
    }

    // Remove the sidecars we can delete directly, then hand off the running
    // uninstall.exe + the (now hopefully empty) install dir to a detached helper.
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(&uninstall_lua);
    let _ = std::fs::remove_file(dir.join("manifest.json"));
    schedule_self_delete(exe, dir);

    report("Uninstall complete.", attached, args.silent);
    ExitCode::SUCCESS
}

/// Run `uninstall.lua` in the same host, with context reconstructed from the
/// `manifest.json` sidecar written at install time. Without the sidecar the
/// script cannot get `embala.package`, so we skip it and rely on the log replay.
fn run_uninstall_script(dir: &Path, script_path: &Path, attached: bool) -> Result<(), String> {
    let manifest_path = dir.join("manifest.json");
    let manifest = match std::fs::read(&manifest_path) {
        Ok(bytes) => Manifest::parse(&bytes).map_err(|e| e.to_string())?,
        Err(_) => return Ok(()), // no context available; log replay still runs
    };
    let manifest_mode =
        ManifestMode::parse(&manifest.install_mode).unwrap_or(ManifestMode::PerUser);
    let resolved = mode::resolve(manifest_mode, None);

    let script = std::fs::read(script_path).map_err(|e| e.to_string())?;
    // No payload at uninstall time; install_dir == the uninstaller's own dir.
    let engine = Rc::new(RefCell::new(Engine::new(
        manifest.clone(),
        resolved,
        manifest.arch.clone(),
        dir.to_path_buf(),
        dir.to_path_buf(),
        attached,
    )));
    host::run_script(&engine, &script)
}

/// Self-delete: a running exe cannot delete itself, so spawn a detached
/// `cmd /c` that waits ~1s (ping) then removes `uninstall.exe` and the install
/// dir if empty. Best-effort — a failure just leaves an empty dir behind.
fn schedule_self_delete(exe: &Path, dir: &Path) {
    // `raw_arg`, not `arg`: std quotes arguments MSVCRT-style (`\"`), which
    // cmd.exe does not parse — the paths would reach `del`/`rmdir` mangled.
    let _ = Command::new("cmd")
        .arg("/c")
        .raw_arg(format!(
            "ping 127.0.0.1 -n 2 >nul & del /q \"{}\" & rmdir \"{}\"",
            exe.display(),
            dir.display()
        ))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// Prove the vendored Lua still links (kept from Phase 2 for the bare-stub path).
fn lua_smoke() -> String {
    let lua = mlua::Lua::new();
    match lua.load("return 1 + 1").eval::<i64>() {
        Ok(n) => format!("mlua/lua54: 1+1={n}"),
        Err(e) => format!("mlua/lua54: error: {e}"),
    }
}
