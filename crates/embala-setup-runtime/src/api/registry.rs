//! `embala.registry.*`, `embala.arp.register`, `embala.env.set` (spec R11) plus
//! their reversal executors. All three are registry-backed, so they share the
//! winsafe `HKEY` plumbing here.

use std::cell::RefCell;
use std::io;
use std::rc::Rc;

use mlua::{Lua, Table};
use winsafe::{self as w, co, msg};

use super::{Engine, to_lua};
use crate::log::{EnvScope, Hive, Record};

/// The Uninstall root the ARP entry lives under (spec R11/R14).
const UNINSTALL_ROOT: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";

// ---------------------------------------------------------------------------
// Table builders
// ---------------------------------------------------------------------------

pub fn registry_table(lua: &Lua, engine: &Rc<RefCell<Engine>>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    {
        let engine = Rc::clone(engine);
        t.set(
            "set",
            lua.create_function(move |_, spec: Table| {
                let hive = parse_hive(&spec.get::<String>("hive")?)?;
                let key = spec.get::<String>("key")?;
                let name = spec.get::<Option<String>>("name")?;
                let kind = spec
                    .get::<Option<String>>("kind")?
                    .unwrap_or_else(|| "sz".to_string());
                let value = parse_value(&spec, &kind)?;
                engine
                    .borrow_mut()
                    .registry_set(hive, &key, name.as_deref(), value)
                    .map_err(to_lua)
            })?,
        )?;
    }
    {
        let engine = Rc::clone(engine);
        t.set(
            "delete",
            lua.create_function(move |_, spec: Table| {
                let hive = parse_hive(&spec.get::<String>("hive")?)?;
                let key = spec.get::<String>("key")?;
                let name = spec.get::<Option<String>>("name")?;
                engine
                    .borrow_mut()
                    .registry_delete(hive, &key, name.as_deref())
                    .map_err(to_lua)
            })?,
        )?;
    }
    Ok(t)
}

pub fn arp_table(lua: &Lua, engine: &Rc<RefCell<Engine>>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let engine = Rc::clone(engine);
    t.set(
        "register",
        lua.create_function(move |_, spec: Table| {
            let a = ArpArgs {
                display_name: spec.get::<String>("display_name")?,
                version: spec.get::<String>("version")?,
                publisher: spec.get::<String>("publisher")?,
                install_location: spec.get::<String>("install_location")?,
                uninstall_string: spec.get::<String>("uninstall_string")?,
                display_icon: spec.get::<Option<String>>("display_icon")?,
                estimated_size: spec.get::<Option<i64>>("estimated_size")?,
                no_modify: spec.get::<Option<bool>>("no_modify")?.unwrap_or(false),
                no_repair: spec.get::<Option<bool>>("no_repair")?.unwrap_or(false),
            };
            engine.borrow_mut().arp_register(&a).map_err(to_lua)
        })?,
    )?;
    Ok(t)
}

pub fn env_table(lua: &Lua, engine: &Rc<RefCell<Engine>>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let engine = Rc::clone(engine);
    t.set(
        "set",
        lua.create_function(move |_, spec: Table| {
            let name = spec.get::<String>("name")?;
            let value = spec.get::<String>("value")?;
            let scope = spec.get::<Option<String>>("scope")?;
            engine
                .borrow_mut()
                .env_set(&name, &value, scope.as_deref())
                .map_err(to_lua)
        })?,
    )?;
    Ok(t)
}

// ---------------------------------------------------------------------------
// Engine methods (forward)
// ---------------------------------------------------------------------------

struct ArpArgs {
    display_name: String,
    version: String,
    publisher: String,
    install_location: String,
    uninstall_string: String,
    display_icon: Option<String>,
    estimated_size: Option<i64>,
    no_modify: bool,
    no_repair: bool,
}

