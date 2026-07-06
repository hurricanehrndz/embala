//! The winsafe wizard renderer (spec R15) — the interactive front-end for the
//! *same* page definitions that drive `/S` headless (R16). Windows-only.
//!
//! ## Window lifecycle: one window, one `run_main`, on a dedicated UI thread
//!
//! Two constraints shape this module:
//!
//! 1. **Every control must exist before the parent window is created** (winsafe
//!    controls hook the parent's `WM_CREATE`; nothing can be added later), and
//!    `WindowMain::run_main` couples window creation with the message loop.
//! 2. The Lua script is **suspended on the stack** inside the wizard gate
//!    ([`crate::api`]'s `gate`) when the wizard first renders — the script cannot
//!    be "started from a button handler"; it is already mid-execution.
//!
//! An earlier revision ran `run_main` on the script thread and tried to keep the
//! window alive across its return (`PostQuitMessage` from the Install handler,
//! then manual pumping); on real Windows the window did not survive, so the
//! progress/finish stages never appeared. This revision removes the keep-alive
//! entirely: [`start`] spawns a **dedicated UI thread** that builds ONE window
//! containing *all* stages (pre-install pages, progress page, and a pre-allocated
//! finish page) and runs `run_main` exactly **once**, posting `WM_QUIT` only when
//! the whole session ends (Finish clicked, cancel, or abort).
//!
//! The script thread never touches a control. It talks to the UI thread through
//! a [`Session`]: an mpsc channel delivers the pre-install outcome (Install +
//! choices / Cancel / Relaunch), and an `Arc<Shared>` carries the progress log
//! lines, the finish-page payload, and a session-state atomic. A 100 ms
//! `SetTimer` tick on the UI thread drains the log into the progress page and
//! reacts to state changes (show finish / quit) — the window stays live while
//! the script runs, with no cross-thread control access at all.
//!
//! The finish page's *content* is only known after the script completes (the
//! generated `install.lua` registers `finish` last, after the mutations), so its
//! controls are pre-allocated generically (body label, run checkbox, up to
//! [`MAX_FINISH_LINKS`] link buttons) and filled in via `SetWindowText` when the
//! engine calls [`Session::finish`].
//!
//! **Keyboard**: winsafe's message loop runs `IsDialogMessage`, so Tab order
//! works natively; the Next/Install/Finish button is `ctrl_id` 1 (`IDOK`) with
//! `BS::DEFPUSHBUTTON` and Cancel is `ctrl_id` 2 (`IDCANCEL`), which maps Enter →
//! advance and Esc → cancel. `on_advance` re-checks the button's enabled state so
//! Enter cannot bypass the license must-accept gate.
//!
//! **DPI**: the baked manifest declares PerMonitorV2; all coordinates go through
//! [`gui::dpi`], which scales by the process's initial DPI. Per-monitor changes
//! mid-session are not re-scaled (v1 limitation — documented).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use winsafe::{self as w, co, gui, prelude::*};

// The pure page-flow logic lives in `wizard/flow.rs`, mounted at crate root as
// `wizard_flow` so its unit tests also build on the (non-winsafe) host.
use crate::wizard_flow::{self as flow, ModeDecision};

/// One pre-install wizard page, as plain data built from `Engine.pages`. `finish`
/// is not a pre-install step (it renders after the install) and `progress` is
/// implicit (R15), so neither appears here.
pub enum Step {
    Welcome { title: String, body: String },
    Mode,
    License { text: String, must_accept: bool },
    Components { items: Vec<CompItem> },
    Directory { default: String, allow_change: bool },
}

/// One entry of a `components` page (label already includes any description).
pub struct CompItem {
    pub id: String,
    pub label: String,
    pub default: bool,
}

/// Plain (winsafe-free, `Send`) description of the wizard to render, built by
/// the engine from `Engine.pages`.
pub struct WizardModel {
    pub display_name: String,
    pub version: String,
    /// Pre-install pages, in display order (finish/progress excluded).
    pub steps: Vec<Step>,
    /// Default install dir for each mode (used to re-point the directory field
    /// when the mode page changes the scope).
    pub default_dir_user: String,
    pub default_dir_machine: String,
    /// The engine's currently resolved mode is per-machine (pre-selects the
    /// mode radio).
    pub current_mode_machine: bool,
    /// This process is elevated (drives the per-machine relaunch decision).
    pub elevated: bool,
    /// `/D=` was given → the directory field is fixed.
    pub dir_locked: bool,
}

/// The finish page's content, delivered by the engine *after* the script
/// completes (the finish page is registered last, past the wizard gate).
pub struct FinishData {
    pub body: Option<String>,
    pub run_label: Option<String>,
    /// Absolute path to launch when the "Run" box is ticked (already resolved
    /// against the final install dir by the engine).
    pub run_target: Option<PathBuf>,
    pub links: Vec<(String, String)>,
}

/// The user's pre-install choices, applied to the engine before mutations run.
pub struct Choices {
    pub install_dir: Option<String>,
    pub mode_machine: bool,
    pub component_ids: Vec<String>,
    pub selected: Vec<String>,
}

/// Result of the pre-install wizard, delivered over the session channel.
pub enum PreOutcome {
    /// User cancelled ([X]/Cancel/Esc) before committing — nothing has mutated.
    Cancel,
    /// Per-machine chosen from a non-elevated process → caller relaunches.
    Relaunch,
    /// User clicked Install; the UI thread has switched to the progress page.
    Install(Choices),
}

