//! Show an elevation prompt on the normal desktop, then go back.
//!
//! When a consent dialog (`consent.exe`, with the secure desktop switched off)
//! opens on a monitor other than the viewed one, the stream moves to that
//! monitor; when the dialog is gone it returns to the monitor the user was on.
//! A monitor the user picks while the dialog is up stays picked. The secure
//! desktop prompt is handled by the capture source (`host_capture.rs`), which
//! drops its temporary monitor itself when the secure desktop ends. See
//! `docs/architecture/follow-active-window.md`.

/// What the caller should do after an [`DialogFollow::observe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Move {
    /// Show `monitor` for the dialog; `restore` is the monitor to go back to.
    Show { monitor: String, restore: String },
    /// The dialog is gone: switch back to this monitor.
    Restore(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Shown {
    monitor: String,
    restore: String,
}

/// Per-session state of the temporary switch.
#[derive(Debug, Default)]
pub struct DialogFollow {
    /// A dialog was seen and not yet gone.
    present: bool,
    /// The monitor this rule switched to, with the one to restore.
    shown: Option<Shown>,
    /// The monitor was changed by someone else during this dialog: leave it.
    declined: bool,
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

impl DialogFollow {
    /// Feed one sample: `dialog` is the capture selector of the monitor that
    /// shows the prompt (`None` when no prompt is up), `current` the selector
    /// of the monitor being streamed.
    pub fn observe(&mut self, dialog: Option<&str>, current: &str) -> Option<Move> {
        let Some(target) = dialog else {
            let shown = self.shown.take();
            let declined = std::mem::take(&mut self.declined);
            self.present = false;
            // Restore only a monitor this rule set and nobody changed since.
            return shown
                .filter(|s| !declined && same(current, &s.monitor))
                .map(|s| Move::Restore(s.restore));
        };
        self.present = true;
        if self.declined {
            return None;
        }
        match &mut self.shown {
            Some(shown) => {
                if !same(current, &shown.monitor) {
                    // Changed behind our back (follow, edge hold, a failed switch).
                    self.declined = true;
                    self.shown = None;
                    return None;
                }
                if same(target, &shown.monitor) {
                    return None;
                }
                // The prompt moved to another monitor: follow it, keep the way back.
                shown.monitor = target.to_owned();
                Some(Move::Show {
                    monitor: target.to_owned(),
                    restore: shown.restore.clone(),
                })
            }
            None => {
                if same(target, current) {
                    return None;
                }
                self.shown = Some(Shown {
                    monitor: target.to_owned(),
                    restore: current.to_owned(),
                });
                Some(Move::Show {
                    monitor: target.to_owned(),
                    restore: current.to_owned(),
                })
            }
        }
    }

    /// The monitor was changed by something other than this rule (the user's
    /// pick, follow-window, edge hold). While a dialog is up that choice stays:
    /// no restore, and no new switch for the same dialog.
    pub fn changed_elsewhere(&mut self) {
        if self.present {
            self.declined = true;
            self.shown = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = r"\.\DISPLAY1";
    const B: &str = r"\.\DISPLAY2";
    const C: &str = r"\.\DISPLAY3";

    fn show(monitor: &str, restore: &str) -> Option<Move> {
        Some(Move::Show {
            monitor: monitor.into(),
            restore: restore.into(),
        })
    }

    #[test]
    fn dialog_on_another_monitor_switches_and_restores() {
        let mut rule = DialogFollow::default();
        assert_eq!(rule.observe(None, A), None);
        assert_eq!(rule.observe(Some(B), A), show(B, A));
        assert_eq!(rule.observe(Some(B), B), None, "no repeat while it is up");
        assert_eq!(rule.observe(None, B), Some(Move::Restore(A.into())));
        assert_eq!(rule.observe(None, A), None);
    }

    #[test]
    fn dialog_on_the_viewed_monitor_changes_nothing() {
        let mut rule = DialogFollow::default();
        assert_eq!(rule.observe(Some(A), A), None);
        assert_eq!(rule.observe(None, A), None);
    }

    #[test]
    fn a_pick_during_the_dialog_is_kept() {
        let mut rule = DialogFollow::default();
        assert_eq!(rule.observe(Some(B), A), show(B, A));
        rule.changed_elsewhere();
        assert_eq!(rule.observe(Some(B), C), None, "no second switch");
        assert_eq!(rule.observe(None, C), None, "no restore");
        // The next dialog starts fresh.
        assert_eq!(rule.observe(Some(B), C), show(B, C));
    }

    #[test]
    fn a_pick_away_from_the_dialog_monitor_is_not_pulled_back() {
        let mut rule = DialogFollow::default();
        assert_eq!(rule.observe(Some(A), A), None);
        rule.changed_elsewhere();
        assert_eq!(rule.observe(Some(A), B), None);
        assert_eq!(rule.observe(None, B), None);
    }

    #[test]
    fn a_dialog_that_moves_keeps_the_original_way_back() {
        let mut rule = DialogFollow::default();
        assert_eq!(rule.observe(Some(B), A), show(B, A));
        assert_eq!(rule.observe(Some(C), B), show(C, A));
        assert_eq!(rule.observe(None, C), Some(Move::Restore(A.into())));
    }
}
