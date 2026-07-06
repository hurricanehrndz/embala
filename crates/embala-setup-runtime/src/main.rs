//! embala `setup.exe` runtime stub.
//!
//! Cross-compiled once via `just stubs` (cargo-zigbuild) to both windows arches
//! and committed into `embala-setup`, which `include_bytes!`-embeds it. Phase 1
//! only locates + validates the PE overlay trailer and reports the section
//! table (or "no overlay"); the wizard, extractor and Lua host arrive later.
//!
//! Built for the `windows` subsystem (no console window on double-click) but it
//! attaches to the parent console at startup so the future `/S` silent mode can
//! write to the launching shell.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod trailer;

#[cfg(windows)]
mod app;

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