impl Engine {
    fn registry_set(
        &mut self,
        hive: Hive,
        key: &str,
        name: Option<&str>,
        value: w::RegistryValue,
    ) -> io::Result<()> {
        let (guard, created) = create_key(hive, key)?;
        self.record(Record::Registry {
            hive,
            key: key.to_string(),
            name: name.map(str::to_string),
            key_created: created,
        });
        guard.RegSetValueEx(name, value).map_err(win)?;
        Ok(())
    }

    /// Destructive → not logged. `name` present deletes the value; absent deletes
    /// the whole key tree.
    fn registry_delete(&mut self, hive: Hive, key: &str, name: Option<&str>) -> io::Result<()> {
        let root = hive_key(hive);
        match name {
            Some(_) => {
                let guard = root
                    .RegOpenKeyEx(Some(key), co::REG_OPTION::default(), co::KEY::WRITE)
                    .map_err(win)?;
                guard.RegDeleteValue(name).map_err(win)?;
            }
            None => root.RegDeleteTree(Some(key)).map_err(win)?,
        }
        Ok(())
    }

    fn arp_register(&mut self, a: &ArpArgs) -> io::Result<()> {
        let hive = self.mode.hive();
        let identifier = self.manifest.package.identifier.clone();
        let key = format!(r"{UNINSTALL_ROOT}\{identifier}");
        let (guard, _created) = create_key(hive, &key)?;

        // Reversal deletes the whole Uninstall\<id> key regardless of who created
        // it, so the record is an `Arp` (not a plain Registry value).
        self.record(Record::Arp { hive, identifier });

        let sz = |v: &str| w::RegistryValue::Sz(v.to_string());
        guard
            .RegSetValueEx(Some("DisplayName"), sz(&a.display_name))
            .map_err(win)?;
        guard
            .RegSetValueEx(Some("DisplayVersion"), sz(&a.version))
            .map_err(win)?;
        guard
            .RegSetValueEx(Some("Publisher"), sz(&a.publisher))
            .map_err(win)?;
        guard
            .RegSetValueEx(Some("InstallLocation"), sz(&a.install_location))
            .map_err(win)?;
        guard
            .RegSetValueEx(Some("UninstallString"), sz(&a.uninstall_string))
            .map_err(win)?;
        if let Some(icon) = &a.display_icon {
            guard
                .RegSetValueEx(Some("DisplayIcon"), sz(icon))
                .map_err(win)?;
        }
        if let Some(size) = a.estimated_size {
            guard
                .RegSetValueEx(Some("EstimatedSize"), w::RegistryValue::Dword(size as u32))
                .map_err(win)?;
        }
        if a.no_modify {
            guard
                .RegSetValueEx(Some("NoModify"), w::RegistryValue::Dword(1))
                .map_err(win)?;
        }
        if a.no_repair {
            guard
                .RegSetValueEx(Some("NoRepair"), w::RegistryValue::Dword(1))
                .map_err(win)?;
        }
        Ok(())
    }

