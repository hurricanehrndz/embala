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
use std::collections::BTreeSet;
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
use crate::mode::{self, ManifestMode, ResolvedMode};
use crate::wizard::confirm;

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

    // Read the sidecar (best-effort): it yields the declared uninstall options
    // and the *resolved* install mode (spec R5). A missing/unparseable sidecar
    // degrades to zero options and per-user (no elevation) — the confirm dialog
    // still shows (spec R3).
    let manifest = std::fs::read(dir.join("manifest.json"))
        .ok()
        .and_then(|b| Manifest::parse(&b).ok());
    let resolved = manifest
        .as_ref()
        .and_then(|m| ManifestMode::parse(&m.install_mode))
        .map(|mm| mode::resolve(mm, None))
        .unwrap_or(ResolvedMode::PerUser);
    let options = manifest
        .as_ref()
        .map(|m| m.uninstall_options.clone())
        .unwrap_or_default();
    let declared: BTreeSet<String> = options.iter().map(|o| o.id.clone()).collect();

    // Resolve the selected options (spec R4 precedence): `/options=` (filtered to
    // declared ids) > interactive confirm dialog > declared defaults.
    let selected: BTreeSet<String> = match &args.options {
        Some(ids) => ids
            .iter()
            .filter(|id| declared.contains(id.as_str()))
            .cloned()
            .collect(),
        None if !args.silent => {
            let dlg_options = options
                .iter()
                .map(|o| confirm::ConfirmOption {
                    id: o.id.clone(),
                    label: o.label.clone(),
                    default: o.default,
                })
                .collect::<Vec<_>>();
            let display_name = manifest
                .as_ref()
                .map(|m| m.package.display_name.clone())
                .unwrap_or_else(|| display_name_fallback(dir));
            match confirm::confirm(&display_name, &dir.to_string_lossy(), &dlg_options) {
                confirm::Outcome::Cancel => {
                    // Nothing has mutated (spec R3): report and exit nonzero.
                    report("Uninstall cancelled.", attached, true);
                    return ExitCode::FAILURE;
                }
                // The dialog only offers declared checkboxes, so its ids are
                // already a subset of `declared`.
                confirm::Outcome::Uninstall(sel) => sel.into_iter().collect(),
            }
        }
        None => options
            .iter()
            .filter(|o| o.default)
            .map(|o| o.id.clone())
            .collect(),
    };

    // Per-machine teardown needs elevation (spec R6): relaunch ourselves elevated
    // and silent, passing the already-resolved options so the child neither
    // re-prompts nor re-shows the dialog. Propagate the child's exit code.
    if resolved == ResolvedMode::PerMachine && !crate::install::is_elevated() {
        let joined = selected.iter().cloned().collect::<Vec<_>>().join(",");
        return match crate::sys::relaunch_elevated(exe, &format!("/S /options={joined}")) {
            Ok(code) => ExitCode::from(code as u8),
            Err(e) => {
                report(&format!("setup: {e}"), attached, args.silent);
                ExitCode::FAILURE
            }
        };
    }

    // Custom teardown first (best-effort), then the guaranteed log replay.
    if uninstall_lua.exists() {
        if let Err(e) = run_uninstall_script(dir, &uninstall_lua, &declared, &selected, attached) {
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

    // Remove the sidecars we can delete directly.
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(&uninstall_lua);
    let _ = std::fs::remove_file(dir.join("manifest.json"));

    report("Uninstall complete.", attached, args.silent);

    // Hand off the running uninstall.exe + the (now hopefully empty) install dir
    // to a detached "ping 1s then del" helper. This must run AFTER the final
    // report: on interactive runs that report is a blocking MessageBox that would
    // outlive the 1s delete delay, so the del would fire while the exe is still
    // running and locked and silently fail, leaving uninstall.exe + the dir behind.
    schedule_self_delete(exe, dir);
    ExitCode::SUCCESS
}

/// Run `uninstall.lua` in the same host, with context reconstructed from the
/// `manifest.json` sidecar written at install time. Without the sidecar the
/// script cannot get `embala.package`, so we skip it and rely on the log replay.
/// `declared`/`selected` are the resolved uninstall options, seeded so
/// `embala.ui.selected(id)` reads them (spec R4).
fn run_uninstall_script(
    dir: &Path,
    script_path: &Path,
    declared: &BTreeSet<String>,
    selected: &BTreeSet<String>,
    attached: bool,
) -> Result<(), String> {
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
    // Uninstall runs no wizard pages: lock the dir (no re-point) and pass no
    // component selection; the ui table is still registered for API parity.
    let engine = Rc::new(RefCell::new(Engine::new(
        manifest.clone(),
        resolved,
        manifest.arch.clone(),
        dir.to_path_buf(),
        dir.to_path_buf(),
        attached,
        true,
        None,
        // Uninstall never shows the wizard: non-interactive, and elevation is
        // already resolved by the time uninstall.exe runs.
        false,
        false,
    )));
    // Seed the resolved options so `ui.selected(id)` works at uninstall (spec R4).
    engine
        .borrow_mut()
        .seed_options(declared.clone(), selected.clone());
    host::run_script(&engine, &script)
}

/// Display name when the sidecar is missing/unparseable (spec R3 degrade): the
/// install dir's folder name, else a generic label.
fn display_name_fallback(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "this application".to_string())
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
