//! Install flow (spec R9/R13/R14): resolve the mode (relaunching elevated for
//! per-machine), extract the payload to a staging dir, run `install.lua` over the
//! `embala.*` host, then write the uninstaller. On any script error or cancel the
//! in-memory install log is replayed LIFO so no partial install survives.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use winsafe::{self as w, co};

use embala_setup_overlay::{Manifest, Trailer};

use crate::api::{self, Engine};
use crate::app::{read_section, read_stub_prefix, report};
use crate::cli::Args;
use crate::host;
use crate::mode::{self, ManifestMode, ResolvedMode};

pub fn run(exe: &Path, trailer: &Trailer, args: &Args, attached: bool) -> ExitCode {
    // Register the Ctrl+C handler so a cancel mid-install triggers rollback.
    crate::sys::install_ctrl_handler();

    let manifest = match read_section(exe, trailer.manifest)
        .map_err(|e| e.to_string())
        .and_then(|b| Manifest::parse(&b).map_err(|e| e.to_string()))
    {
        Ok(m) => m,
        Err(e) => {
            report(&format!("setup: bad manifest: {e}"), attached, args.silent);
            return ExitCode::FAILURE;
        }
    };

    let manifest_mode =
        ManifestMode::parse(&manifest.install_mode).unwrap_or(ManifestMode::PerUser);
    let resolved = mode::resolve(manifest_mode, args.mode);

    // Per-machine needs elevation: relaunch via UAC and propagate the child's
    // exit code (spec R13). The relaunch appends /mode=per-machine so the elevated
    // child resolves the same mode without re-prompting.
    if resolved == ResolvedMode::PerMachine && !is_elevated() {
        return match elevate(exe, args) {
            Ok(code) => ExitCode::from(code as u8),
            Err(e) => {
                report(&format!("setup: {e}"), attached, args.silent);
                ExitCode::FAILURE
            }
        };
    }

    match do_install(exe, trailer, args, &manifest, resolved, attached) {
        Ok(msg) => {
            report(&msg, attached, args.silent);
            ExitCode::SUCCESS
        }
        Err(e) => {
            report(
                &format!("setup: install failed and was rolled back: {e}"),
                attached,
                args.silent,
            );
            ExitCode::FAILURE
        }
    }
}

fn do_install(
    exe: &Path,
    trailer: &Trailer,
    args: &Args,
    manifest: &Manifest,
    resolved: ResolvedMode,
    attached: bool,
) -> Result<String, String> {
    let install_dir = resolve_install_dir(args, resolved, &manifest.package.name)?;
    std::fs::create_dir_all(&install_dir).map_err(|e| e.to_string())?;

    // Fresh staging dir for the read-only payload (spec R11 `payload_dir`).
    let payload_dir = staging_dir();
    let _ = std::fs::remove_dir_all(&payload_dir);
    std::fs::create_dir_all(&payload_dir).map_err(|e| e.to_string())?;
    let payload = read_section(exe, trailer.payload_zip).map_err(|e| e.to_string())?;
    extract_zip(&payload, &payload_dir).map_err(|e| e.to_string())?;

    let install_lua = read_section(exe, trailer.install_lua).map_err(|e| e.to_string())?;

    // `/D=` locks the directory (the directory page cannot re-point it, spec
    // R16); `/components=` (when given) picks the component set, else defaults.
    let dir_locked = args.dir.is_some();
    let cli_components = (!args.components.is_empty()).then(|| args.components.clone());
    let engine = Rc::new(RefCell::new(Engine::new(
        manifest.clone(),
        resolved,
        manifest.arch.clone(),
        install_dir.clone(),
        payload_dir.clone(),
        attached,
        dir_locked,
        cli_components,
    )));

    let result = host::run_script(&engine, &install_lua);
    match result {
        Ok(()) => {
            write_uninstaller(exe, trailer, &install_dir, manifest, &engine.borrow())?;
            let _ = std::fs::remove_dir_all(&payload_dir);
            Ok(format!(
                "Installed {} to {}",
                manifest.package.display_name,
                install_dir.display()
            ))
        }
        Err(e) => {
            // Reverse everything logged so far, LIFO (spec R14 engine), then drop
            // the staging dir + any flushed log + the now-empty install dir.
            let records = engine.borrow().records().to_vec();
            api::reverse_all(&records);
            let _ = std::fs::remove_file(install_dir.join("install.log"));
            let _ = std::fs::remove_dir_all(&payload_dir);
            let _ = std::fs::remove_dir(&install_dir);
            Err(e)
        }
    }
}