// --- session state shared between the script and UI threads ------------------

/// Session running: pre-install pages or the progress page.
const STATE_RUNNING: u8 = 0;
/// Session over: the UI thread should quit (cancel path, script error, or a
/// successful install with no finish page).
const STATE_QUIT: u8 = 1;
/// Install succeeded and a finish page is ready in `Shared::finish`.
const STATE_FINISH: u8 = 2;

struct Shared {
    state: AtomicU8,
    /// Progress lines pushed by the script thread, drained by the UI timer.
    logs: Mutex<Vec<String>>,
    /// The finish-page payload, set before `state` flips to [`STATE_FINISH`].
    finish: Mutex<Option<FinishData>>,
}

/// The script-thread handle to the running wizard session.
pub struct Session {
    shared: Arc<Shared>,
    rx: Receiver<PreOutcome>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// Block until the pre-install wizard concludes (Install/Cancel/Relaunch).
    /// A dead UI thread (panic during window creation) reads as Cancel — nothing
    /// has mutated at that point, so the plain-exit path is correct.
    pub fn wait_preinstall(&self) -> PreOutcome {
        self.rx.recv().unwrap_or(PreOutcome::Cancel)
    }

    /// Queue a progress line (the UI timer drains it into the progress page).
    pub fn push_log(&self, line: &str) {
        if let Ok(mut logs) = self.shared.logs.lock() {
            logs.push(line.to_string());
        }
    }

    /// Install succeeded and the script registered a finish page: hand its
    /// content to the UI thread and block until the user clicks Finish (which
    /// also honours the run-app checkbox) or closes the window.
    pub fn finish(mut self, data: FinishData) {
        if let Ok(mut slot) = self.shared.finish.lock() {
            *slot = Some(data);
        }
        self.shared.state.store(STATE_FINISH, Ordering::Release);
        self.join_ui();
    }

    /// End the session without a finish page (cancel, script error, or a
    /// finish-less success): the UI thread quits on its next timer tick.
    pub fn quit(mut self) {
        self.shared.state.store(STATE_QUIT, Ordering::Release);
        self.join_ui();
    }

    fn join_ui(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Safety net (never the normal path): a dropped-without-quit session must
        // not strand the UI thread in its message loop. Signal quit; do not join
        // (drop may run during unwinding).
        self.shared.state.store(STATE_QUIT, Ordering::Release);
    }
}