    fn env_set(&mut self, name: &str, value: &str, scope: Option<&str>) -> io::Result<()> {
        let scope = parse_scope(scope, self.mode);
        let (hive, key) = env_location(scope);
        let (guard, _created) = create_key(hive, key)?;
        self.record(Record::Env {
            name: name.to_string(),
            scope,
        });
        // ExpandSz so a value containing %VARS% (e.g. a PATH addition) expands.
        guard
            .RegSetValueEx(Some(name), w::RegistryValue::ExpandSz(value.to_string()))
            .map_err(win)?;
        broadcast_env_change();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reversal executors (called from api::apply_reversal)
// ---------------------------------------------------------------------------

pub fn reverse_registry(
    hive: Hive,
    key: &str,
    name: Option<&str>,
    delete_key: bool,
) -> io::Result<()> {
    let root = hive_key(hive);
    if delete_key {
        // The install created the key: remove it and everything under it.
        let _ = root.RegDeleteTree(Some(key));
    } else if let Ok(guard) =
        root.RegOpenKeyEx(Some(key), co::REG_OPTION::default(), co::KEY::WRITE)
    {
        let _ = guard.RegDeleteValue(name);
    }
    Ok(())
}

pub fn reverse_arp(hive: Hive, identifier: &str) -> io::Result<()> {
    let key = format!(r"{UNINSTALL_ROOT}\{identifier}");
    let _ = hive_key(hive).RegDeleteTree(Some(&key));
    Ok(())
}

pub fn reverse_env(name: &str, scope: EnvScope) -> io::Result<()> {
    let (hive, key) = env_location(scope);
    if let Ok(guard) =
        hive_key(hive).RegOpenKeyEx(Some(key), co::REG_OPTION::default(), co::KEY::WRITE)
    {
        let _ = guard.RegDeleteValue(Some(name));
    }
    broadcast_env_change();
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn hive_key(hive: Hive) -> w::HKEY {
    match hive {
        Hive::Hkcu => w::HKEY::CURRENT_USER,
        Hive::Hklm => w::HKEY::LOCAL_MACHINE,
    }
}

/// Create-or-open `key` under `hive`, returning the guard and whether this call
/// created the key (drives `key_created` in the log).
fn create_key(hive: Hive, key: &str) -> io::Result<(w::guard::RegCloseKeyGuard, bool)> {
    let (guard, disposition) = hive_key(hive)
        .RegCreateKeyEx(
            key,
            None,
            co::REG_OPTION::NON_VOLATILE,
            co::KEY::WRITE,
            None,
        )
        .map_err(win)?;
    Ok((guard, disposition == co::REG_DISPOSITION::CREATED_NEW_KEY))
}

fn env_location(scope: EnvScope) -> (Hive, &'static str) {
    match scope {
        EnvScope::User => (Hive::Hkcu, "Environment"),
        EnvScope::Machine => (
            Hive::Hklm,
            r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
        ),
    }
}

/// Broadcast `WM_SETTINGCHANGE`/"Environment" so running shells pick up the
/// change (spec R11). Best-effort; a failed broadcast just delays visibility.
fn broadcast_env_change() {
    let env = w::WString::from_str("Environment");
    unsafe {
        let _ = w::HWND::BROADCAST.SendMessageTimeout(
            msg::Wm {
                msg_id: co::WM::WININICHANGE,
                wparam: 0,
                lparam: env.as_ptr() as isize,
            },
            co::SMTO::ABORTIFHUNG,
            5000,
        );
    }
}

fn parse_hive(s: &str) -> mlua::Result<Hive> {
    match s {
        "HKCU" => Ok(Hive::Hkcu),
        "HKLM" => Ok(Hive::Hklm),
        other => Err(mlua::Error::runtime(format!(
            "registry: unknown hive {other:?} (expected HKCU or HKLM)"
        ))),
    }
}

fn parse_scope(scope: Option<&str>, mode: crate::mode::ResolvedMode) -> EnvScope {
    match scope {
        Some("machine") => EnvScope::Machine,
        Some("user") => EnvScope::User,
        // Default follows the install mode (spec R11).
        _ => match mode {
            crate::mode::ResolvedMode::PerMachine => EnvScope::Machine,
            crate::mode::ResolvedMode::PerUser => EnvScope::User,
        },
    }
}

fn parse_value(spec: &Table, kind: &str) -> mlua::Result<w::RegistryValue> {
    Ok(match kind {
        "dword" => w::RegistryValue::Dword(spec.get::<i64>("value")? as u32),
        "expand-sz" => w::RegistryValue::ExpandSz(spec.get::<String>("value")?),
        "sz" => w::RegistryValue::Sz(spec.get::<String>("value")?),
        other => {
            return Err(mlua::Error::runtime(format!(
                "registry: unknown kind {other:?} (expected sz, dword, or expand-sz)"
            )));
        }
    })
}

/// Map a winsafe error into an `io::Error` carrying its display text.
fn win<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}