/// Write `uninstall.exe` (the stub prefix, no overlay), the optional
/// `uninstall.lua`, the `manifest.json` sidecar (so `uninstall.lua` can rebuild
/// its `embala.package` context), and flush `install.log` — all into the install
/// dir (spec R14).
fn write_uninstaller(
    exe: &Path,
    trailer: &Trailer,
    install_dir: &Path,
    manifest: &Manifest,
    engine: &Engine,
) -> Result<(), String> {
    let prefix = read_stub_prefix(exe, trailer).map_err(|e| e.to_string())?;
    std::fs::write(install_dir.join("uninstall.exe"), &prefix).map_err(|e| e.to_string())?;
    if trailer.uninstall_lua.len > 0 {
        let script = read_section(exe, trailer.uninstall_lua).map_err(|e| e.to_string())?;
        std::fs::write(install_dir.join("uninstall.lua"), &script).map_err(|e| e.to_string())?;
    }
    std::fs::write(install_dir.join("manifest.json"), manifest.to_bytes())
        .map_err(|e| e.to_string())?;
    engine.flush_log().map_err(|e| e.to_string())?;
    Ok(())
}

fn resolve_install_dir(args: &Args, resolved: ResolvedMode, name: &str) -> Result<PathBuf, String> {
    if let Some(dir) = &args.dir {
        return Ok(dir.clone());
    }
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let program_files = std::env::var("ProgramFiles").unwrap_or_default();
    Ok(mode::default_install_dir(
        resolved,
        name,
        &local,
        &program_files,
    ))
}

fn staging_dir() -> PathBuf {
    std::env::temp_dir().join(format!("embala-stage-{}", std::process::id()))
}

/// Extract a payload zip byte-slice into `target`. `enclosed_name` rejects any
/// entry escaping `target` (zip-slip defense, mirrors the Phase-2 extractor).
fn extract_zip(bytes: &[u8], target: &Path) -> std::io::Result<()> {
    let reader = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader).map_err(std::io::Error::other)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(std::io::Error::other)?;
        if entry.is_dir() {
            continue;
        }
        let rel = entry.enclosed_name().ok_or_else(|| {
            std::io::Error::other(format!("payload entry {:?} escapes", entry.name()))
        })?;
        let dest = target.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&dest)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

/// Detect process elevation via the access token (spec R13). A failure to query
/// is treated as *not* elevated, so per-machine still relaunches.
fn is_elevated() -> bool {
    let Ok(token) = w::HPROCESS::GetCurrentProcess().OpenProcessToken(co::TOKEN::QUERY) else {
        return false;
    };
    matches!(
        token.GetTokenInformation(co::TOKEN_INFORMATION_CLASS::Elevation),
        Ok(w::TokenInfo::Elevation(e)) if e.TokenIsElevated()
    )
}

fn elevate(exe: &Path, args: &Args) -> Result<u32, String> {
    crate::sys::relaunch_elevated(exe, &relaunch_params(args)).map_err(|e| e.to_string())
}

/// Reconstruct the child command line, forcing `/mode=per-machine` so the
/// elevated child does not re-prompt. Tokens with spaces are quoted so
/// `CommandLineToArgvW` re-splits them as single args.
fn relaunch_params(args: &Args) -> String {
    let mut parts: Vec<String> = Vec::new();
    if args.silent {
        parts.push("/S".to_string());
    }
    if let Some(dir) = &args.dir {
        parts.push(format!("/D={}", dir.display()));
    }
    parts.push("/mode=per-machine".to_string());
    parts
        .into_iter()
        .map(|p| {
            if p.contains(' ') {
                format!("\"{p}\"")
            } else {
                p
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
