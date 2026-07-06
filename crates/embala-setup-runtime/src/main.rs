//! embala `setup.exe` runtime stub.
//!
//! Cross-compiled once via `just stubs` (cargo-zigbuild) to both windows arches
//! and committed into `embala-setup`, which `include_bytes!`-embeds it. Phase 2
//! locates the PE overlay trailer (shared `embala-setup-overlay`) and, when an
//! overlay is present, self-extracts `payload.zip` to a target dir; a bare stub
//! still reports "no overlay". The wizard and Lua host arrive later.
//!
//! Built for the `windows` subsystem (no console window on double-click) but it
//! attaches to the parent console at startup so the future `/S` silent mode can
//! write to the launching shell.
#![cfg_attr(windows, windows_subsystem = "windows")]

// Pure-logic modules (no Win32): compiled on Windows (used by the engine) and in
// `cfg(test)` (their unit tests run on the Linux host, satisfying the plan's
// host-side coverage for the log format, mode resolution, and CLI parsing). They
// are absent from a host non-test build, so no dead-code lint fires there.
#[cfg(any(windows, test))]
mod cli;
#[cfg(any(windows, test))]
mod log;
#[cfg(any(windows, test))]
mod mode;

#[cfg(windows)]
mod api;
#[cfg(windows)]
mod app;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod install;
#[cfg(windows)]
mod sys;
#[cfg(windows)]
mod uninstall;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    app::run()
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    // A Windows-only self-extractor: cross-compiled via `just stubs`, never run
    // natively. Building on this host still compiles the vendored Lua C and
    // links it, which is why mlua is a normal (not Windows-gated) dependency.
    eprintln!("embala-setup-runtime is a Windows binary; build it with `just stubs`.");
    std::process::ExitCode::FAILURE
}
