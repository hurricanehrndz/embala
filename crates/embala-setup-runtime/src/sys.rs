//! Thin Win32 FFI for the few calls winsafe 0.0.28 does not cover (research-
//! confirmed): `RtlGetVersion`, `SetConsoleCtrlHandler`, an elevated
//! `ShellExecuteExW` that returns a waitable child handle, and a WinHTTP GET.
//!
//! Everything here is `#[cfg(windows)]` and kept deliberately small; the higher
//! layers (`api`, `install`, `uninstall`) prefer winsafe. Elevation *detection*
//! and the `WM_SETTINGCHANGE` broadcast use winsafe and live where they are used.

use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, FALSE, GetLastError, TRUE};
use windows_sys::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WinHttpCloseHandle, WinHttpConnect,
    WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable, WinHttpReadData,
    WinHttpReceiveResponse, WinHttpSendRequest,
};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Set by the console control handler; polled by the Lua instruction hook to
/// abort a running install (spec R10). A process-global `AtomicBool` because
/// `SetConsoleCtrlHandler` takes a bare `extern "system"` fn (no capture).
pub static CANCEL: AtomicBool = AtomicBool::new(false);

/// True once Ctrl+C / Ctrl+Break has been seen.
pub fn is_cancelled() -> bool {
    CANCEL.load(Ordering::Relaxed)
}

/// Request cancellation (the wizard's Cancel/[X] during the progress phase, spec
/// R15). Same flag the console handler sets, so the Lua instruction hook aborts
/// the script and rollback runs.
pub fn request_cancel() {
    CANCEL.store(true, Ordering::Relaxed);
}

unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> i32 {
    if ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT {
        CANCEL.store(true, Ordering::Relaxed);
        // Return TRUE = "handled": suppress the default terminate-now behavior so
        // the hook can unwind the script and the engine can roll back cleanly.
        TRUE
    } else {
        FALSE
    }
}

/// Register the Ctrl+C/Ctrl+Break handler. Best-effort; a failure just means the
/// default (immediate terminate) applies, which is still safe (no partial ARP is
/// left because rollback only runs on a caught cancel — an OS kill leaves the
/// install log for the uninstaller).
pub fn install_ctrl_handler() {
    unsafe {
        SetConsoleCtrlHandler(Some(ctrl_handler), TRUE);
    }
}

/// `os_version()` → `(major, minor, build)` via `RtlGetVersion` (the only API
/// that reports the true version without a manifest compatibility shim).
pub fn os_version() -> (u32, u32, u32) {
    let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    // RtlGetVersion returns STATUS_SUCCESS (0) and never realistically fails.
    unsafe { RtlGetVersion(&mut info) };
    (info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber)
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Relaunch this exe elevated with `params` (spec R13), wait for it, and return
/// its exit code. Uses `ShellExecuteExW` with the `runas` verb — the standard
/// UAC-consent path — and `SEE_MASK_NOCLOSEPROCESS` so we get a handle to wait
/// on (winsafe's wrapper drops it, hence the raw call). A declined UAC prompt
/// surfaces as `ERROR_CANCELLED`.
pub fn relaunch_elevated(exe: &Path, params: &str) -> io::Result<u32> {
    let verb = wide("runas");
    let file = wide_path(exe);
    let parameters = wide(params);

    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = SW_SHOWNORMAL;

    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == FALSE {
        let err = unsafe { GetLastError() };
        if err == ERROR_CANCELLED {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "elevation declined (UAC cancelled)",
            ));
        }
        return Err(io::Error::from_raw_os_error(err as i32));
    }

    if info.hProcess.is_null() {
        return Err(io::Error::other(
            "elevated launch returned no process handle",
        ));
    }
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code: u32 = 0;
        let got = GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        if got == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(code)
    }
}

/// Fetch `url` over WinHTTP using the system TLS stack (no rustls/openssl). Only
/// `http`/`https` are supported. Returns the response body bytes; the caller
/// verifies the sha256 and writes the file (`embala.download`).
pub fn http_get(url: &str) -> io::Result<Vec<u8>> {
    let (secure, host, port, path) = parse_url(url)?;

    // RAII-ish: close each HINTERNET on the way out. Handles are *mut c_void.
    struct Handle(*mut core::ffi::c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    let agent = wide("embala-setup");
    let session = Handle(unsafe {
        WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    });
    if session.0.is_null() {
        return Err(io::Error::last_os_error());
    }

    let host_w = wide(&host);
    let connect = Handle(unsafe { WinHttpConnect(session.0, host_w.as_ptr(), port, 0) });
    if connect.0.is_null() {
        return Err(io::Error::last_os_error());
    }

    let verb = wide("GET");
    let path_w = wide(&path);
    let flags = if secure { WINHTTP_FLAG_SECURE } else { 0 };
    let request = Handle(unsafe {
        WinHttpOpenRequest(
            connect.0,
            verb.as_ptr(),
            path_w.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            flags,
        )
    });
    if request.0.is_null() {
        return Err(io::Error::last_os_error());
    }

    let sent =
        unsafe { WinHttpSendRequest(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) };
    if sent == FALSE {
        return Err(io::Error::last_os_error());
    }
    if unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) } == FALSE {
        return Err(io::Error::last_os_error());
    }

    let mut body = Vec::new();
    loop {
        let mut available: u32 = 0;
        if unsafe { WinHttpQueryDataAvailable(request.0, &mut available) } == FALSE {
            return Err(io::Error::last_os_error());
        }
        if available == 0 {
            break;
        }
        let mut chunk = vec![0u8; available as usize];
        let mut read: u32 = 0;
        if unsafe {
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr() as *mut core::ffi::c_void,
                available,
                &mut read,
            )
        } == FALSE
        {
            return Err(io::Error::last_os_error());
        }
        chunk.truncate(read as usize);
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Minimal `http(s)://host[:port]/path` split. Returns `(secure, host, port,
/// path)`; the port defaults to 443/80. Enough for the controlled URLs
/// `embala.download` is handed (mirrors nupkg's download style).
fn parse_url(url: &str) -> io::Result<(bool, String, u16, String)> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("download: only http(s) URLs are supported: {url}"),
        ));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "download: bad port"))?,
        ),
        None => (authority.to_string(), if secure { 443 } else { 80 }),
    };
    Ok((secure, host, port, path.to_string()))
}
