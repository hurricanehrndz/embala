//! The native `embala.*` installer API exposed to Lua (spec R11), backed by
//! winsafe/Win32/COM. Split by domain: [`fs`], [`shortcut`], [`registry`]
//! (registry + ARP + env), [`exec`] (exec + download). This module owns the
//! shared [`Engine`] state, assembles the `embala` global, and holds the reversal
//! executor that both mid-install rollback and the uninstaller replay.
//!
//! **Shared state.** Closures capture a clone of `Rc<RefCell<Engine>>` (mlua has
//! no `send` feature, so single-threaded `Rc` is fine and avoids app-data
//! re-entrancy caveats). Each closure borrows the engine for the duration of one
//! leaf operation only.
//!
//! **Crash-ordering (applied uniformly, per [`crate::log`]).** A reversible
//! mutation records its [`Record`] *before* it touches the system and the log is
//! best-effort flushed, so an interrupt leaves a log that reverses work that
//! *might* have happened; every reversal is idempotent (delete-if-exists /
//! rmdir-if-empty), so reversing a not-actually-created resource is harmless.
//! Destructive calls (`fs.remove*`, `registry.delete`) have no inverse and are
//! therefore applied but **not** logged.

mod exec;
mod fs;
mod registry;
mod shortcut;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Lua, Table};

use embala_setup_overlay::Manifest;

use crate::log::{self, Record, Reversal};
use crate::mode::ResolvedMode;

/// Shared, mutable install state threaded through every mutating API call.
pub struct Engine {
    manifest: Manifest,
    mode: ResolvedMode,
    arch: String,
    install_dir: PathBuf,
    payload_dir: PathBuf,
    /// A console is attached (`/S` or launched from a shell) → echo `log` lines.
    attached: bool,
    /// Reversible mutations, in call order; reversed LIFO on rollback/uninstall.
    records: Vec<Record>,
    /// `<install_dir>\install.log`, flushed best-effort after each record.
    log_path: PathBuf,
    /// Buffered `log` lines for the future progress page (spec R11).
    progress: Vec<String>,
}

impl Engine {
    pub fn new(
        manifest: Manifest,
        mode: ResolvedMode,
        arch: String,
        install_dir: PathBuf,
        payload_dir: PathBuf,
        attached: bool,
    ) -> Engine {
        let log_path = install_dir.join("install.log");
        Engine {
            manifest,
            mode,
            arch,
            install_dir,
            payload_dir,
            attached,
            records: Vec::new(),
            log_path,
            progress: Vec::new(),
        }
    }

    /// The records logged so far (for writing `install.log` after a successful
    /// install and for rollback).
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Flush the whole log to `<install_dir>\install.log` (spec R12). Called at
    /// the end of a successful install; also best-effort after each record.
    pub fn flush_log(&self) -> std::io::Result<()> {
        std::fs::write(&self.log_path, log::encode(&self.records))
    }

    /// `embala.log(msg)`: buffer for the progress page, echo when a console is
    /// attached (stdout in `/S`), never pop a window.
    fn log_line(&mut self, msg: &str) {
        if self.attached {
            println!("{msg}");
        }
        self.progress.push(msg.to_string());
    }

    /// Append a reversible record and best-effort flush (crash-ordering above).
    fn record(&mut self, record: Record) {
        self.records.push(record);
        let _ = self.flush_log();
    }

    /// Resolve an API `src` (relative to `payload_dir`, absolute allowed).
    fn resolve_src(&self, rel: &str) -> PathBuf {
        self.payload_dir.join(rel)
    }

    /// Resolve an API `dest` (relative to `install_dir`, absolute allowed).
    fn resolve_dest(&self, rel: &str) -> PathBuf {
        let p = Path::new(rel);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.install_dir.join(p)
        }
    }
}

