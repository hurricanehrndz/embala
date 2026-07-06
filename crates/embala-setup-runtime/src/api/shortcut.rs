//! `embala.shortcut.create{...}` — a `.lnk` via `IShellLink` + `IPersistFile`
//! COM (spec R11). The `.lnk` lands in the mode's Start Menu\Programs (default)
//! or Desktop folder; `target` resolves relative to `install_dir`.

use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{Lua, Table};
use winsafe::{self as w, co, prelude::*};

use super::{Engine, to_lua};
use crate::log::Record;
use crate::mode::ResolvedMode;

/// Parsed `shortcut.create{...}` arguments.
struct ShortcutArgs {
    name: String,
    target: String,
    args: Option<String>,
    icon: Option<String>,
    workdir: Option<String>,
    description: Option<String>,
    location: String,
}

/// Build the `embala.shortcut` table.
pub fn table(lua: &Lua, engine: &Rc<RefCell<Engine>>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let engine = Rc::clone(engine);
    t.set(
        "create",
        lua.create_function(move |lua, spec: Table| {
            super::gate(&engine, lua)?;
            let args = ShortcutArgs {
                name: spec.get::<String>("name")?,
                target: spec.get::<String>("target")?,
                args: spec.get::<Option<String>>("args")?,
                icon: spec.get::<Option<String>>("icon")?,
                workdir: spec.get::<Option<String>>("workdir")?,
                description: spec.get::<Option<String>>("description")?,
                location: spec
                    .get::<Option<String>>("location")?
                    .unwrap_or_else(|| "start-menu".to_string()),
            };
            engine.borrow_mut().shortcut_create(&args).map_err(to_lua)
        })?,
    )?;
    Ok(t)
}

impl Engine {
    fn shortcut_create(&mut self, a: &ShortcutArgs) -> io::Result<()> {
        let dir = shortcut_dir(&a.location, self.mode)?;
        // The Programs/Desktop folder normally exists; create best-effort but do
        // not log it (reversing it could remove a shared system folder).
        std::fs::create_dir_all(&dir)?;
        let lnk = dir.join(format!("{}.lnk", a.name));
        let target = self.resolve_dest(&a.target);
        let workdir = a
            .workdir
            .clone()
            .unwrap_or_else(|| target.parent().map(path_string).unwrap_or_default());

        // Record before writing the .lnk (crash-ordering; delete-if-exists undo).
        self.record(Record::Shortcut { path: lnk.clone() });
        write_lnk(&lnk, &path_string(&target), &workdir, a).map_err(io::Error::other)?;
        Ok(())
    }
}

/// Create the `.lnk` via COM. Kept free-standing (returns a winsafe error string)
/// so the COM apartment guard scopes to exactly this call.
fn write_lnk(
    lnk: &std::path::Path,
    target: &str,
    workdir: &str,
    a: &ShortcutArgs,
) -> Result<(), String> {
    let _com = w::CoInitializeEx(co::COINIT::APARTMENTTHREADED | co::COINIT::DISABLE_OLE1DDE)
        .map_err(|e| e.to_string())?;
    let link = w::CoCreateInstance::<w::IShellLink>(
        &co::CLSID::ShellLink,
        None::<&w::IUnknown>,
        co::CLSCTX::INPROC_SERVER,
    )
    .map_err(|e| e.to_string())?;

    link.SetPath(target).map_err(|e| e.to_string())?;
    link.SetWorkingDirectory(workdir)
        .map_err(|e| e.to_string())?;
    if let Some(args) = &a.args {
        link.SetArguments(args).map_err(|e| e.to_string())?;
    }
    if let Some(desc) = &a.description {
        link.SetDescription(desc).map_err(|e| e.to_string())?;
    }
    if let Some(icon) = &a.icon {
        link.SetIconLocation(icon, 0).map_err(|e| e.to_string())?;
    }

    let persist = link
        .QueryInterface::<w::IPersistFile>()
        .map_err(|e| e.to_string())?;
    persist
        .Save(Some(&lnk.to_string_lossy()), true)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Resolve the shortcut folder for `location` under the resolved mode.
fn shortcut_dir(location: &str, mode: ResolvedMode) -> io::Result<PathBuf> {
    let env = |k: &str| {
        std::env::var(k).map_err(|_| io::Error::other(format!("missing %{k}% for shortcut")))
    };
    const START: &str = r"Microsoft\Windows\Start Menu\Programs";
    let dir = match (location, mode) {
        ("desktop", ResolvedMode::PerUser) => PathBuf::from(env("USERPROFILE")?).join("Desktop"),
        ("desktop", ResolvedMode::PerMachine) => PathBuf::from(env("PUBLIC")?).join("Desktop"),
        // start-menu (default) — per-user under %APPDATA%, per-machine all-users.
        (_, ResolvedMode::PerUser) => PathBuf::from(env("APPDATA")?).join(START),
        (_, ResolvedMode::PerMachine) => PathBuf::from(env("ProgramData")?).join(START),
    };
    Ok(dir)
}

fn path_string(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}