/// Spawn the UI thread: build the single wizard window and run its message loop
/// until the session ends. Returns immediately with the [`Session`] handle.
pub fn start(model: WizardModel) -> Session {
    let shared = Arc::new(Shared {
        state: AtomicU8::new(STATE_RUNNING),
        logs: Mutex::new(Vec::new()),
        finish: Mutex::new(None),
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let shared_ui = Arc::clone(&shared);
    let join = std::thread::spawn(move || {
        let wiz = Wizard::build(&model, tx, shared_ui);
        wiz.wire_events();
        // Blocks until WM_QUIT (Finish clicked / cancel / STATE_QUIT tick).
        let _ = wiz.inner.wnd.run_main(None);
    });
    Session {
        shared,
        rx,
        join: Some(join),
    }
}

/// Relaunch this installer elevated for a per-machine install chosen on the mode
/// page (spec R13). Appends `/mode=per-machine` (no `/S`), so the elevated child
/// re-runs the interactive wizard. Waits for the child and returns its exit code.
pub fn relaunch_per_machine() -> std::io::Result<u32> {
    let exe = std::env::current_exe()?;
    crate::sys::relaunch_elevated(&exe, "/mode=per-machine")
}

// --- the UI-thread window -----------------------------------------------------

/// Which stage the window is in (drives the button handlers + `wm_close`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    PreInstall,
    Progress,
    Finish,
}

/// Mutable navigation state, shared into the event closures (UI thread only).
struct Nav {
    idx: usize,
    phase: Phase,
    chosen_machine: bool,
    /// The pre-install outcome has been sent (guards double-sends).
    sent: bool,
    /// Rolling progress counter (the total is unknown; small installs).
    progress_count: u32,
    /// How many queued log lines have been drained into the progress page.
    logs_drained: usize,
    /// The finish payload, taken from `Shared` when entering the finish phase.
    finish: Option<FinishData>,
}

/// Lightweight tag of a step's kind, parallel to the visibility groups, so the
/// Next handler can act without re-reading winsafe controls.
#[derive(Clone, Copy)]
enum StepKind {
    Welcome,
    Mode,
    License { must_accept: bool },
    Components,
    Directory,
}

// Layout in DIP; scaled through gui::dpi at use.
const WIN_W: i32 = 500;
const WIN_H: i32 = 400;
const MARGIN: i32 = 20;
const CONTENT_Y: i32 = 55;
const BTN_W: i32 = 84;
const BTN_H: i32 = 26;
const BTN_Y: i32 = 340;
/// Pre-allocated finish-page link buttons (the real link count is unknown when
/// the window is built); extra links are dropped.
const MAX_FINISH_LINKS: usize = 3;

/// `IDOK` — `IsDialogMessage` maps Enter to a `WM_COMMAND` with this id.
const ID_NEXT: u16 = 1;
/// `IDCANCEL` — `IsDialogMessage` maps Esc to a `WM_COMMAND` with this id.
const ID_CANCEL: u16 = 2;
/// The UI timer that drains progress logs + polls the session state.
const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 100;

fn xy(x: i32, y: i32) -> (i32, i32) {
    gui::dpi(x, y)
}

/// The wizard window and all its controls. Cloneable (an `Rc` handle) so event
/// closures can capture it. Lives entirely on the UI thread.
#[derive(Clone)]
struct Wizard {
    inner: Rc<Inner>,
}

struct Inner {
    wnd: gui::WindowMain,
    heading: gui::Label,
    btn_back: gui::Button,
    btn_next: gui::Button,
    btn_cancel: gui::Button,

    // Pre-install pages, one visibility group per step (parallel to `step_kinds`
    // and `headings`).
    step_groups: Vec<Vec<Box<dyn GuiWindow>>>,
    step_kinds: Vec<StepKind>,
    headings: Vec<String>,

    // Typed handles read on the Install click.
    mode_radio: Option<gui::RadioGroup>,
    license_accept: Option<gui::CheckBox>,
    comp_checks: Vec<(String, gui::CheckBox)>,
    dir_edit: Option<gui::Edit>,
    dir_browse: Option<gui::Button>,

    // Progress page.
    progress_group: Vec<Box<dyn GuiWindow>>,
    progress_bar: gui::ProgressBar,
    progress_status: gui::Label,
    progress_log: gui::ListBox,

    // Finish page (pre-allocated; texts filled from FinishData at finish time).
    finish_group: Vec<Box<dyn GuiWindow>>,
    finish_body: gui::Label,
    finish_run: gui::CheckBox,
    finish_link_btns: Vec<gui::Button>,

    // Session plumbing.
    tx: Sender<PreOutcome>,
    shared: Arc<Shared>,

    display_name: String,
    version: String,
    default_dir_user: String,
    default_dir_machine: String,
    /// `/D=` or `allow_change = false`: the directory field is read-only.
    dir_fixed: bool,
    elevated: bool,
    nav: RefCell<Nav>,
}

impl Wizard {
    fn build(model: &WizardModel, tx: Sender<PreOutcome>, shared: Arc<Shared>) -> Wizard {
        let wnd = gui::WindowMain::new(gui::WindowMainOpts {
            title: &format!("{} Setup", model.display_name),
            // Baked installer icon (resource id 1) → title bar + taskbar (R17).
            class_icon: gui::Icon::Id(1),
            size: xy(WIN_W, WIN_H),
            ..Default::default()
        });

        let heading = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: "",
                position: xy(MARGIN, 15),
                size: xy(WIN_W - 2 * MARGIN, 26),
                ..Default::default()
            },
        );

        let mut step_groups: Vec<Vec<Box<dyn GuiWindow>>> = Vec::new();
        let mut step_kinds: Vec<StepKind> = Vec::new();
        let mut headings: Vec<String> = Vec::new();
        let mut mode_radio = None;
        let mut license_accept = None;
        let mut comp_checks: Vec<(String, gui::CheckBox)> = Vec::new();
        let mut dir_edit = None;
        let mut dir_browse = None;
        // The directory field is fixed by `/D=` (CLI wins) or by the page itself
        // declaring `allow_change = false`.
        let mut dir_fixed = model.dir_locked;

        for step in &model.steps {
            headings.push(step_heading(step, &model.display_name));
            let mut group: Vec<Box<dyn GuiWindow>> = Vec::new();
            match step {
                Step::Welcome { body, .. } => {
                    let lbl = gui::Label::new(
                        &wnd,
                        gui::LabelOpts {
                            text: body,
                            position: xy(MARGIN, CONTENT_Y + 15),
                            size: xy(WIN_W - 2 * MARGIN, 230),
                            ..Default::default()
                        },
                    );
                    group.push(Box::new(lbl));
                    step_kinds.push(StepKind::Welcome);
                }
                Step::Mode => {
                    let prompt = gui::Label::new(
                        &wnd,
                        gui::LabelOpts {
                            text: "Install this software for:",
                            position: xy(MARGIN, CONTENT_Y + 15),
                            size: xy(WIN_W - 2 * MARGIN, 24),
                            ..Default::default()
                        },
                    );
                    let radios = gui::RadioGroup::new(
                        &wnd,
                        &[
                            gui::RadioButtonOpts {
                                text: "Only for me",
                                position: xy(MARGIN + 10, CONTENT_Y + 55),
                                size: xy(WIN_W - 2 * MARGIN - 20, 24),
                                selected: !model.current_mode_machine,
                                ..Default::default()
                            },
                            gui::RadioButtonOpts {
                                text: "For all users (requires administrator)",
                                position: xy(MARGIN + 10, CONTENT_Y + 90),
                                size: xy(WIN_W - 2 * MARGIN - 20, 24),
                                selected: model.current_mode_machine,
                                ..Default::default()
                            },
                        ],
                    );
                    group.push(Box::new(prompt));
                    for rb in radios.iter() {
                        group.push(Box::new(rb.clone()));
                    }
                    mode_radio = Some(radios);
                    step_kinds.push(StepKind::Mode);
                }
                Step::License { text, must_accept } => {
                    // A multiline Edit breaks lines on \r\n only; normalize the
                    // script's \n (idempotently) so the license renders as written.
                    let text = text.replace("\r\n", "\n").replace('\n', "\r\n");
                    let edit = gui::Edit::new(
                        &wnd,
                        gui::EditOpts {
                            text: &text,
                            position: xy(MARGIN, CONTENT_Y),
                            width: gui::dpi_x(WIN_W - 2 * MARGIN),
                            height: gui::dpi_y(200),
                            control_style: co::ES::MULTILINE
                                | co::ES::READONLY
                                | co::ES::AUTOVSCROLL,
                            window_style: co::WS::CHILD
                                | co::WS::VISIBLE
                                | co::WS::BORDER
                                | co::WS::VSCROLL
                                | co::WS::TABSTOP,
                            ..Default::default()
                        },
                    );
                    let accept = gui::CheckBox::new(
                        &wnd,
                        gui::CheckBoxOpts {
                            text: "I accept the terms of the license agreement",
                            position: xy(MARGIN, CONTENT_Y + 215),
                            size: xy(WIN_W - 2 * MARGIN, 24),
                            ..Default::default()
                        },
                    );
                    group.push(Box::new(edit));
                    group.push(Box::new(accept.clone()));
                    license_accept = Some(accept);
                    step_kinds.push(StepKind::License {
                        must_accept: *must_accept,
                    });
                }
                Step::Components { items } => {
                    let prompt = gui::Label::new(
                        &wnd,
                        gui::LabelOpts {
                            text: "Choose the features to install:",
                            position: xy(MARGIN, CONTENT_Y + 5),
                            size: xy(WIN_W - 2 * MARGIN, 22),
                            ..Default::default()
                        },
                    );
                    group.push(Box::new(prompt));
                    for (i, item) in items.iter().enumerate() {
                        // The initial check must be set at creation time via
                        // `check_state` — the control does not exist yet, so a
                        // `set_check` here would be a NULL-hwnd no-op.
                        let cb = gui::CheckBox::new(
                            &wnd,
                            gui::CheckBoxOpts {
                                text: &item.label,
                                position: xy(MARGIN + 5, CONTENT_Y + 35 + (i as i32) * 28),
                                size: xy(WIN_W - 2 * MARGIN - 10, 24),
                                check_state: if item.default {
                                    co::BST::CHECKED
                                } else {
                                    co::BST::UNCHECKED
                                },
                                ..Default::default()
                            },
                        );
                        group.push(Box::new(cb.clone()));
                        comp_checks.push((item.id.clone(), cb));
                    }
                    step_kinds.push(StepKind::Components);
                }
                Step::Directory {
                    default,
                    allow_change,
                } => {
                    if !*allow_change {
                        dir_fixed = true;
                    }
                    let prompt = gui::Label::new(
                        &wnd,
                        gui::LabelOpts {
                            text: "Install to:",
                            position: xy(MARGIN, CONTENT_Y + 15),
                            size: xy(WIN_W - 2 * MARGIN, 22),
                            ..Default::default()
                        },
                    );
                    let edit = gui::Edit::new(
                        &wnd,
                        gui::EditOpts {
                            text: default,
                            position: xy(MARGIN, CONTENT_Y + 45),
                            width: gui::dpi_x(WIN_W - 2 * MARGIN - 100),
                            height: gui::dpi_y(24),
                            ..Default::default()
                        },
                    );
                    let browse = gui::Button::new(
                        &wnd,
                        gui::ButtonOpts {
                            text: "Browse...",
                            position: xy(WIN_W - MARGIN - 88, CONTENT_Y + 44),
                            width: gui::dpi_x(88),
                            height: gui::dpi_y(26),
                            ..Default::default()
                        },
                    );
                    group.push(Box::new(prompt));
                    group.push(Box::new(edit.clone()));
                    group.push(Box::new(browse.clone()));
                    dir_edit = Some(edit);
                    dir_browse = Some(browse);
                    step_kinds.push(StepKind::Directory);
                }
            }
            step_groups.push(group);
        }

        // --- progress page ----------------------------------------------------
        let progress_status = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: "",
                position: xy(MARGIN, CONTENT_Y + 55),
                size: xy(WIN_W - 2 * MARGIN, 22),
                ..Default::default()
            },
        );
        let progress_bar = gui::ProgressBar::new(
            &wnd,
            gui::ProgressBarOpts {
                position: xy(MARGIN, CONTENT_Y + 85),
                size: xy(WIN_W - 2 * MARGIN, 24),
                range: (0, 100),
                ..Default::default()
            },
        );
        let progress_log = gui::ListBox::new(
            &wnd,
            gui::ListBoxOpts {
                position: xy(MARGIN, CONTENT_Y + 125),
                size: xy(WIN_W - 2 * MARGIN, 120),
                ..Default::default()
            },
        );
        let progress_group: Vec<Box<dyn GuiWindow>> = vec![
            Box::new(progress_status.clone()),
            Box::new(progress_bar.clone()),
            Box::new(progress_log.clone()),
        ];

        // --- finish page (pre-allocated; content arrives after the script) -----
        let finish_body = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: "",
                position: xy(MARGIN, CONTENT_Y + 15),
                size: xy(WIN_W - 2 * MARGIN, 100),
                ..Default::default()
            },
        );
        let finish_run = gui::CheckBox::new(
            &wnd,
            gui::CheckBoxOpts {
                text: "",
                // Fixed size: the default (0,0) auto-fits the empty creation text
                // and would render zero-wide for the real label set at finish.
                position: xy(MARGIN, CONTENT_Y + 130),
                size: xy(WIN_W - 2 * MARGIN, 24),
                check_state: co::BST::CHECKED,
                ..Default::default()
            },
        );
        let mut finish_link_btns: Vec<gui::Button> = Vec::new();
        for i in 0..MAX_FINISH_LINKS {
            finish_link_btns.push(gui::Button::new(
                &wnd,
                gui::ButtonOpts {
                    text: "",
                    position: xy(MARGIN, CONTENT_Y + 165 + (i as i32) * 30),
                    width: gui::dpi_x(220),
                    height: gui::dpi_y(24),
                    ..Default::default()
                },
            ));
        }
        let mut finish_group: Vec<Box<dyn GuiWindow>> =
            vec![Box::new(finish_body.clone()), Box::new(finish_run.clone())];
        for b in &finish_link_btns {
            finish_group.push(Box::new(b.clone()));
        }

        // --- buttons ------------------------------------------------------------
        let btn_back = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: "< Back",
                position: xy(WIN_W - MARGIN - 3 * BTN_W - 12, BTN_Y),
                width: gui::dpi_x(BTN_W),
                height: gui::dpi_y(BTN_H),
                ..Default::default()
            },
        );
        // ctrl_id 1 = IDOK: IsDialogMessage turns Enter into WM_COMMAND(IDOK), so
        // Enter advances; DEFPUSHBUTTON draws it as the default button.
        let btn_next = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: "Next >",
                position: xy(WIN_W - MARGIN - 2 * BTN_W - 6, BTN_Y),
                width: gui::dpi_x(BTN_W),
                height: gui::dpi_y(BTN_H),
                ctrl_id: ID_NEXT,
                control_style: co::BS::DEFPUSHBUTTON,
                ..Default::default()
            },
        );
        // ctrl_id 2 = IDCANCEL: IsDialogMessage turns Esc into WM_COMMAND(IDCANCEL).
        let btn_cancel = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: "Cancel",
                position: xy(WIN_W - MARGIN - BTN_W, BTN_Y),
                width: gui::dpi_x(BTN_W),
                height: gui::dpi_y(BTN_H),
                ctrl_id: ID_CANCEL,
                ..Default::default()
            },
        );

        Wizard {
            inner: Rc::new(Inner {
                wnd,
                heading,
                btn_back,
                btn_next,
                btn_cancel,
                step_groups,
                step_kinds,
                headings,
                mode_radio,
                license_accept,
                comp_checks,
                dir_edit,
                dir_browse,
                progress_group,
                progress_bar,
                progress_status,
                progress_log,
                finish_group,
                finish_body,
                finish_run,
                finish_link_btns,
                tx,
                shared,
                display_name: model.display_name.clone(),
                version: model.version.clone(),
                default_dir_user: model.default_dir_user.clone(),
                default_dir_machine: model.default_dir_machine.clone(),
                dir_fixed,
                elevated: model.elevated,
                nav: RefCell::new(Nav {
                    idx: 0,
                    phase: Phase::PreInstall,
                    chosen_machine: model.current_mode_machine,
                    sent: false,
                    progress_count: 0,
                    logs_drained: 0,
                    finish: None,
                }),
            }),
        }
    }

    fn wire_events(&self) {
        // wm_create runs after all controls exist and before the top-level
        // ShowWindow: apply deferred initial state (a build-time hwnd() call is a
        // no-op on the not-yet-created control), show page 0, start the timer.
        {
            let w2 = self.clone();
            self.inner.wnd.on().wm_create(move |_| {
                if w2.inner.dir_fixed {
                    // `/D=` or allow_change=false: render the field read-only.
                    if let Some(edit) = &w2.inner.dir_edit {
                        edit.hwnd().EnableWindow(false);
                    }
                    if let Some(browse) = &w2.inner.dir_browse {
                        browse.hwnd().EnableWindow(false);
                    }
                }
                w2.show_step(0);
                let _ = w2.inner.wnd.hwnd().SetTimer(TIMER_ID, TIMER_MS, None);
                Ok(0)
            });
        }

        // The session timer: drain progress logs, react to state changes.
        {
            let w2 = self.clone();
            self.inner.wnd.on().wm_timer(TIMER_ID, move || {
                w2.on_timer();
                Ok(())
            });
        }

        // [X] — same phase-aware behavior as the Cancel button / Esc.
        {
            let w2 = self.clone();
            self.inner.wnd.on().wm_close(move || {
                w2.on_cancel();
                Ok(())
            });
        }
        {
            let w2 = self.clone();
            self.inner.btn_cancel.on().bn_clicked(move || {
                w2.on_cancel();
                Ok(())
            });
        }

        {
            let w2 = self.clone();
            self.inner.btn_back.on().bn_clicked(move || {
                w2.on_back();
                Ok(())
            });
        }

        // Next / Install / Finish (branches on phase).
        {
            let w2 = self.clone();
            self.inner.btn_next.on().bn_clicked(move || {
                w2.on_next();
                Ok(())
            });
        }

        // License "I accept" toggles the Next button.
        if let Some(accept) = &self.inner.license_accept {
            let w2 = self.clone();
            accept.on().bn_clicked(move || {
                w2.refresh_next_enabled();
                Ok(())
            });
        }

        // Browse button (directory step) → folder picker.
        if let Some(browse) = &self.inner.dir_browse {
            let w2 = self.clone();
            browse.on().bn_clicked(move || {
                w2.on_browse();
                Ok(())
            });
        }

        // Finish-page link buttons → open the URL bound at finish time.
        for (i, btn) in self.inner.finish_link_btns.iter().enumerate() {
            let w2 = self.clone();
            btn.on().bn_clicked(move || {
                let nav = w2.inner.nav.borrow();
                if let Some(data) = &nav.finish {
                    if let Some((_, url)) = data.links.get(i) {
                        shell_open(url);
                    }
                }
                Ok(())
            });
        }
    }

    // --- timer: the script-thread → UI bridge --------------------------------

    fn on_timer(&self) {
        // Drain any queued progress lines into the progress page.
        let new_lines: Vec<String> = {
            let Ok(logs) = self.inner.shared.logs.lock() else {
                return;
            };
            let mut nav = self.inner.nav.borrow_mut();
            let drained = nav.logs_drained;
            nav.logs_drained = logs.len();
            logs[drained..].to_vec()
        };
        for line in &new_lines {
            let _ = self.inner.progress_log.items().add(&[line]);
            if let Ok(count) = self.inner.progress_log.items().count() {
                let _ = self
                    .inner
                    .progress_log
                    .items()
                    .ensure_visible(count.saturating_sub(1));
            }
            let _ = self.inner.progress_status.hwnd().SetWindowText(line);
            let pos = {
                let mut nav = self.inner.nav.borrow_mut();
                nav.progress_count = (nav.progress_count + 4).min(90);
                nav.progress_count
            };
            self.inner.progress_bar.set_position(pos);
        }

        match self.inner.shared.state.load(Ordering::Acquire) {
            STATE_QUIT => w::PostQuitMessage(0),
            STATE_FINISH if self.inner.nav.borrow().phase != Phase::Finish => {
                let data = self
                    .inner
                    .shared
                    .finish
                    .lock()
                    .ok()
                    .and_then(|mut slot| slot.take());
                match data {
                    Some(data) => self.enter_finish(data),
                    // State says finish but no payload — quit over hanging.
                    None => w::PostQuitMessage(0),
                }
            }
            _ => {}
        }
    }

    // --- pre-install navigation ------------------------------------------------

    /// Show pre-install step `idx`, hide everything else, and update the heading
    /// + buttons.
    fn show_step(&self, idx: usize) {
        self.hide_all_groups();
        {
            let mut nav = self.inner.nav.borrow_mut();
            nav.idx = idx;
            nav.phase = Phase::PreInstall;
        }
        set_group(&self.inner.step_groups[idx], true);

        let _ = self
            .inner
            .heading
            .hwnd()
            .SetWindowText(&self.inner.headings[idx]);

        show_ctrl(self.inner.btn_back.hwnd(), true);
        show_ctrl(self.inner.btn_next.hwnd(), true);
        show_ctrl(self.inner.btn_cancel.hwnd(), true);
        self.inner.btn_back.hwnd().EnableWindow(idx > 0);
        let label = flow::next_label(idx, self.inner.step_kinds.len());
        let _ = self.inner.btn_next.hwnd().SetWindowText(label);
        self.refresh_next_enabled();
    }

    /// Enable/disable Next per the current step (license must-accept gate).
    fn refresh_next_enabled(&self) {
        let idx = self.inner.nav.borrow().idx;
        let enabled = match self.inner.step_kinds.get(idx) {
            Some(StepKind::License { must_accept }) => {
                let accepted = self
                    .inner
                    .license_accept
                    .as_ref()
                    .map(|c| c.is_checked())
                    .unwrap_or(true);
                flow::can_advance_license(*must_accept, accepted)
            }
            _ => true,
        };
        self.inner.btn_next.hwnd().EnableWindow(enabled);
    }

    fn on_back(&self) {
        if self.inner.nav.borrow().phase != Phase::PreInstall {
            return;
        }
        let idx = self.inner.nav.borrow().idx;
        if idx > 0 {
            self.show_step(idx - 1);
        }
    }

    fn on_next(&self) {
        let phase = self.inner.nav.borrow().phase;
        match phase {
            Phase::Progress => {}
            Phase::Finish => self.on_finish_clicked(),
            Phase::PreInstall => self.on_advance(),
        }
    }

    fn on_advance(&self) {
        // Enter reaches here via WM_COMMAND(IDOK) even when the button is
        // disabled — re-check so the license must-accept gate holds (R15).
        if !self.inner.btn_next.hwnd().IsWindowEnabled() {
            return;
        }
        let idx = self.inner.nav.borrow().idx;
        let kind = self.inner.step_kinds[idx];

        // Mode page: decide per-machine relaunch / scope before advancing.
        if let StepKind::Mode = kind {
            let machine = self
                .inner
                .mode_radio
                .as_ref()
                .and_then(|r| r.selected_index())
                .map(|i| i == 1)
                .unwrap_or(false);
            match flow::mode_decision(machine, self.inner.elevated) {
                ModeDecision::RelaunchElevated => {
                    self.send_outcome(PreOutcome::Relaunch);
                    w::PostQuitMessage(0);
                    return;
                }
                ModeDecision::ProceedPerMachine => {
                    self.inner.nav.borrow_mut().chosen_machine = true;
                    self.repoint_dir(true);
                }
                ModeDecision::ProceedPerUser => {
                    self.inner.nav.borrow_mut().chosen_machine = false;
                    self.repoint_dir(false);
                }
            }
        }

        if idx + 1 >= self.inner.step_kinds.len() {
            // Install: switch this window to the progress page (the message loop
            // keeps running) and hand the choices to the script thread.
            let choices = self.read_choices();
            self.enter_progress();
            self.send_outcome(PreOutcome::Install(choices));
        } else {
            self.show_step(idx + 1);
        }
    }

    fn on_cancel(&self) {
        let phase = self.inner.nav.borrow().phase;
        match phase {
            Phase::PreInstall => {
                // Nothing has mutated: report cancel and end the session.
                self.send_outcome(PreOutcome::Cancel);
                w::PostQuitMessage(0);
            }
            Phase::Progress => {
                // Signal the running script to abort; the Lua instruction hook
                // picks it up, rollback runs, and the engine ends the session
                // (STATE_QUIT → the timer quits). Do not quit mid-mutation here.
                crate::sys::request_cancel();
            }
            Phase::Finish => {
                // Close without running the app.
                w::PostQuitMessage(0);
            }
        }
    }

    fn on_finish_clicked(&self) {
        // Capture the run choice while the window is alive (we are inside the
        // Finish click handler, so the checkbox state is authoritative here).
        let run = {
            let nav = self.inner.nav.borrow();
            let wanted = self.inner.finish_run.hwnd().IsWindowVisible()
                && self.inner.finish_run.is_checked();
            nav.finish
                .as_ref()
                .and_then(|d| d.run_target.clone())
                .filter(|_| wanted)
        };
        // Launch BEFORE posting quit: the UI thread (and shortly after, the
        // process) exits right behind this handler, and only a synchronous
        // (`SEE_MASK::NOASYNC`) launch survives that. Failures are loud.
        if let Some(path) = run {
            if let Err(e) = launch_app(&path) {
                let _ = self.inner.wnd.hwnd().MessageBox(
                    &format!("Setup could not start {}:\n{e}", path.display()),
                    "embala setup",
                    co::MB::OK | co::MB::ICONWARNING,
                );
            }
        }
        w::PostQuitMessage(0);
    }

    fn send_outcome(&self, outcome: PreOutcome) {
        let mut nav = self.inner.nav.borrow_mut();
        if !nav.sent {
            nav.sent = true;
            let _ = self.inner.tx.send(outcome);
        }
    }

    /// Re-point the directory field to the chosen mode's default (unless fixed
    /// by `/D=`/`allow_change = false`). The mode page precedes the directory
    /// page, so overwriting is safe — the user has not seen the field yet.
    fn repoint_dir(&self, machine: bool) {
        if self.inner.dir_fixed {
            return;
        }
        if let Some(edit) = &self.inner.dir_edit {
            let dir = if machine {
                &self.inner.default_dir_machine
            } else {
                &self.inner.default_dir_user
            };
            let _ = edit.hwnd().SetWindowText(dir);
        }
    }

    fn on_browse(&self) {
        let Some(edit) = &self.inner.dir_edit else {
            return;
        };
        if let Some(dir) = pick_folder(self.inner.wnd.hwnd()) {
            let _ = edit.hwnd().SetWindowText(&dir);
        }
    }

    fn read_choices(&self) -> Choices {
        let nav = self.inner.nav.borrow();
        let install_dir = self
            .inner
            .dir_edit
            .as_ref()
            .and_then(|e| e.text().ok())
            .filter(|s| !s.trim().is_empty());
        let ids: Vec<String> = self
            .inner
            .comp_checks
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        let checked: Vec<bool> = self
            .inner
            .comp_checks
            .iter()
            .map(|(_, c)| c.is_checked())
            .collect();
        let selected = flow::selected_components(&ids, &checked)
            .into_iter()
            .collect();
        Choices {
            install_dir,
            mode_machine: nav.chosen_machine,
            component_ids: ids,
            selected,
        }
    }

    // --- progress + finish stages ----------------------------------------------

    /// Switch to the progress page: hide the pre-install UI, keep Cancel visible
    /// (it now requests a script abort), show the progress controls.
    fn enter_progress(&self) {
        self.inner.nav.borrow_mut().phase = Phase::Progress;
        self.hide_all_groups();
        show_ctrl(self.inner.btn_back.hwnd(), false);
        show_ctrl(self.inner.btn_next.hwnd(), false);
        show_ctrl(self.inner.btn_cancel.hwnd(), true);
        let _ = self.inner.heading.hwnd().SetWindowText(&format!(
            "Installing {} {}",
            self.inner.display_name, self.inner.version
        ));
        set_group(&self.inner.progress_group, true);
    }

    /// Switch to the finish page, filling the pre-allocated controls from the
    /// engine-supplied [`FinishData`].
    fn enter_finish(&self, data: FinishData) {
        self.inner.progress_bar.set_position(100);
        self.hide_all_groups();

        let body = data
            .body
            .clone()
            .unwrap_or_else(|| format!("{} has been installed.", self.inner.display_name));
        let _ = self.inner.finish_body.hwnd().SetWindowText(&body);
        show_ctrl(self.inner.finish_body.hwnd(), true);

        if data.run_target.is_some() {
            let label = data
                .run_label
                .clone()
                .unwrap_or_else(|| format!("Run {}", self.inner.display_name));
            let _ = self.inner.finish_run.hwnd().SetWindowText(&label);
            show_ctrl(self.inner.finish_run.hwnd(), true);
        }
        for (i, btn) in self.inner.finish_link_btns.iter().enumerate() {
            if let Some((label, _)) = data.links.get(i) {
                let _ = btn.hwnd().SetWindowText(label);
                show_ctrl(btn.hwnd(), true);
            }
        }

        let _ = self
            .inner
            .heading
            .hwnd()
            .SetWindowText(&format!("{} Setup Complete", self.inner.display_name));
        show_ctrl(self.inner.btn_back.hwnd(), false);
        show_ctrl(self.inner.btn_cancel.hwnd(), false);
        show_ctrl(self.inner.btn_next.hwnd(), true);
        self.inner.btn_next.hwnd().EnableWindow(true);
        let _ = self.inner.btn_next.hwnd().SetWindowText("Finish");

        let mut nav = self.inner.nav.borrow_mut();
        nav.finish = Some(data);
        nav.phase = Phase::Finish;
    }

    fn hide_all_groups(&self) {
        for g in &self.inner.step_groups {
            set_group(g, false);
        }
        set_group(&self.inner.progress_group, false);
        set_group(&self.inner.finish_group, false);
    }
}

