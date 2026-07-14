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
mod ui;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Lua, Table};

use embala_setup_overlay::Manifest;

use crate::log::{self, Record, Reversal};
use crate::mode::ResolvedMode;
use crate::wizard;

use ui::Page;

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
    /// Pages registered by `embala.ui.page{...}`, in call order. Stored for the
    /// Phase-5 renderer; this headless phase resolves each page's effect eagerly.
    pages: Vec<Page>,
    /// `/D=` was given → the directory page cannot re-point install_dir (CLI wins).
    dir_locked: bool,
    /// `/components=` ids, when the flag was given (else `None` → use defaults).
    cli_components: Option<Vec<String>>,
    /// Every component id declared by the components page (validates `selected`).
    component_ids: BTreeSet<String>,
    /// The resolved selected component ids (`ui.selected(id)` reads this).
    selected_components: BTreeSet<String>,
    /// Interactive run (not `/S`): the wizard renders; pages defer their effect
    /// to the user's choices instead of resolving eagerly (spec R15/R16).
    interactive: bool,
    /// This process is elevated (drives the mode-page relaunch decision, R13).
    elevated: bool,
    /// The wizard gate has already fired (runs once, before the first mutation).
    wizard_started: bool,
    /// The live wizard session (UI thread) during the progress/finish phases.
    ui: Option<wizard::Session>,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        manifest: Manifest,
        mode: ResolvedMode,
        arch: String,
        install_dir: PathBuf,
        payload_dir: PathBuf,
        attached: bool,
        dir_locked: bool,
        cli_components: Option<Vec<String>>,
        interactive: bool,
        elevated: bool,
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
            pages: Vec::new(),
            dir_locked,
            cli_components,
            component_ids: BTreeSet::new(),
            selected_components: BTreeSet::new(),
            interactive,
            elevated,
            wizard_started: false,
            ui: None,
        }
    }

    /// Whether pages should defer their effect to the wizard (interactive) rather
    /// than resolve eagerly (`/S` headless). Read by [`ui`] page handlers.
    fn is_interactive(&self) -> bool {
        self.interactive
    }

    /// The records logged so far (for writing `install.log` after a successful
    /// install and for rollback).
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// The resolved install directory (the wizard's directory page may have
    /// re-pointed it from the constructor default).
    pub fn install_dir(&self) -> &Path {
        &self.install_dir
    }

    /// The resolved install mode string (`per-user`/`per-machine`) — never
    /// `user-choice`. The uninstaller sidecar records this so a per-machine
    /// uninstall knows to elevate (spec R5).
    pub fn resolved_mode_str(&self) -> &'static str {
        self.mode.as_str()
    }

    /// Seed the declared/selected sets directly (spec R4) so `ui.selected(id)`
    /// works at uninstall time, where there is no components page to declare them.
    /// `ids` are every declared uninstall-option id; `selected` the resolved
    /// subset. Reuses the component machinery `is_selected` already reads.
    pub fn seed_options(&mut self, ids: BTreeSet<String>, selected: BTreeSet<String>) {
        self.component_ids = ids;
        self.selected_components = selected;
    }

    /// Flush the whole log to `<install_dir>\install.log` (spec R12). Called at
    /// the end of a successful install; also best-effort after each record.
    pub fn flush_log(&self) -> std::io::Result<()> {
        std::fs::write(&self.log_path, log::encode(&self.records))
    }

    /// `embala.log(msg)`: buffer for the progress page, echo when a console is
    /// attached (stdout in `/S`), and — when the wizard session is up — queue for
    /// its progress page (the UI thread's timer drains the queue, spec R15).
    fn log_line(&mut self, msg: &str) {
        if self.attached {
            println!("{msg}");
        }
        self.progress.push(msg.to_string());
        if let Some(ui) = &self.ui {
            ui.push_log(msg);
        }
    }

    /// Append a reversible record and best-effort flush (crash-ordering above).
    /// Also drives the progress page: each mutation advances the bar + status.
    fn record(&mut self, record: Record) {
        if let Some(ui) = &self.ui {
            ui.push_log(&progress_desc(&record));
        }
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
    embala.set("ui", ui::table(lua, engine)?)?;

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

/// A short progress-page line for a mutation record.
fn progress_desc(record: &Record) -> String {
    let name = |p: &Path| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    match record {
        Record::File { path } => format!("Installing {}", name(path)),
        Record::Mkdir { path } => format!("Creating {}", name(path)),
        Record::Shortcut { path } => format!("Creating shortcut {}", name(path)),
        Record::Registry { .. } => "Writing registry".to_string(),
        Record::Arp { .. } => "Registering with Add/Remove Programs".to_string(),
        Record::Env { name, .. } => format!("Setting environment variable {name}"),
    }
}

/// **The wizard gate (spec R15).** Called at the top of every mutating API
/// closure (which have the `&Lua`); on the *first* mutation of an interactive
/// run it starts the wizard session (a dedicated UI thread, see
/// [`crate::wizard`]), blocks until the user commits (Install) or bails
/// (Cancel/[X]/per-machine relaunch), applies the user's choices to the engine,
/// and only then lets the mutation proceed — the UI thread meanwhile shows the
/// progress page. Idempotent after the first call and a no-op under `/S`
/// (`interactive = false`), preserving headless parity (R16).
///
/// The choke point is the closure entry (not [`Engine::record`]) because a
/// mutating method resolves its paths from `install_dir` *before* recording, so
/// the directory page's choice must land first.
pub(crate) fn gate(engine: &Rc<RefCell<Engine>>, lua: &Lua) -> mlua::Result<()> {
    // Fast path: not interactive, or the wizard already ran.
    {
        let e = engine.borrow();
        if !e.interactive || e.wizard_started {
            return Ok(());
        }
    }

    // Build the plain wizard model with no engine borrow held while blocked.
    let model = {
        let mut e = engine.borrow_mut();
        e.wizard_started = true;
        e.build_wizard_model()
    };
    // No pre-install pages → no wizard UI (a bare hand-written script). The
    // mutation proceeds and the completion is reported as today (documented).
    if model.steps.is_empty() {
        return Ok(());
    }

    let session = wizard::start(model);
    match session.wait_preinstall() {
        wizard::PreOutcome::Cancel => {
            session.quit();
            crate::sys::request_cancel();
            Err(mlua::Error::runtime("install cancelled by user"))
        }
        wizard::PreOutcome::Relaunch => {
            session.quit();
            match wizard::relaunch_per_machine() {
                // The elevated child re-runs the whole wizard (acceptable v1).
                Ok(code) => std::process::exit(code as i32),
                // UAC declined → treat as a clean cancel (nothing has mutated).
                Err(_) => {
                    crate::sys::request_cancel();
                    Err(mlua::Error::runtime("install cancelled by user"))
                }
            }
        }
        wizard::PreOutcome::Install(choices) => {
            let mut e = engine.borrow_mut();
            e.apply_choices(&choices);
            e.ui = Some(session);
            // Re-sync the Lua context so later reads (ARP install_location,
            // shortcut targets, finish run) see the resolved directory + mode.
            let g = lua.globals().get::<Table>("embala")?;
            g.set("install_dir", e.install_dir_string())?;
            g.set("mode", e.mode.as_str())?;
            Ok(())
        }
    }
}

impl Engine {
    /// Build the winsafe-free [`wizard::WizardModel`] from the registered pages.
    fn build_wizard_model(&self) -> wizard::WizardModel {
        let name = &self.manifest.package.name;
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let pf = std::env::var("ProgramFiles").unwrap_or_default();
        let dir_for = |m| {
            crate::mode::default_install_dir(m, name, &local, &pf)
                .to_string_lossy()
                .into_owned()
        };

        let mut steps = Vec::new();
        for page in &self.pages {
            match page {
                Page::Mode => steps.push(wizard::Step::Mode),
                Page::Welcome { title, body } => steps.push(wizard::Step::Welcome {
                    title: title.clone().unwrap_or_default(),
                    body: body
                        .clone()
                        .unwrap_or_else(|| self.manifest.package.description.clone()),
                }),
                Page::License { text, must_accept } => steps.push(wizard::Step::License {
                    text: text.clone().unwrap_or_default(),
                    must_accept: *must_accept,
                }),
                Page::Components { items } => {
                    let items = items
                        .iter()
                        .map(|i| wizard::CompItem {
                            id: i.id.clone(),
                            label: match &i.description {
                                Some(d) => format!("{} — {d}", i.label),
                                None => i.label.clone(),
                            },
                            default: i.default,
                        })
                        .collect();
                    steps.push(wizard::Step::Components { items });
                }
                Page::Directory {
                    default,
                    allow_change,
                } => steps.push(wizard::Step::Directory {
                    default: default.clone().unwrap_or_else(|| self.install_dir_string()),
                    allow_change: *allow_change,
                }),
                // The finish page renders as a separate window after the script
                // completes (it is registered last), so it is not a pre-install
                // step; see `build_finish_spec`.
                Page::Finish { .. } => {}
            }
        }

        wizard::WizardModel {
            display_name: self.manifest.package.display_name.clone(),
            version: self.manifest.package.version.clone(),
            steps,
            default_dir_user: dir_for(ResolvedMode::PerUser),
            default_dir_machine: dir_for(ResolvedMode::PerMachine),
            current_mode_machine: self.mode == ResolvedMode::PerMachine,
            elevated: self.elevated,
            dir_locked: self.dir_locked,
        }
    }

    /// Apply the wizard's pre-install choices before any mutation runs.
    fn apply_choices(&mut self, c: &wizard::Choices) {
        if let Some(dir) = &c.install_dir {
            if !self.dir_locked {
                let p = PathBuf::from(dir);
                self.log_path = p.join("install.log");
                self.install_dir = p;
            }
        }
        // Per-machine is only reachable here when elevated (else we relaunched).
        self.mode = if c.mode_machine {
            ResolvedMode::PerMachine
        } else {
            ResolvedMode::PerUser
        };
        self.component_ids = c.component_ids.iter().cloned().collect();
        self.selected_components = c.selected.iter().cloned().collect();
    }

    /// Conclude the wizard session after a successful install. If the script
    /// registered a finish page, hand it to the UI thread (which swaps the
    /// window to the finish stage and waits for the user's Finish click,
    /// honouring the run-app checkbox); else the session just ends. Returns
    /// whether the wizard owned the completion UI (so the caller skips the
    /// default message box). Interactive only.
    pub fn finish_ui(&mut self) -> bool {
        let Some(session) = self.ui.take() else {
            return false;
        };
        match self.build_finish_data() {
            Some(data) => {
                session.finish(data);
                true
            }
            None => {
                session.quit();
                false
            }
        }
    }

    /// Build the finish-page payload from the pages registered by the (now
    /// completed) script, resolving the run target against the final install
    /// dir; `None` if the script registered no finish page.
    fn build_finish_data(&self) -> Option<wizard::FinishData> {
        self.pages.iter().find_map(|p| match p {
            Page::Finish { body, run, links } => Some(wizard::FinishData {
                body: body.clone(),
                run_label: run.as_ref().and_then(|r| r.label.clone()),
                run_target: run.as_ref().map(|r| self.resolve_dest(&r.target)),
                links: links.clone(),
            }),
            _ => None,
        })
    }

    /// Tear down the wizard session on a failed/cancelled install.
    pub fn close_ui(&mut self) {
        if let Some(session) = self.ui.take() {
            session.quit();
        }
    }
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
