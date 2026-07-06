//! Windows entry path (Phase 2): attach to the parent console, locate the PE
//! overlay trailer, and — when an overlay is present — self-extract the payload
//! zip to a target directory. A bare stub (no overlay) still reports "no
//! overlay" and proves the mlua host links. The Lua host and wizard arrive
//! later; extraction here is native.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use winsafe::prelude::*;
use winsafe::{self as w, HWND, PidParent, co};

use embala_setup_overlay::{TRAILER_LEN, Trailer, TrailerError};

pub fn run() -> ExitCode {
    // Attach to the launching console so a `/S`-style run reports to the shell;
    // when double-clicked there is no parent console and this simply fails,
    // leaving us in GUI (message-box) mode.
    let attached = w::AttachConsole(PidParent::Parent).is_ok();

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            report(&format!("setup: cannot locate own exe: {e}"), attached);
            return ExitCode::FAILURE;
        }
    };

    let (msg, code) = match read_trailer(&exe) {
        Ok(Some(t)) => {
            let target = target_dir(&exe);
            match extract(&exe, &t, &target) {
                Ok(count) => (
                    format!("extracted {count} file(s) to:\n{}", target.display()),
                    ExitCode::SUCCESS,
                ),
                Err(e) => (format!("extraction failed: {e}"), ExitCode::FAILURE),
            }
        }
        // A bare stub (an un-appended build or the future copied uninstall.exe):
        // report no overlay and confirm the vendored Lua host still links.
        Ok(None) => (
            format!("{}\n\noverlay: none (bare stub)", lua_smoke()),
            ExitCode::SUCCESS,
        ),
        Err(e) => (format!("overlay: read failed: {e}"), ExitCode::FAILURE),
    };

    report(&msg, attached);
    code
}

/// Prove the vendored Lua 5.4 C actually linked by evaluating a trivial script.
fn lua_smoke() -> String {
    let lua = mlua::Lua::new();
    match lua.load("return 1 + 1").eval::<i64>() {
        Ok(n) => format!("mlua/lua54: 1+1={n}"),
        Err(e) => format!("mlua/lua54: error: {e}"),
    }
}

/// Resolve the extraction target directory.
///
/// NSIS convention is `/D=<dir>` as the last argument, where the directory is
/// the unquoted remainder of the raw command line. We simplify: take everything
/// after `/D=` *within that single argv token*, so a target path with spaces
/// must be quoted by the caller. Absent `/D=`, default to a deterministic temp
/// location `%TEMP%\embala-setup\<exe-stem>`.
fn target_dir(exe: &Path) -> PathBuf {
    if let Some(dir) = std::env::args().find_map(|a| a.strip_prefix("/D=").map(PathBuf::from)) {
        if !dir.as_os_str().is_empty() {
            return dir;
        }
    }
    let stem = exe.file_stem().unwrap_or_default();
    let mut base = std::env::temp_dir();
    base.push("embala-setup");
    base.push(stem);
    base
}

/// Read the last [`TRAILER_LEN`] bytes of `exe` and parse them.
///
/// `Ok(None)` means "no overlay" (bare stub); `Err` is an I/O failure or a
/// magic-present-but-malformed trailer.
fn read_trailer(exe: &Path) -> Result<Option<Trailer>, String> {
    let mut file = std::fs::File::open(exe).map_err(|e| e.to_string())?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    if len < TRAILER_LEN as u64 {
        return Ok(None);
    }
    file.seek(SeekFrom::End(-(TRAILER_LEN as i64)))
        .map_err(|e| e.to_string())?;
    let mut buf = [0u8; TRAILER_LEN];
    file.read_exact(&mut buf).map_err(|e| e.to_string())?;
    match Trailer::parse(&buf) {
        Ok(t) => Ok(Some(t)),
        // A stub with no appended overlay (or the copied uninstall.exe).
        Err(TrailerError::BadMagic | TrailerError::TooShort) => Ok(None),
        Err(e) => Err(format!("{e:?}")),
    }
}

/// Extract the `payload.zip` overlay section to `target`, returning the number
/// of files written. Parent dirs are created. `enclosed_name` rejects any entry
/// whose path escapes `target` (zip-slip defense).
fn extract(exe: &Path, trailer: &Trailer, target: &Path) -> Result<usize, String> {
    let mut file = std::fs::File::open(exe).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(trailer.payload_zip.offset))
        .map_err(|e| e.to_string())?;
    let mut zip_bytes = vec![0u8; trailer.payload_zip.len as usize];
    file.read_exact(&mut zip_bytes).map_err(|e| e.to_string())?;

    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).map_err(|e| e.to_string())?;
    let mut count = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        if entry.is_dir() {
            continue;
        }
        let rel = entry
            .enclosed_name()
            .ok_or_else(|| format!("payload entry {:?} escapes the target dir", entry.name()))?;
        let dest = target.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
        count += 1;
    }
    Ok(count)
}

/// Console line when attached (silent mode), message box otherwise.
fn report(msg: &str, attached: bool) {
    if attached {
        println!("{msg}");
    } else {
        let _ = HWND::NULL.MessageBox(msg, "embala setup", co::MB::OK | co::MB::ICONINFORMATION);
    }
}
