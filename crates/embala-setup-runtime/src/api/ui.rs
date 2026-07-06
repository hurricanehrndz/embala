//! `embala.ui.page{...}` / `embala.ui.selected(id)` — headless resolution only
//! (spec R15/R16). This phase treats every run headlessly: pages register their
//! definition (stored for the Phase-5 winsafe renderer) and resolve their effect
//! eagerly from config + CLI flags. No window is ever shown here.
//!
//! Per-kind headless semantics:
//! - `mode`: no-op — the install mode is resolved from `/mode=`/the manifest
//!   before the script runs (spec R13); this page is only a Phase-5 render marker.
//! - `welcome` / `finish`: no-op (informational).
//! - `license`: auto-accept under headless/`/S` (spec R16), logging the fact.
//!   Phase 5 gates the interactive must-accept.
//! - `directory`: re-point `install_dir` to `default` unless `/D=` was given
//!   (CLI wins), and only before any mutating call has run (else it is a script
//!   bug — a hard error). The `embala.install_dir` global is re-synced so later
//!   ARP/shortcut calls see the final path.
//! - `components`: resolve the selected set from `/components=` (if given) else
//!   each item's `default`; `ui.selected(id)` reads it and errors on an unknown id.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{Lua, Table};

use super::{Engine, to_lua};

/// A wizard page registered by `install.lua`, stored in call order and consumed
/// by the winsafe renderer (`crate::wizard`, spec R15). Under `/S` the page
/// effects resolve eagerly instead (R16); interactively they are deferred to the
/// user's choices in the wizard.
pub enum Page {
    Mode,
    Welcome {
        title: Option<String>,
        body: Option<String>,
    },
    License {
        text: Option<String>,
        must_accept: bool,
    },
    Directory {
        default: Option<String>,
        allow_change: bool,
    },
    Components {
        items: Vec<ComponentItem>,
    },
    Finish {
        body: Option<String>,
        run: Option<RunTarget>,
        links: Vec<(String, String)>,
    },
}

/// One entry of a `components` page.
pub struct ComponentItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub default: bool,
}

/// The optional "run app on finish" target of a `finish` page.
pub struct RunTarget {
    pub target: String,
    pub label: Option<String>,
}

/// Build the `embala.ui` table.
pub fn table(lua: &Lua, engine: &Rc<RefCell<Engine>>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    {
        let engine = Rc::clone(engine);
        t.set(
            "page",
            lua.create_function(move |lua, spec: Table| handle_page(lua, &engine, spec))?,
        )?;
    }
    {
        let engine = Rc::clone(engine);
        t.set(
            "selected",
            lua.create_function(move |_, id: String| {
                engine.borrow().is_selected(&id).map_err(to_lua)
            })?,
        )?;
    }
    Ok(t)
}

/// Register + eagerly resolve one page.
fn handle_page(lua: &Lua, engine: &Rc<RefCell<Engine>>, spec: Table) -> mlua::Result<()> {
    let kind = spec.get::<String>("kind")?;
    match kind.as_str() {
        "mode" => engine.borrow_mut().push_page(Page::Mode),
        "welcome" => {
            let page = Page::Welcome {
                title: spec.get::<Option<String>>("title")?,
                body: spec.get::<Option<String>>("body")?,
            };
            engine.borrow_mut().push_page(page);
        }
        "license" => {
            let text = spec.get::<Option<String>>("text")?;
            let must_accept = spec.get::<Option<bool>>("must_accept")?.unwrap_or(true);
            let mut eng = engine.borrow_mut();
            // Interactive: the wizard gates the must-accept (spec R15). Headless:
            // auto-accept and log it (spec R16).
            if !eng.is_interactive() {
                eng.accept_license_silent();
            }
            eng.push_page(Page::License { text, must_accept });
        }
        "directory" => {
            let default = spec.get::<Option<String>>("default")?;
            let allow_change = spec.get::<Option<bool>>("allow_change")?.unwrap_or(true);
            {
                let mut eng = engine.borrow_mut();
                // Interactive: the directory page defers to the wizard. Headless:
                // re-point install_dir now (spec R16) and re-sync the context.
                if !eng.is_interactive() {
                    eng.apply_directory(default.as_deref()).map_err(to_lua)?;
                }
            }
            if !engine.borrow().is_interactive() {
                let dir = engine.borrow().install_dir_string();
                lua.globals()
                    .get::<Table>("embala")?
                    .set("install_dir", dir)?;
            }
            engine.borrow_mut().push_page(Page::Directory {
                default,
                allow_change,
            });
        }
        "components" => {
            let items = parse_items(&spec)?;
            let mut eng = engine.borrow_mut();
            // Interactive: the wizard resolves the selection; headless: resolve
            // from defaults/`/components=` now (spec R16).
            if !eng.is_interactive() {
                eng.resolve_components(&items);
            }
            eng.push_page(Page::Components { items });
        }
        "finish" => {
            let run = match spec.get::<Option<Table>>("run")? {
                Some(t) => Some(RunTarget {
                    target: t.get::<String>("target")?,
                    label: t.get::<Option<String>>("label")?,
                }),
                None => None,
            };
            let links = parse_links(&spec)?;
            let page = Page::Finish {
                body: spec.get::<Option<String>>("body")?,
                run,
                links,
            };
            engine.borrow_mut().push_page(page);
        }
        // `progress` is implicit (spec R15) — accepted as a no-op so a hand-written
        // script may still name it; unknown kinds fail loud (script bug).
        "progress" => {}
        other => {
            return Err(mlua::Error::runtime(format!(
                "ui.page: unknown kind {other:?}"
            )));
        }
    }
    Ok(())
}

