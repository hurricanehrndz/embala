//! Windows entry path (Phase 1): attach to the parent console, prove the mlua
//! host links, then locate + report the PE overlay trailer.

use std::io::{Read, Seek, SeekFrom};
use std::process::ExitCode;

use winsafe::prelude::*;
use winsafe::{self as w, HWND, PidParent, co};

use crate::trailer::{TRAILER_LEN, Trailer, TrailerError};

pub fn run() -> ExitCode {
    // Attach to the launching console so a future `/S` run reports to the shell;
    // when double-clicked there is no parent console and this simply fails,
    // leaving us in GUI (message-box) mode.
    let attached = w::AttachConsole(PidParent::Parent).is_ok();

    let lua = lua_smoke();
    let overlay = match read_trailer() {
        Ok(Some(t)) => describe(&t),
        Ok(None) => "overlay: none (bare stub)".to_string(),
        Err(e) => format!("overlay: read failed: {e}"),
    };

    report(&format!("{lua}\n\n{overlay}"), attached);
    ExitCode::SUCCESS
}

/// Prove the vendored Lua 5.4 C actually linked by evaluating a trivial script.
fn lua_smoke() -> String {
    let lua = mlua::Lua::new();
    match lua.load("return 1 + 1").eval::<i64>() {
        Ok(n) => format!("mlua/lua54: 1+1={n}"),
        Err(e) => format!("mlua/lua54: error: {e}"),
    }
}

/// Read the last [`TRAILER_LEN`] bytes of our own exe and parse them.
///
/// `Ok(None)` means "no overlay" (bare stub); `Err` is an I/O failure or a
/// magic-present-but-malformed trailer.
fn read_trailer() -> Result<Option<Trailer>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut file = std::fs::File::open(&exe).map_err(|e| e.to_string())?;
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

/// Render the section table for display.
fn describe(t: &Trailer) -> String {
    let s = |label: &str, sec: crate::trailer::Section| {
        format!("  {label:<14} offset={:<12} len={}", sec.offset, sec.len)
    };
    format!(
        "overlay v{} section table:\n{}\n{}\n{}\n{}\n{}",
        t.version,
        s("stub", t.stub),
        s("payload.zip", t.payload_zip),
        s("install.lua", t.install_lua),
        s("uninstall.lua", t.uninstall_lua),
        s("manifest", t.manifest),
    )
}

/// Console line when attached (silent mode), message box otherwise.
fn report(msg: &str, attached: bool) {
    if attached {
        println!("{msg}");
    } else {
        let _ = HWND::NULL.MessageBox(msg, "embala setup", co::MB::OK | co::MB::ICONINFORMATION);
    }
}
