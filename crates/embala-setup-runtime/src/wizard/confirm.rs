//! Uninstall confirm dialog (spec R3) — one modal window shown before any
//! teardown on an interactive (`!/S`) uninstall: a body line naming the install
//! dir, one checkbox per declared uninstall option (pre-ticked per `default`),
//! and "Uninstall" (IDOK, the default button) / "Cancel" (IDCANCEL). Cancel,
//! [X], and Esc all cancel with nothing mutated. Windows-only.
//!
//! Unlike the install wizard this runs on the *calling* thread — the uninstaller
//! shows it synchronously and acts on the result — so there is no UI-thread or
//! channel plumbing: just `run_main` and a shared outcome cell. Layout constants
//! and DPI scaling mirror `wizard/mod.rs` so the two windows look of a piece.

use std::cell::RefCell;
use std::rc::Rc;

use winsafe::{co, gui, prelude::*};

/// One uninstall option to render as a checkbox.
pub struct ConfirmOption {
    pub id: String,
    pub label: String,
    pub default: bool,
}

/// The user's decision from the confirm dialog.
pub enum Outcome {
    /// Cancel / [X] / Esc — nothing must mutate.
    Cancel,
    /// Uninstall clicked; carries the ticked option ids.
    Uninstall(Vec<String>),
}

// Layout in DIP; scaled through gui::dpi at use (mirrors wizard/mod.rs:270-287).
const WIN_W: i32 = 460;
const MARGIN: i32 = 20;
const CONTENT_Y: i32 = 20;
const OPT_Y: i32 = 74;
const OPT_H: i32 = 28;
const BTN_W: i32 = 84;
const BTN_H: i32 = 26;

/// `IDOK` — `IsDialogMessage` maps Enter (the default button) to this id.
const ID_OK: u16 = 1;
/// `IDCANCEL` — `IsDialogMessage` maps Esc to this id.
const ID_CANCEL: u16 = 2;

fn xy(x: i32, y: i32) -> (i32, i32) {
    gui::dpi(x, y)
}

struct Inner {
    wnd: gui::WindowMain,
    // Kept alive for the window's lifetime (winsafe controls own their state).
    _body: gui::Label,
    btn_ok: gui::Button,
    btn_cancel: gui::Button,
    checks: Vec<(String, gui::CheckBox)>,
    outcome: RefCell<Outcome>,
}

#[derive(Clone)]
struct Confirm {
    inner: Rc<Inner>,
}

/// Show the modal confirm dialog and block until the user decides.
pub fn confirm(display_name: &str, install_dir: &str, options: &[ConfirmOption]) -> Outcome {
    let dlg = Confirm::build(display_name, install_dir, options);
    dlg.wire_events();
    // Blocks until a handler posts WM_QUIT (Uninstall / Cancel / [X] / Esc).
    let _ = dlg.inner.wnd.run_main(None);
    dlg.inner.outcome.replace(Outcome::Cancel)
}

impl Confirm {
    fn build(display_name: &str, install_dir: &str, options: &[ConfirmOption]) -> Confirm {
        // Height grows with the option count; the buttons sit a margin below the
        // last checkbox (or the body, when there are no options — R3 degrade).
        let win_h = OPT_Y + options.len() as i32 * OPT_H + 60;

        let wnd = gui::WindowMain::new(gui::WindowMainOpts {
            title: &format!("Uninstall {display_name}"),
            // Baked installer icon (resource id 1) → title bar + taskbar (R10).
            class_icon: gui::Icon::Id(1),
            size: xy(WIN_W, win_h),
            ..Default::default()
        });

        let body = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: &format!("This will remove {display_name} from:\r\n{install_dir}"),
                position: xy(MARGIN, CONTENT_Y),
                size: xy(WIN_W - 2 * MARGIN, 44),
                ..Default::default()
            },
        );

        let mut checks: Vec<(String, gui::CheckBox)> = Vec::new();
        for (i, opt) in options.iter().enumerate() {
            let cb = gui::CheckBox::new(
                &wnd,
                gui::CheckBoxOpts {
                    text: &opt.label,
                    position: xy(MARGIN, OPT_Y + (i as i32) * OPT_H),
                    size: xy(WIN_W - 2 * MARGIN, 24),
                    // Initial check must be set at creation (the hwnd does not
                    // exist yet), mirroring the wizard's components page.
                    check_state: if opt.default {
                        co::BST::CHECKED
                    } else {
                        co::BST::UNCHECKED
                    },
                    ..Default::default()
                },
            );
            checks.push((opt.id.clone(), cb));
        }

        let btn_y = win_h - MARGIN - BTN_H;
        // ctrl_id 1 = IDOK + DEFPUSHBUTTON: Enter confirms and it draws as default.
        let btn_ok = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: "Uninstall",
                position: xy(WIN_W - MARGIN - 2 * BTN_W - 6, btn_y),
                width: gui::dpi_x(BTN_W),
                height: gui::dpi_y(BTN_H),
                ctrl_id: ID_OK,
                control_style: co::BS::DEFPUSHBUTTON,
                ..Default::default()
            },
        );
        // ctrl_id 2 = IDCANCEL: IsDialogMessage turns Esc into WM_COMMAND(IDCANCEL).
        let btn_cancel = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: "Cancel",
                position: xy(WIN_W - MARGIN - BTN_W, btn_y),
                width: gui::dpi_x(BTN_W),
                height: gui::dpi_y(BTN_H),
                ctrl_id: ID_CANCEL,
                ..Default::default()
            },
        );

        Confirm {
            inner: Rc::new(Inner {
                wnd,
                _body: body,
                btn_ok,
                btn_cancel,
                checks,
                outcome: RefCell::new(Outcome::Cancel),
            }),
        }
    }

    fn wire_events(&self) {
        {
            let s = self.clone();
            self.inner.btn_ok.on().bn_clicked(move || {
                s.on_uninstall();
                Ok(())
            });
        }
        // Cancel / [X] / Esc leave the outcome at its default (Cancel); destroying
        // the window fires the default wm_nc_destroy → WM_QUIT and, crucially,
        // removes the dialog from the screen before teardown starts. ([X]/Esc use
        // the default WM_CLOSE → DestroyWindow path, so no wm_close override.)
        {
            let s = self.clone();
            self.inner.btn_cancel.on().bn_clicked(move || {
                let _ = s.inner.wnd.hwnd().DestroyWindow();
                Ok(())
            });
        }
    }

    fn on_uninstall(&self) {
        let selected: Vec<String> = self
            .inner
            .checks
            .iter()
            .filter(|(_, c)| c.is_checked())
            .map(|(id, _)| id.clone())
            .collect();
        // Write the outcome before destroying the window (the destroy quits the
        // loop, after which `confirm` reads this cell).
        *self.inner.outcome.borrow_mut() = Outcome::Uninstall(selected);
        let _ = self.inner.wnd.hwnd().DestroyWindow();
    }
}
