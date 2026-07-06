//! `embala.exec{...}` and `embala.download{...}` (spec R11).
//!
//! `exec` runs a process via `std::process` and returns `{code, stdout?,
//! stderr?}`; it has no inverse and is not logged. `download` fetches over
//! WinHTTP (system TLS, [`crate::sys::http_get`]), verifies the sha256 with
//! `sha2` (no rustls/openssl), and writes the file — a reversible creation, so it
//! logs a `File` record.

use std::cell::RefCell;
use std::io;
use std::process::Command;
use std::rc::Rc;

use mlua::{Lua, Table};
use sha2::{Digest as _, Sha256};

use super::{Engine, to_lua};
use crate::log::Record;

/// `embala.exec` — no engine needed (unlogged).
pub fn exec_fn(lua: &Lua) -> mlua::Result<mlua::Function> {
    lua.create_function(|lua, spec: Table| {
        let cmd = spec.get::<String>("cmd")?;
        let args = spec.get::<Option<Vec<String>>>("args")?.unwrap_or_default();
        let cwd = spec.get::<Option<String>>("cwd")?;
        let wait = spec.get::<Option<bool>>("wait")?.unwrap_or(true);

        let mut command = Command::new(&cmd);
        command.args(&args);
        if let Some(cwd) = &cwd {
            command.current_dir(cwd);
        }

        let result = lua.create_table()?;
        if wait {
            let out = command.output().map_err(to_lua)?;
            result.set("code", out.status.code().unwrap_or(-1))?;
            result.set("stdout", String::from_utf8_lossy(&out.stdout).into_owned())?;
            result.set("stderr", String::from_utf8_lossy(&out.stderr).into_owned())?;
        } else {
            // Fire-and-forget: report launch success as code 0.
            command.spawn().map_err(to_lua)?;
            result.set("code", 0)?;
        }
        Ok(result)
    })
}

/// `embala.download` — needs the engine to resolve `dest` and log the file.
pub fn download_fn(lua: &Lua, engine: &Rc<RefCell<Engine>>) -> mlua::Result<mlua::Function> {
    let engine = Rc::clone(engine);
    lua.create_function(move |_, spec: Table| {
        let url = spec.get::<String>("url")?;
        let sha256 = spec.get::<String>("sha256")?;
        let dest = spec.get::<String>("dest")?;
        engine
            .borrow_mut()
            .download(&url, &sha256, &dest)
            .map_err(to_lua)
    })
}

impl Engine {
    fn download(&mut self, url: &str, sha256: &str, dest: &str) -> io::Result<()> {
        let body = crate::sys::http_get(url)?;
        let got = hex(Sha256::digest(&body));
        if !got.eq_ignore_ascii_case(sha256) {
            return Err(io::Error::other(format!(
                "download: checksum mismatch for {url} (expected {sha256}, got {got})"
            )));
        }
        let d = self.resolve_dest(dest);
        if let Some(parent) = d.parent() {
            self.ensure_dir(parent)?;
        }
        self.record(Record::File { path: d.clone() });
        std::fs::write(&d, &body)?;
        Ok(())
    }
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