/// Assemble the `embala` global and its sub-tables, wiring the engine into every
/// mutating closure. Read-only context (package/arch/mode/dirs) is snapshotted as
/// plain values — in this headless phase `install_dir` is fixed (the directory
/// page is Phase 5).
pub fn register(lua: &Lua, engine: &Rc<RefCell<Engine>>) -> mlua::Result<()> {
    let embala = lua.create_table()?;

    // --- read-only context (spec R11) --------------------------------------
    {
        let eng = engine.borrow();
        embala.set("package", package_table(lua, &eng.manifest)?)?;
        embala.set("mode", eng.mode.as_str())?;
        embala.set("install_dir", path_str(&eng.install_dir))?;
        embala.set("payload_dir", path_str(&eng.payload_dir))?;

        let arch = eng.arch.clone();
        embala.set("arch", lua.create_function(move |_, ()| Ok(arch.clone()))?)?;
    }
    embala.set(
        "os_version",
        lua.create_function(|lua, ()| {
            let (major, minor, build) = crate::sys::os_version();
            let t = lua.create_table()?;
            t.set("major", major)?;
            t.set("minor", minor)?;
            t.set("build", build)?;
            Ok(t)
        })?,
    )?;

    // embala.log(msg)
    {
        let engine = Rc::clone(engine);
        embala.set(
            "log",
            lua.create_function(move |_, msg: String| {
                engine.borrow_mut().log_line(&msg);
                Ok(())
            })?,
        )?;
    }

    // --- mutating sub-tables ------------------------------------------------
    embala.set("fs", fs::table(lua, engine)?)?;
    embala.set("shortcut", shortcut::table(lua, engine)?)?;
    embala.set("registry", registry::registry_table(lua, engine)?)?;
    embala.set("arp", registry::arp_table(lua, engine)?)?;
    embala.set("env", registry::env_table(lua, engine)?)?;
    embala.set("exec", exec::exec_fn(lua)?)?;
    embala.set("download", exec::download_fn(lua, engine)?)?;

    lua.globals().set("embala", embala)?;
    Ok(())
}

/// Build the `embala.package` table from the manifest.
fn package_table(lua: &Lua, manifest: &Manifest) -> mlua::Result<Table> {
    let p = &manifest.package;
    let t = lua.create_table()?;
    t.set("name", p.name.as_str())?;
    t.set("display_name", p.display_name.as_str())?;
    t.set("version", p.version.as_str())?;
    t.set("identifier", p.identifier.as_str())?;
    t.set("publisher", p.publisher.as_str())?;
    t.set("description", p.description.as_str())?;
    t.set("homepage", p.homepage.clone())?;
    t.set("license", p.license.clone())?;
    Ok(t)
}

/// Lossy UTF-8 of a path for handing to Lua (install paths are UTF-8 in practice).
fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Map any error to an mlua runtime error (the script sees a Lua-level error it
/// can neither swallow-by-accident nor misattribute).
fn to_lua<E: std::fmt::Display>(e: E) -> mlua::Error {
    mlua::Error::runtime(e.to_string())
}

/// Replay an install log LIFO, best-effort (spec R14). Every action is
/// idempotent, so a already-gone resource is a no-op; errors are swallowed so one
/// stuck value cannot strand the rest of the cleanup. Shared by mid-install
/// rollback and the uninstaller.
pub fn reverse_all(records: &[Record]) {
    for reversal in log::reversal_plan(records) {
        let _ = apply_reversal(&reversal);
    }
}

/// Execute one undo action against the system.
fn apply_reversal(reversal: &Reversal) -> std::io::Result<()> {
    match reversal {
        Reversal::DeleteFile(path) | Reversal::DeleteShortcut(path) => remove_file_if_exists(path),
        Reversal::RemoveDirIfEmpty(path) => {
            // Non-empty dirs (user data, still-present siblings) are left alone.
            let _ = std::fs::remove_dir(path);
            Ok(())
        }
        Reversal::DeleteRegistryValue {
            hive,
            key,
            name,
            delete_key,
        } => registry::reverse_registry(*hive, key, name.as_deref(), *delete_key),
        Reversal::DeleteArp { hive, identifier } => registry::reverse_arp(*hive, identifier),
        Reversal::DeleteEnv { name, scope } => registry::reverse_env(name, *scope),
    }
}

fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