/// Parse a `finish` page's optional `links = { {label=, url=}, ... }` array.
fn parse_links(spec: &Table) -> mlua::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    if let Some(links) = spec.get::<Option<Table>>("links")? {
        for entry in links.sequence_values::<Table>() {
            let entry = entry?;
            out.push((entry.get::<String>("label")?, entry.get::<String>("url")?));
        }
    }
    Ok(out)
}

/// Parse a `components` page's `items = { {...}, ... }` array.
fn parse_items(spec: &Table) -> mlua::Result<Vec<ComponentItem>> {
    let mut out = Vec::new();
    let items = spec.get::<Table>("items")?;
    for entry in items.sequence_values::<Table>() {
        let entry = entry?;
        out.push(ComponentItem {
            id: entry.get::<String>("id")?,
            label: entry.get::<String>("label")?,
            description: entry.get::<Option<String>>("description")?,
            default: entry.get::<Option<bool>>("default")?.unwrap_or(false),
        });
    }
    Ok(out)
}

impl Engine {
    fn push_page(&mut self, page: Page) {
        self.pages.push(page);
    }

    /// `install_dir` as a Lua string (for re-syncing the context global).
    pub(crate) fn install_dir_string(&self) -> String {
        self.install_dir.to_string_lossy().into_owned()
    }

    /// Headless license acceptance (spec R16): under `/S` (and this headless
    /// phase generally) the license is auto-accepted; log the fact for the audit
    /// trail. Phase 5 gates this interactively.
    fn accept_license_silent(&mut self) {
        self.log_line("license accepted (silent)");
    }

    /// Re-point `install_dir` to the directory page's `default` (spec R16). `/D=`
    /// wins, so a locked dir is left untouched; a page with no default is a no-op.
    /// Re-pointing after a mutation already ran is a script bug (hard error).
    fn apply_directory(&mut self, default: Option<&str>) -> Result<(), String> {
        if self.dir_locked {
            return Ok(());
        }
        let Some(default) = default else {
            return Ok(());
        };
        let new = PathBuf::from(default);
        if new == self.install_dir {
            return Ok(());
        }
        if !self.records.is_empty() {
            return Err("ui.page{kind=directory} must run before any install action".to_string());
        }
        self.log_path = new.join("install.log");
        self.install_dir = new;
        Ok(())
    }

    /// Resolve the selected component set: `/components=` if given (ids not
    /// declared by the page are ignored — lenient CLI), else each item's default.
    fn resolve_components(&mut self, items: &[ComponentItem]) {
        self.component_ids = items.iter().map(|i| i.id.clone()).collect();
        self.selected_components = match &self.cli_components {
            Some(list) => list
                .iter()
                .filter(|id| self.component_ids.contains(id.as_str()))
                .cloned()
                .collect(),
            None => items
                .iter()
                .filter(|i| i.default)
                .map(|i| i.id.clone())
                .collect(),
        };
    }

    /// `ui.selected(id)`: whether `id` was chosen. An id never declared by a
    /// components page is a script error (typo or wrong id), surfaced to Lua.
    fn is_selected(&self, id: &str) -> Result<bool, String> {
        if !self.component_ids.contains(id) {
            return Err(format!("ui.selected: unknown component id {id:?}"));
        }
        Ok(self.selected_components.contains(id))
    }
}
