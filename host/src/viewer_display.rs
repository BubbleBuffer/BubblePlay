//! Viewer-scoped display hold and bounded Default-desktop wake policy.
use std::{
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant},
};

const ES_CONTINUOUS: u32 = 0x8000_0000;
const VIEWER_EXECUTION_STATE: u32 = ES_CONTINUOUS | 0x2 | 0x1;
const WAKE_INTERVAL: Duration = Duration::from_secs(2);

// Keep the narrow Win32 binding here: the windows crate's Power feature is
// intentionally not required by this binary-only module.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetThreadExecutionState(flags: u32) -> u32;
}

#[derive(Default)]
struct ExecutionHold {
    held: bool,
}

impl ExecutionHold {
    fn update(&mut self, attached: bool, mut apply: impl FnMut(u32) -> bool) -> bool {
        if self.held == attached {
            return true;
        }
        if !apply(if attached {
            VIEWER_EXECUTION_STATE
        } else {
            ES_CONTINUOUS
        }) {
            return false;
        }
        self.held = attached;
        true
    }
}

/// Created and dropped on the synchronous producer thread. !Send/!Sync keeps
/// the thread-owned execution requirement on that thread for its lifetime.
#[derive(Default)]
pub(super) struct ViewerDisplayHold {
    state: ExecutionHold,
    thread_bound: PhantomData<Rc<()>>,
}

impl ViewerDisplayHold {
    pub(super) fn set_attached(&mut self, attached: bool) {
        if !self.state.update(attached, |flags| unsafe {
            SetThreadExecutionState(flags) != 0
        }) {
            tracing::warn!(attached, "viewer display execution state failed");
        }
    }
}

impl Drop for ViewerDisplayHold {
    fn drop(&mut self) {
        self.set_attached(false);
    }
}

#[derive(Default)]
pub(super) struct DisplayWakePolicy {
    last_attempt: Option<Instant>,
    recovery_pending: bool,
}

impl DisplayWakePolicy {
    pub(super) fn attempt_due(
        &mut self,
        attached: bool,
        unavailable: bool,
        default_desktop: bool,
        now: Instant,
    ) -> bool {
        if !attached {
            *self = Self::default();
            return false;
        }
        if !unavailable
            || !default_desktop
            || self
                .last_attempt
                .is_some_and(|last| now.saturating_duration_since(last) < WAKE_INTERVAL)
        {
            return false;
        }
        self.last_attempt = Some(now);
        self.recovery_pending = true;
        true
    }

    pub(super) fn recovered(&mut self) -> bool {
        std::mem::take(&mut self.recovery_pending)
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_hold_only_changes_on_viewer_transitions() {
        let mut hold = ExecutionHold::default();
        let mut calls = Vec::new();
        for attached in [false, true, true, false, false, true, false] {
            assert!(hold.update(attached, |flags| {
                calls.push(flags);
                true
            }));
            assert_eq!(hold.held, attached);
        }
        assert_eq!(
            calls,
            [
                VIEWER_EXECUTION_STATE,
                ES_CONTINUOUS,
                VIEWER_EXECUTION_STATE,
                ES_CONTINUOUS
            ]
        );
    }

    #[test]
    fn failed_execution_request_does_not_claim_a_hold() {
        let mut hold = ExecutionHold::default();
        assert!(!hold.update(true, |_| false));
        assert!(!hold.held);
        assert!(hold.update(true, |_| true));
        assert!(!hold.update(false, |_| false));
        assert!(hold.held);
        assert!(hold.update(false, |_| true));
    }

    #[test]
    fn wake_requires_viewer_unavailable_default_and_two_second_spacing() {
        let now = Instant::now();
        let mut wake = DisplayWakePolicy::default();
        for (attached, unavailable, default) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
        ] {
            assert!(!wake.attempt_due(attached, unavailable, default, now));
        }
        assert!(wake.attempt_due(true, true, true, now));
        assert!(!wake.attempt_due(true, true, true, now + Duration::from_millis(1999)));
        assert!(wake.attempt_due(true, true, true, now + WAKE_INTERVAL));
        assert!(wake.recovered());
        assert!(!wake.recovered());
        assert!(!wake.attempt_due(true, false, true, now + WAKE_INTERVAL));
        assert!(!wake.attempt_due(false, true, true, now + WAKE_INTERVAL));
        assert!(!wake.recovered());
        assert!(wake.attempt_due(true, true, true, now + WAKE_INTERVAL));
    }
}
