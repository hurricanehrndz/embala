//! Windows `setup.exe` builder (prebuilt stub + PE overlay).
//!
//! Honors embala's byte-writer invariant: `embala build` never compiles. The
//! Win32 runtime stub (`embala-setup-runtime`) is cross-compiled once by
//! `just stubs` and committed under `stubs/<target>/setup-stub.exe`; this crate
//! will `include_bytes!`-embed it and append `payload.zip + install.lua +
//! uninstall.lua + manifest + trailer` as a PE overlay — pure byte-writing,
//! no compile, no network.
//!
//! Scope for v1: self-extracting graphical installer with Start-menu shortcut,
//! Add/Remove Programs entry, per-user and per-machine modes, and a matching
//! `uninstall.exe`. Nothing else.
//!
//! Phase 1 is the stub-delivery pipeline only: the committed stubs land under
//! `stubs/`, and `just stubs` regenerates them. The `SetupSpec`/`build` API and
//! the `include_bytes!` embed arrive in Phase 2.
