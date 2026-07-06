//! Windows entry path (Phase 3): parse the CLI, then dispatch by overlay
//! presence (spec R9). A full `setup.exe` carries the PE overlay → **install
//! mode** (run `install.lua`). The copied `uninstall.exe` is the bare stub prefix
//! with no overlay → **uninstall mode** (reverse the install). A truly bare stub
//! (no overlay, no sibling `install.log`) keeps the Phase-2 "no overlay" report.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::ExitCode;

use winsafe::prelude::*;
use winsafe::{self as w, HWND, PidParent, co};

use embala_setup_overlay::{Section, TRAILER_LEN, Trailer, TrailerError};

pub fn run() -> ExitCode {
    // Attach to the launching console so `/S` reports to the shell; a
    // double-clicked run has no parent console and falls back to a message box
    // (never in `/S` — see `report`).
    let attached = w::AttachConsole(PidParent::Parent).is_ok();

    let args = match crate::cli::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            report(&format!("setup: {e}"), attached, false);
            return ExitCode::FAILURE;
        }
    };

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            report(
                &format!("setup: cannot locate own exe: {e}"),
                attached,
                args.silent,
            );
            return ExitCode::FAILURE;
        }
    };

    match read_trailer(&exe) {
        Ok(Some(trailer)) => crate::install::run(&exe, &trailer, &args, attached),
        Ok(None) => crate::uninstall::run(&exe, &args, attached),
        Err(e) => {
            report(&format!("overlay: read failed: {e}"), attached, args.silent);
            ExitCode::FAILURE
        }
    }
}

/// Read the last [`TRAILER_LEN`] bytes of `exe` and parse them. `Ok(None)` = no
/// overlay (bare stub / uninstall.exe); `Err` is I/O or a corrupt trailer.
pub fn read_trailer(exe: &Path) -> Result<Option<Trailer>, String> {
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
        Err(TrailerError::BadMagic | TrailerError::TooShort) => Ok(None),
        Err(e) => Err(format!("{e:?}")),
    }
}

/// Read one overlay section out of `exe`.
pub fn read_section(exe: &Path, section: Section) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(exe)?;
    file.seek(SeekFrom::Start(section.offset))?;
    let mut buf = vec![0u8; section.len as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read the stub-prefix bytes `[0..stub.len)` — the engine with no overlay, which
/// the installer copies out as `uninstall.exe` (spec R14).
pub fn read_stub_prefix(exe: &Path, trailer: &Trailer) -> std::io::Result<Vec<u8>> {
    read_section(
        exe,
        Section {
            offset: 0,
            len: trailer.stub.len,
        },
    )
}

/// Report a result: console line when attached (silent mode), message box
/// otherwise — but **never** a window under `/S` (spec R16).
pub fn report(msg: &str, attached: bool, silent: bool) {
    if attached {
        println!("{msg}");
    } else if !silent {
        let _ = HWND::NULL.MessageBox(msg, "embala setup", co::MB::OK | co::MB::ICONINFORMATION);
    }
}