// --- free helpers -------------------------------------------------------------

/// Heading text for a pre-install step. Welcome honours a script-supplied
/// `title`; the rest are fixed captions.
fn step_heading(step: &Step, display_name: &str) -> String {
    match step {
        Step::Welcome { title, .. } if !title.is_empty() => title.clone(),
        Step::Welcome { .. } => format!("Welcome to {display_name} Setup"),
        Step::Mode => "Choose Installation Scope".to_string(),
        Step::License { .. } => "License Agreement".to_string(),
        Step::Components { .. } => "Select Components".to_string(),
        Step::Directory { .. } => "Choose Install Location".to_string(),
    }
}

fn set_group(group: &[Box<dyn GuiWindow>], show: bool) {
    for c in group {
        show_ctrl(c.hwnd(), show);
    }
}

fn show_ctrl(hwnd: &w::HWND, show: bool) {
    hwnd.ShowWindow(if show { co::SW::SHOW } else { co::SW::HIDE });
}

/// Open a finish-page link URL with the default verb, best-effort. Only safe
/// because the window (and its message loop) stays alive after a link click; the
/// run-app-at-exit path must use [`launch_app`] instead.
fn shell_open(target: &str) {
    let _ = w::ShellExecuteEx(&w::SHELLEXECUTEINFO {
        file: target,
        show: co::SW::SHOWNORMAL,
        ..Default::default()
    });
}

