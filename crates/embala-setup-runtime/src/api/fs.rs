//! `embala.fs.*` — file operations (spec R11). Reversible creations (`copy`,
//! `copy_tree`, `mkdir`) log a [`Record`]; destructive calls (`remove`,
//! `remove_tree`) are applied but not logged (no inverse).

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use mlua::{Lua, Table};

use super::{Engine, to_lua};
use crate::log::Record;

/// Build the `embala.fs` table.
pub fn table(lua: &Lua, engine: &Rc<RefCell<Engine>>) -> mlua::Result<Table> {
    let t = lua.create_table()?;

    macro_rules! two_arg {
        ($name:literal, $method:ident) => {{
            let engine = Rc::clone(engine);
            t.set(
                $name,
                lua.create_function(move |_, (src, dest): (String, String)| {
                    engine.borrow_mut().$method(&src, &dest).map_err(to_lua)
                })?,
            )?;
        }};
    }
    macro_rules! one_arg {
        ($name:literal, $method:ident) => {{
            let engine = Rc::clone(engine);
            t.set(
                $name,
                lua.create_function(move |_, path: String| {
                    engine.borrow_mut().$method(&path).map_err(to_lua)
                })?,
            )?;
        }};
    }

    two_arg!("copy", fs_copy);
    two_arg!("copy_tree", fs_copy_tree);
    one_arg!("mkdir", fs_mkdir);
    one_arg!("remove", fs_remove);
    one_arg!("remove_tree", fs_remove_tree);

    Ok(t)
}

impl Engine {
    /// Copy one file: `src` under `payload_dir`, `dest` under `install_dir`.
    fn fs_copy(&mut self, src: &str, dest: &str) -> std::io::Result<()> {
        let s = self.resolve_src(src);
        let d = self.resolve_dest(dest);
        if let Some(parent) = d.parent() {
            self.ensure_dir(parent)?;
        }
        // Record before the write (crash-ordering): a delete-if-exists reversal
        // is safe even if the copy never completed.
        self.record(Record::File { path: d.clone() });
        std::fs::copy(&s, &d)?;
        Ok(())
    }

    /// Recursively copy a directory tree, logging every dir and file created.
    fn fs_copy_tree(&mut self, src: &str, dest: &str) -> std::io::Result<()> {
        let s = self.resolve_src(src);
        let d = self.resolve_dest(dest);
        self.copy_tree_inner(&s, &d)
    }

    fn copy_tree_inner(&mut self, src: &Path, dest: &Path) -> std::io::Result<()> {
        self.ensure_dir(dest)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let from = entry.path();
            let to = dest.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                self.copy_tree_inner(&from, &to)?;
            } else {
                self.record(Record::File { path: to.clone() });
                std::fs::copy(&from, &to)?;
            }
        }
        Ok(())
    }

    /// Create a directory (and missing parents), logging each new level.
    fn fs_mkdir(&mut self, path: &str) -> std::io::Result<()> {
        let d = self.resolve_dest(path);
        self.ensure_dir(&d)
    }

    /// `remove` a single file. Destructive → not logged.
    fn fs_remove(&mut self, path: &str) -> std::io::Result<()> {
        let d = self.resolve_dest(path);
        match std::fs::remove_file(&d) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }

    /// `remove_tree`. Destructive → not logged.
    fn fs_remove_tree(&mut self, path: &str) -> std::io::Result<()> {
        let d = self.resolve_dest(path);
        match std::fs::remove_dir_all(&d) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }

    /// Create `dir` and any missing ancestors, appending an `Mkdir` record for
    /// each level this call actually creates (so LIFO reversal removes the
    /// deepest first and each is empty when removed). `pub(crate)` so sibling
    /// API submodules (`download`) reuse the same logged directory creation.
    pub(crate) fn ensure_dir(&mut self, dir: &Path) -> std::io::Result<()> {
        if dir.is_dir() {
            return Ok(());
        }
        if let Some(parent) = dir.parent() {
            if !parent.as_os_str().is_empty() {
                self.ensure_dir(parent)?;
            }
        }
        // Guard against a race/symlink: only record if we created it.
        match std::fs::create_dir(dir) {
            Ok(()) => {
                self.record(Record::Mkdir {
                    path: dir.to_path_buf(),
                });
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    }
}
