//! Lower a declarative `[setup]` section to a generated `install.lua` over the
//! `embala.*` API (spec R19). This is the "80% path": authors describe the
//! install declaratively and embala emits the same script a hand author would,
//! so there is one runtime substrate, not two engines.
//!
//! The output is **deterministic** (config-order iteration, no timestamps) and
//! **human-readable plaintext** (spec R7): it ships verbatim in the overlay and
//! is auditable without extraction, so it carries a header saying it was
//! generated. The license text is embedded as an escaped Lua string rather than
//! shipped as a payload file — self-contained and auditable in one artifact.
//!
//! When `[setup].script` is set the raw script ships instead and lowering is
//! skipped (`build_setup` decides); this module only handles the sugar path.

use crate::config::{Component, Package, SetupSection, Shortcut, ShortcutLocation};

/// Generate the `install.lua` for a declarative `[setup]`. `license_text` is the
/// contents of `[setup].license` read at build time (config-dir-relative), or
/// `None` when no license is configured.
pub fn lower(package: &Package, setup: &SetupSection, license_text: Option<&str>) -> String {
    let mut out = String::new();

    out.push_str(HEADER);
    // Audit line (spec R7): the identity this installer was generated for. The
    // fields are parse-validated to be single-line and safe, so no escaping.
    out.push_str(&format!(
        "-- Package: {} {} ({})\n",
        package.name, package.version, package.identifier
    ));
    out.push_str("\nlocal pkg = embala.package\n");

    // --- Pages, in wizard order --------------------------------------------
    // user-choice prepends a mode-selection page; fixed modes take the mode from
    // the manifest and emit nothing here (spec R13, mapping table).
    if setup.install_mode == crate::config::InstallMode::UserChoice {
        out.push_str("\nembala.ui.page{ kind = \"mode\" }\n");
    }

    // welcome: title/body from the package identity (referenced, not embedded).
    out.push_str("\nembala.ui.page{ kind = \"welcome\", title = pkg.display_name, body = pkg.description }\n");

    // license: embed the text as an escaped Lua string (auditable, self-contained).
    if let Some(text) = license_text {
        out.push_str("\nembala.ui.page{ kind = \"license\", must_accept = true, text = ");
        out.push_str(&lua_str(text));
        out.push_str(" }\n");
    }

    // directory: default is the already-resolved install_dir, so the no-flag path
    // is a no-op; a /D= flag still wins at install time (spec R16).
    out.push_str(
        "\nembala.ui.page{ kind = \"directory\", default = embala.install_dir, allow_change = true }\n",
    );

    // components: register the checklist; gated copies follow below.
    if !setup.components.is_empty() {
        out.push_str("\nembala.ui.page{\n  kind = \"components\",\n  items = {\n");
        for c in &setup.components {
            out.push_str("    ");
            out.push_str(&component_item(c));
            out.push_str(",\n");
        }
        out.push_str("  },\n}\n");
    }

    // --- Actions ------------------------------------------------------------
    // top-level files: always installed. At install time the payload zip is keyed
    // by dest, so the API `src` (relative to payload_dir) equals the dest.
    out.push_str("\n-- Payload files (always installed).\n");
    for f in &setup.files {
        out.push_str(&format!(
            "embala.fs.copy({}, {})\n",
            lua_str(&f.dest),
            lua_str(&f.dest)
        ));
    }

    // component files: gated on ui.selected(id) (headless: from default/`/components=`).
    for c in &setup.components {
        if c.files.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "\n-- Component {} (installed when selected).\nif embala.ui.selected({}) then\n",
            lua_str(&c.id),
            lua_str(&c.id)
        ));
        for f in &c.files {
            out.push_str(&format!(
                "  embala.fs.copy({}, {})\n",
                lua_str(&f.dest),
                lua_str(&f.dest)
            ));
        }
        out.push_str("end\n");
    }

    // main-executable: a start-menu shortcut named after the package.
    if let Some(main) = &setup.main_executable {
        out.push_str(&format!(
            "\n-- Start-menu shortcut for the main executable.\nembala.shortcut.create{{ name = pkg.display_name, target = {}, location = \"start-menu\", description = pkg.description }}\n",
            lua_str(main)
        ));
    }

    // explicit shortcuts, in config order.
    if !setup.shortcuts.is_empty() {
        out.push_str("\n-- Shortcuts.\n");
        for s in &setup.shortcuts {
            out.push_str(&shortcut_call(s));
        }
    }

    // ARP registration from the package identity + the resolved install_dir.
    out.push_str("\n-- Add/Remove Programs registration.\nembala.arp.register{\n");
    out.push_str("  display_name = pkg.display_name,\n");
    out.push_str("  version = pkg.version,\n");
    out.push_str("  publisher = pkg.publisher,\n");
    out.push_str("  install_location = embala.install_dir,\n");
    out.push_str("  uninstall_string = '\"' .. embala.install_dir .. '\\\\uninstall.exe\"',\n");
    if let Some(main) = &setup.main_executable {
        out.push_str(&format!(
            "  display_icon = embala.install_dir .. '\\\\' .. {},\n",
            lua_str(&main.replace('/', "\\"))
        ));
    }
    out.push_str("}\n");

    // finish: offers to run the main executable when present (mapping table).
    match &setup.main_executable {
        Some(main) => out.push_str(&format!(
            "\nembala.ui.page{{ kind = \"finish\", run = {{ target = {} }} }}\n",
            lua_str(main)
        )),
        None => out.push_str("\nembala.ui.page{ kind = \"finish\" }\n"),
    }

    out
}

