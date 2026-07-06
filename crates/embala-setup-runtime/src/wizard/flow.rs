//! Pure page-flow logic for the interactive wizard (spec R15/R16) — no winsafe,
//! no Win32, so it unit-tests on the Linux host. The winsafe renderer
//! ([`super`], `cfg(windows)`) owns the page *data* types and calls these
//! functions for the parts worth testing without a display: the mode-page
//! relaunch decision, the advance-button label, the license gate, and the
//! resolved component set.
//!
//! Compiled under `cfg(any(windows, test))`: on the host only for `test` (where
//! the unit tests exercise every function), on Windows for the real wizard
//! (which calls every function) — so no configuration leaves one unused.

use std::collections::BTreeSet;

/// What leaving the mode page does (spec R13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeDecision {
    /// Per-machine picked from a non-elevated process → relaunch with
    /// `/mode=per-machine` and let the elevated child re-run the wizard.
    RelaunchElevated,
    /// Continue in this process as per-user.
    ProceedPerUser,
    /// Continue in this (already elevated) process as per-machine.
    ProceedPerMachine,
}

/// Decide what leaving the mode page does (spec R13). Per-machine needs
/// elevation; from a non-elevated process that means relaunching. Per-user never
/// elevates. An already-elevated process (the relaunched child) proceeds
/// per-machine in place.
pub fn mode_decision(machine_selected: bool, elevated: bool) -> ModeDecision {
    match (machine_selected, elevated) {
        (false, _) => ModeDecision::ProceedPerUser,
        (true, true) => ModeDecision::ProceedPerMachine,
        (true, false) => ModeDecision::RelaunchElevated,
    }
}

/// The label of the advance button for the pre-install step at `index` of
/// `count`: the last pre-install page commits the install, so its button reads
/// "Install"; earlier pages read "Next".
pub fn next_label(index: usize, count: usize) -> &'static str {
    if index + 1 >= count {
        "Install"
    } else {
        "Next"
    }
}

/// Whether the advance button may proceed from a license page: a `must_accept`
/// license blocks until the "I accept" box is ticked (spec R15).
pub fn can_advance_license(must_accept: bool, accepted: bool) -> bool {
    !must_accept || accepted
}

/// Resolve the selected component id set from the checkbox states, where
/// `checked[i]` pairs with `ids[i]`. A `BTreeSet` so membership tests
/// (`ui.selected`) are order-independent and de-duplicated.
pub fn selected_components(ids: &[String], checked: &[bool]) -> BTreeSet<String> {
    ids.iter()
        .zip(checked.iter().copied())
        .filter(|(_, c)| *c)
        .map(|(id, _)| id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_machine_from_unelevated_relaunches_not_installs_wrong_hive() {
        // Why: a per-machine install writes Program Files + HKLM, which a
        // non-elevated process cannot do. Proceeding instead of relaunching would
        // fail mid-install (partial state); the decision must be to relaunch.
        assert_eq!(mode_decision(true, false), ModeDecision::RelaunchElevated);
        assert_eq!(mode_decision(true, true), ModeDecision::ProceedPerMachine);
        assert_eq!(mode_decision(false, false), ModeDecision::ProceedPerUser);
        assert_eq!(mode_decision(false, true), ModeDecision::ProceedPerUser);
    }

    #[test]
    fn advance_button_reads_install_only_on_the_last_page() {
        // Why: the user must see "Install" exactly once, on the final pre-install
        // page — that click is the point of no return into the mutation phase.
        assert_eq!(next_label(0, 3), "Next");
        assert_eq!(next_label(1, 3), "Next");
        assert_eq!(next_label(2, 3), "Install");
        // A single-page wizard (welcome only) commits immediately.
        assert_eq!(next_label(0, 1), "Install");
    }

    #[test]
    fn must_accept_license_blocks_until_ticked() {
        // Why: R15 requires the license be accepted before proceeding; a wizard
        // that advanced on an unticked must-accept box would ship an unaccepted
        // license install.
        assert!(!can_advance_license(true, false));
        assert!(can_advance_license(true, true));
        // A non-must-accept (or absent) license never blocks.
        assert!(can_advance_license(false, false));
    }

    #[test]
    fn selected_components_reads_checkbox_states_in_declared_order() {
        // Why: the checkbox order mirrors the declared id order; a mismatch would
        // install the wrong component for a given tick.
        let ids = vec!["core".to_string(), "docs".to_string(), "extra".to_string()];
        let checked = [true, false, true];
        let sel = selected_components(&ids, &checked);
        assert!(sel.contains("core"));
        assert!(!sel.contains("docs"));
        assert!(sel.contains("extra"));
        assert_eq!(sel.len(), 2);
    }

    #[test]
    fn selected_components_empty_when_nothing_checked() {
        let ids = vec!["core".to_string()];
        assert!(selected_components(&ids, &[false]).is_empty());
    }
}