/// Launch the installed app from the finish page (spec R15 run-app).
///
/// `SEE_MASK::NOASYNC` is required: without it `ShellExecuteEx` may delegate the
/// launch asynchronously, and the UI thread — which exits immediately after the
/// Finish click, followed by the process — silently drops it (observed on the
/// VM: Finish closed, hello.exe never ran). NOASYNC makes the call synchronous,
/// so on return the child process exists and survives our exit. COM is
/// initialized around the call per the `ShellExecuteEx` docs, and the app gets
/// its own directory as the working dir. Errors are returned for loud reporting.
fn launch_app(path: &std::path::Path) -> Result<(), String> {
    let _com = w::CoInitializeEx(co::COINIT::APARTMENTTHREADED | co::COINIT::DISABLE_OLE1DDE)
        .map_err(|e| e.to_string())?;
    let file = path.to_string_lossy();
    let directory = path.parent().map(|p| p.to_string_lossy().into_owned());
    w::ShellExecuteEx(&w::SHELLEXECUTEINFO {
        mask: co::SEE_MASK::NOASYNC,
        file: &file,
        directory: directory.as_deref(),
        show: co::SW::SHOWNORMAL,
        ..Default::default()
    })
    .map_err(|e| e.to_string())
}

/// Native folder picker (spec R15 directory page) via `IFileOpenDialog` with
/// `FOS::PICKFOLDERS` — the modern replacement for `SHBrowseForFolder`, using the
/// already-enabled shell/ole features. Returns the chosen filesystem path, or
/// `None` if the user cancelled or the dialog could not be shown.
fn pick_folder(owner: &w::HWND) -> Option<String> {
    let _com =
        w::CoInitializeEx(co::COINIT::APARTMENTTHREADED | co::COINIT::DISABLE_OLE1DDE).ok()?;
    let dlg = w::CoCreateInstance::<w::IFileOpenDialog>(
        &co::CLSID::FileOpenDialog,
        None::<&w::IUnknown>,
        co::CLSCTX::INPROC_SERVER,
    )
    .ok()?;
    let opts = dlg.GetOptions().ok()?;
    dlg.SetOptions(opts | co::FOS::PICKFOLDERS | co::FOS::FORCEFILESYSTEM)
        .ok()?;
    if dlg.Show(owner).ok()? {
        dlg.GetResult()
            .ok()?
            .GetDisplayName(co::SIGDN::FILESYSPATH)
            .ok()
    } else {
        None
    }
}
