//! The mlua host (spec R10): a curated `StdLib`, an instruction hook wired to the
//! Ctrl+C cancel flag, and script execution. The engine is shared into the API
//! closures via `Rc<RefCell<_>>`; the caller keeps the same `Rc` so it can read
//! the install log for rollback (on error) or the uninstaller (on success).

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{HookTriggers, Lua, LuaOptions, StdLib, VmState};

use crate::api::{self, Engine};

/// How often (in VM instructions) the hook polls the cancel flag. Small enough
/// that Ctrl+C aborts promptly, large enough to be negligible overhead. This is
/// a cancel/runaway poll, not a hard total cap — the author is trusted (R10).
const CANCEL_POLL_INTERVAL: u32 = 10_000;

/// Build the Lua host and run `script` against the shared `engine`.
///
/// `Ok(())` = the script ran to completion (caller writes the uninstaller);
/// `Err(msg)` = a Lua error or a cancel (caller rolls back from `engine`'s log).
/// The internal `Lua` is dropped on return, releasing its clones of `engine`, so
/// the caller's `Rc` is the sole owner afterward.
pub fn run_script(engine: &Rc<RefCell<Engine>>, script: &[u8]) -> Result<(), String> {
    // Curated StdLib (spec R10): TABLE|STRING|MATH|OS. The author is trusted, so
    // this is a runaway/cancel budget, not a security sandbox — but we still omit
    // IO and PACKAGE so filesystem access and module loading flow through the
    // audited embala.* API rather than raw `io`/`require`, and DEBUG (which is
    // `unsafe` in mlua) is never loaded.
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::OS,
        LuaOptions::default(),
    )
    .map_err(|e| e.to_string())?;

    // Instruction hook → cancel flag (spec R10): raising a Lua error here unwinds
    // the script mid-call, which the caller turns into a LIFO rollback.
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(CANCEL_POLL_INTERVAL),
        |_lua, _debug| {
            if crate::sys::is_cancelled() {
                Err(mlua::Error::runtime("install cancelled by user"))
            } else {
                Ok(VmState::Continue)
            }
        },
    )
    .map_err(|e| e.to_string())?;

    api::register(&lua, engine).map_err(|e| e.to_string())?;
    lua.load(script).exec().map_err(|e| e.to_string())
}