const HEADER: &str = "\
-- Generated by embala from [setup]; do not edit by hand.
-- Declarative TOML lowered to the embala.* installer API (spec R19). Change the
-- installer by editing [setup] in embala.toml and rebuilding, or ship a raw
-- script via [setup].script.
";

/// One `{ id=, label=, description=?, default= }` component-list entry.
fn component_item(c: &Component) -> String {
    let mut s = format!("{{ id = {}, label = {}", lua_str(&c.id), lua_str(&c.label));
    if let Some(desc) = &c.description {
        s.push_str(&format!(", description = {}", lua_str(desc)));
    }
    s.push_str(&format!(", default = {} }}", c.default));
    s
}

/// One `embala.shortcut.create{...}` call for an explicit `[setup].shortcuts` entry.
fn shortcut_call(s: &Shortcut) -> String {
    let location = match s.location {
        ShortcutLocation::StartMenu => "start-menu",
        ShortcutLocation::Desktop => "desktop",
    };
    let mut call = format!(
        "embala.shortcut.create{{ name = {}, target = {}, location = {}",
        lua_str(&s.name),
        lua_str(&s.target),
        lua_str(location)
    );
    if let Some(args) = &s.args {
        call.push_str(&format!(", args = {}", lua_str(args)));
    }
    if let Some(icon) = &s.icon {
        call.push_str(&format!(", icon = {}", lua_str(icon)));
    }
    call.push_str(" }\n");
    call
}

/// Escape an arbitrary string into a double-quoted Lua string literal. Handles
/// quotes, backslashes, and the common whitespace controls with letter escapes;
/// other control bytes use the zero-padded `\ddd` numeric form (padded so a
/// following digit cannot merge into the escape). Non-ASCII passes through as
/// its UTF-8 bytes, which Lua string literals accept verbatim.
pub fn lua_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn fixtures_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/hello"))
    }

    fn hello_config() -> Config {
        let text = std::fs::read_to_string(fixtures_dir().join("embala.toml")).expect("fixture");
        toml::from_str(&text).expect("fixture parses")
    }

    #[test]
    fn generated_install_lua_matches_golden() {
        // Why: the generated install.lua ships verbatim in the overlay as a
        // user-auditable artifact (spec R7). Any drift here is a silent change to
        // installer behavior — the golden makes such a change a failing test the
        // author must consciously re-bless, not a surprise on the user's machine.
        let config = hello_config();
        let setup = config.setup.as_ref().expect("fixture has [setup]");
        let license = std::fs::read_to_string(fixtures_dir().join("license.txt")).expect("license");
        let generated = lower(&config.package, setup, Some(&license));
        let golden = std::fs::read_to_string(fixtures_dir().join("install.lua.expected"))
            .expect("golden install.lua.expected");
        assert_eq!(generated, golden);
    }

    #[test]
    fn lua_str_escapes_quotes_backslash_newline_and_passes_unicode() {
        // Why: the license text (and every config-derived literal) is embedded
        // into the shipped script; a missed escape would produce a script that
        // fails to parse on the user's machine — or worse, silently truncates the
        // license at an embedded quote. Unicode must survive so non-ASCII license
        // text renders correctly.
        assert_eq!(lua_str("plain"), "\"plain\"");
        assert_eq!(lua_str("a\"b"), "\"a\\\"b\"");
        assert_eq!(lua_str("a\\b"), "\"a\\\\b\"");
        assert_eq!(lua_str("line1\nline2"), "\"line1\\nline2\"");
        assert_eq!(lua_str("tab\there"), "\"tab\\there\"");
        // A bare control byte followed by a digit must stay unambiguous.
        assert_eq!(lua_str("\u{1}5"), "\"\\0015\"");
        // Non-ASCII passes through verbatim (UTF-8 bytes are valid in Lua strings).
        assert_eq!(lua_str("café — ☕"), "\"café — ☕\"");
    }

    #[test]
    fn component_files_are_gated_top_level_files_are_not() {
        // Why: the whole point of components is optional install. A top-level file
        // must copy unconditionally; a component file must sit inside an
        // `if embala.ui.selected(id)` guard so an unselected component installs
        // nothing.
        let config = hello_config();
        let setup = config.setup.as_ref().unwrap();
        let out = lower(&config.package, setup, None);
        // Top-level hello.exe copies unconditionally (not inside a selected() guard).
        assert!(out.contains(
            "-- Payload files (always installed).\nembala.fs.copy(\"hello.exe\", \"hello.exe\")\n"
        ));
        // The docs component's readme is gated.
        assert!(out.contains("if embala.ui.selected(\"docs\") then\n  embala.fs.copy(\"readme.txt\", \"readme.txt\")\nend\n"));
    }

    #[test]
    fn user_choice_prepends_a_mode_page_fixed_mode_does_not() {
        // Why: user-choice must let the runtime pick the hive/dir via a mode page;
        // a fixed mode takes it from the manifest and must NOT emit a mode page
        // (which would imply a choice the author did not offer).
        let config = hello_config();
        let mut setup_toml: SetupSection = toml::from_str(
            "arch = \"x86_64\"\nfiles = [{ src = \"dist/hello.exe\", dest = \"hello.exe\" }]",
        )
        .unwrap();
        // fixed (default per-user): no mode page.
        assert!(!lower(&config.package, &setup_toml, None).contains("kind = \"mode\""));
        setup_toml.install_mode = crate::config::InstallMode::UserChoice;
        assert!(
            lower(&config.package, &setup_toml, None).contains("embala.ui.page{ kind = \"mode\" }")
        );
    }
}
