//! Live monitor and display-mode controls shared by the UI thread and the net
//! worker. The UI calls the nonblocking methods on [`SessionControls`]. The net
//! thread applies the host's `monitor-list` and `monitor-error` messages and
//! writes queued `media-control` commands to the `/ws` socket, after releasing
//! the lock. Mirrors `handleMonitorList`, `requestMonitor` and `sendStreamMode`
//! in `worker/public/receiver.js`.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::model::StreamMode;

/// Longest list the browser accepts; a longer `monitor-list` is refused whole.
pub const MAX_MONITORS: usize = 16;
/// Longest selector the browser accepts, counted in UTF-16 code units as `String.length` does.
pub const MAX_SELECTOR_CHARS: usize = 128;
/// Longest `monitor-error` reason kept, in UTF-16 code units, as `handleMonitorError` slices it.
pub const MAX_ERROR_CHARS: usize = 180;

/// The shell's wake-up, called after a change to what [`SessionControls::snapshot`] reports.
/// It runs with no lock held, so it may call back into the controls.
pub type Notifier = Arc<dyn Fn() + Send + Sync>;

/// One display the host reported. `follow` in the same message is not kept here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorEntry {
    pub selector: String,
    /// The host's name, or the selector when the host sent no string name (as `handleMonitorList`).
    pub name: String,
    /// 0 when the host sent no usable number.
    pub width: u32,
    pub height: u32,
    /// Position in the host desktop (`origin_x`, `origin_y` on the wire).
    pub origin_x: i32,
    pub origin_y: i32,
    pub selected: bool,
}

/// The host's last `monitor-error`: the selector it refused and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorError {
    /// `None` when the host sent no usable selector.
    pub selector: Option<String>,
    pub reason: String,
}

/// A copy of the shared state for the UI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ControlSnapshot {
    /// The last valid `monitor-list`; empty before the first one.
    pub monitors: Vec<MonitorEntry>,
    /// The selector flagged `selected` in that list.
    pub selected: Option<String>,
    /// A `user-monitor` request waiting for a list, an error or an epoch restart.
    /// Any valid list or error clears it, including a pick that was not yet written.
    pub pending: Option<String>,
    /// The last `monitor-error`. The next valid list clears it.
    pub error: Option<MonitorError>,
    pub mode: StreamMode,
    /// The default applied on the first list of the run. A valid user pick changes it in memory.
    pub default_monitor: Option<String>,
    /// Bumped after each live `mode` command is written, so the UI can resend cursor sync.
    pub mode_revision: u64,
    /// Whether `/ws` has answered `hello-ok` on the current epoch.
    pub connected: bool,
}

/// Result of [`SessionControls::pick_monitor`]. Nothing is sent unless it is `Queued`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickOutcome {
    /// Pending is set; the net thread writes the command on its next tick.
    Queued,
    /// The selector is already the host's selection; no command.
    AlreadySelected,
    /// Another pick is still waiting for a list, an error or an epoch restart.
    Pending,
    /// `/ws` is not registered; nothing is stored.
    NotConnected,
    /// The selector is not in the last valid list.
    UnknownMonitor,
}

/// Result of [`SessionControls::set_mode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeOutcome {
    /// A live socket: the command is written on the next net tick. The latest target wins.
    Queued,
    /// Already this mode; no command.
    Unchanged,
    /// No live socket: stored, and sent in `signaling-start` on the next connect.
    Stored,
}

/// Commands the net thread has not written yet. Each one is bound to the wire
/// epoch it was queued for.
#[derive(Debug, Default)]
struct Outbox {
    /// The epoch, the target and the request number it was set under.
    mode: Option<(String, StreamMode, u64)>,
    pick: Option<(String, String)>,
}

/// The notifier, held in the state. Its `Debug` output hides the closure.
struct Hook(Notifier);

impl fmt::Debug for Hook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Notifier")
    }
}

#[derive(Debug, Default)]
struct State {
    monitors: Vec<MonitorEntry>,
    pending: Option<String>,
    error: Option<MonitorError>,
    default_monitor: Option<String>,
    /// Set by the first valid list of the run, and never cleared.
    default_applied: bool,
    mode: StreamMode,
    /// Whether a mode was set or seeded. An unset mode leaves `NetConfig.stream_mode` alone.
    mode_explicit: bool,
    mode_revision: u64,
    /// Numbers each accepted mode change. Internal only; never sent on the wire.
    mode_request: u64,
    /// The newest request number that is on the wire, or that the start mode
    /// of a registered socket covers. Internal only.
    mode_acked: u64,
    /// Wire epoch of the registered `/ws` socket; `None` before `hello-ok` and between attempts.
    epoch: Option<String>,
    outbox: Outbox,
    hook: Option<Hook>,
}

/// Monitor and mode controls. Clone it into `NetConfig.controls` and the UI;
/// all clones share one state. Every method is nonblocking and never touches
/// the socket.
#[derive(Clone, Debug, Default)]
pub struct SessionControls {
    inner: Arc<Mutex<State>>,
}

/// Commands the net thread takes for writing, already filtered to its epoch.
#[derive(Debug, Default)]
pub(crate) struct Outbound {
    pub(crate) mode: Option<StreamMode>,
    /// The request number of `mode`; pass it to `mark_mode_written` once written.
    pub(crate) mode_request: u64,
    pub(crate) pick: Option<String>,
}

impl SessionControls {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run `change` under the state lock. If it reports a change, clone the
    /// notifier, release the lock, and only then call the notifier.
    fn update<T>(&self, change: impl FnOnce(&mut State) -> (T, bool)) -> T {
        let (value, notify) = {
            let mut state = self.state();
            let (value, changed) = change(&mut state);
            let notify = match (changed, &state.hook) {
                (true, Some(hook)) => Some(hook.0.clone()),
                _ => None,
            };
            (value, notify)
        };
        if let Some(notify) = notify {
            notify();
        }
        value
    }

    /// Register the shell's wake-up. It runs after each change to what
    /// [`snapshot`](Self::snapshot) reports: monitors, selection, pending, error,
    /// mode, mode revision and epoch. It runs on the thread that made the change,
    /// with no lock held, so it may call `snapshot` or any other method here.
    pub fn set_notifier(&self, notifier: Notifier) {
        self.state().hook = Some(Hook(notifier));
    }

    /// A copy of the state for the UI thread.
    pub fn snapshot(&self) -> ControlSnapshot {
        let state = self.state();
        ControlSnapshot {
            monitors: state.monitors.clone(),
            selected: state
                .monitors
                .iter()
                .find(|monitor| monitor.selected)
                .map(|monitor| monitor.selector.clone()),
            pending: state.pending.clone(),
            error: state.error.clone(),
            mode: state.mode,
            default_monitor: state.default_monitor.clone(),
            mode_revision: state.mode_revision,
            connected: state.epoch.is_some(),
        }
    }

    /// Ask the host to show `selector`. Refused while another pick is pending,
    /// before `/ws` is registered, or for a selector not in the last list.
    pub fn pick_monitor(&self, selector: &str) -> PickOutcome {
        self.update(|state| {
            let Some(epoch) = state.epoch.clone() else {
                return (PickOutcome::NotConnected, false);
            };
            if state.pending.is_some() {
                return (PickOutcome::Pending, false);
            }
            let Some(selected) = state
                .monitors
                .iter()
                .find(|monitor| monitor.selector == selector)
                .map(|monitor| monitor.selected)
            else {
                return (PickOutcome::UnknownMonitor, false);
            };
            if selected {
                return (PickOutcome::AlreadySelected, false);
            }
            queue_pick(state, &epoch, selector);
            state.default_monitor = Some(selector.to_owned());
            (PickOutcome::Queued, true)
        })
    }

    /// Set the mode. On a live socket the command is queued for the net thread,
    /// and the latest target replaces any earlier one.
    pub fn set_mode(&self, mode: StreamMode) -> ModeOutcome {
        self.request_mode(mode).0
    }

    /// [`set_mode`](Self::set_mode) that also returns the request number of the
    /// change (0 when unchanged). The window thread waits for that number to be
    /// acknowledged, so an older write cannot stand in for this one.
    pub(crate) fn request_mode(&self, mode: StreamMode) -> (ModeOutcome, u64) {
        self.update(|state| {
            state.mode_explicit = true;
            if state.mode == mode {
                return ((ModeOutcome::Unchanged, 0), false);
            }
            state.mode = mode;
            state.mode_request += 1;
            let request = state.mode_request;
            match state.epoch.clone() {
                Some(epoch) => {
                    state.outbox.mode = Some((epoch, mode, request));
                    ((ModeOutcome::Queued, request), true)
                }
                None => ((ModeOutcome::Stored, request), true),
            }
        })
    }

    /// Set the mode the next connect starts in. Nothing is sent.
    pub fn initialize_mode(&self, mode: StreamMode) {
        self.update(|state| {
            let changed = state.mode != mode;
            state.mode = mode;
            state.mode_explicit = true;
            ((), changed)
        })
    }

    /// Set the default applied on the first list of the run, in memory only.
    /// Empty or over-long selectors are dropped.
    pub fn set_default_monitor(&self, selector: Option<String>) {
        let selector =
            selector.filter(|value| !value.is_empty() && within_utf16(value, MAX_SELECTOR_CHARS));
        self.update(|state| {
            let changed = state.default_monitor != selector;
            state.default_monitor = selector;
            ((), changed)
        })
    }

    /// `hello-ok` arrived on `/ws` for `epoch`. Any stale pending pick or queued command is dropped.
    pub(crate) fn epoch_registered(&self, epoch: &str) {
        self.update(|state| {
            state.epoch = Some(epoch.to_owned());
            state.pending = None;
            state.outbox = Outbox::default();
            // `signaling-start` sends the configured mode on this socket right
            // after this call, and it is the newest request, so every request
            // made so far is on the wire.
            state.mode_acked = state.mode_request;
            ((), true)
        })
    }

    /// The `/ws` attempt ended. Clears the epoch, any pending pick and queued
    /// commands. The last monitor list is kept, as the browser keeps it.
    pub(crate) fn epoch_restart(&self) {
        self.update(|state| {
            let changed = state.epoch.is_some()
                || state.pending.is_some()
                || state.outbox.mode.is_some()
                || state.outbox.pick.is_some();
            state.epoch = None;
            state.pending = None;
            state.outbox = Outbox::default();
            ((), changed)
        })
    }

    /// A valid `monitor-list` from the socket of `epoch`. It clears the pending
    /// pick (queued or not) and the error notice. The first list of the run
    /// applies the default monitor if it is present and not selected.
    pub(crate) fn apply_monitor_list(&self, monitors: Vec<MonitorEntry>, epoch: &str) {
        self.update(|state| {
            if state.epoch.as_deref() != Some(epoch) {
                return ((), false);
            }
            state.monitors = monitors;
            state.error = None;
            state.pending = None;
            state.outbox.pick = None;
            if !state.default_applied {
                state.default_applied = true;
                if let Some(selector) = state.default_monitor.clone() {
                    let selected = state
                        .monitors
                        .iter()
                        .find(|monitor| monitor.selector == selector)
                        .map(|monitor| monitor.selected);
                    if selected == Some(false) {
                        queue_pick(state, epoch, &selector);
                    }
                }
            }
            ((), true)
        })
    }

    /// A valid `monitor-error` from the socket of `epoch`. It clears the pending
    /// pick (queued or not) and keeps the reason until the next list.
    pub(crate) fn apply_monitor_error(&self, error: MonitorError, epoch: &str) {
        self.update(|state| {
            if state.epoch.as_deref() != Some(epoch) {
                return ((), false);
            }
            state.pending = None;
            state.outbox.pick = None;
            state.error = Some(error);
            ((), true)
        })
    }

    /// Take the commands to write on the socket of `epoch`. A command queued for
    /// another epoch is dropped, and a dropped pick is no longer pending.
    pub(crate) fn take_outbound(&self, epoch: &str) -> Outbound {
        self.update(|state| {
            let (mode, mode_request) = match state.outbox.mode.take() {
                Some((queued, mode, request)) if queued == epoch => (Some(mode), request),
                _ => (None, 0),
            };
            let mut changed = false;
            let pick = match state.outbox.pick.take() {
                Some((queued, selector)) if queued == epoch => Some(selector),
                Some(_) => {
                    state.pending = None;
                    changed = true;
                    None
                }
                None => None,
            };
            (
                Outbound {
                    mode,
                    mode_request,
                    pick,
                },
                changed,
            )
        })
    }

    /// A live `mode` command with request number `request` reached the socket.
    pub(crate) fn mark_mode_written(&self, request: u64) {
        self.update(|state| {
            state.mode_acked = state.mode_acked.max(request);
            state.mode_revision += 1;
            ((), true)
        })
    }

    /// The newest mode request that is on the wire or covered by a start mode.
    pub(crate) fn mode_acked(&self) -> u64 {
        self.state().mode_acked
    }

    /// The mode a connect must send, once one has been set or seeded.
    pub(crate) fn configured_mode(&self) -> Option<StreamMode> {
        let state = self.state();
        state.mode_explicit.then_some(state.mode)
    }

    /// Seed the mode from `NetConfig.stream_mode` unless a mode was already set.
    pub(crate) fn seed_mode(&self, wire: Option<&str>) {
        let Some(mode) = wire.and_then(stream_mode_from_wire) else {
            return;
        };
        self.update(|state| {
            if state.mode_explicit {
                return ((), false);
            }
            let changed = state.mode != mode;
            state.mode = mode;
            state.mode_explicit = true;
            ((), changed)
        })
    }
}

fn queue_pick(state: &mut State, epoch: &str, selector: &str) {
    state.pending = Some(selector.to_owned());
    state.outbox.pick = Some((epoch.to_owned(), selector.to_owned()));
}

/// Whether `text` is at most `max` UTF-16 code units, the length `String.length` counts in the browser.
pub(crate) fn within_utf16(text: &str, max: usize) -> bool {
    text.encode_utf16().count() <= max
}

/// The longest prefix of `text` within `max` UTF-16 code units, as `String.slice` counts them.
/// A surrogate pair is kept whole or dropped whole.
pub(crate) fn utf16_prefix(text: &str, max: usize) -> String {
    let mut units = 0usize;
    text.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= max
        })
        .collect()
}

/// The `streamMode` value receiver.js sends.
pub(crate) fn stream_mode_wire(mode: StreamMode) -> &'static str {
    match mode {
        StreamMode::Work => "work",
        StreamMode::Game => "game",
    }
}

/// Parse a `streamMode` value. Only the two browser values are accepted.
pub(crate) fn stream_mode_from_wire(value: &str) -> Option<StreamMode> {
    match value {
        "work" => Some(StreamMode::Work),
        "game" => Some(StreamMode::Game),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210";

    fn monitor(selector: &str, selected: bool) -> MonitorEntry {
        MonitorEntry {
            selector: selector.into(),
            name: selector.into(),
            width: 1920,
            height: 1080,
            origin_x: 0,
            origin_y: 0,
            selected,
        }
    }

    /// Registered on EPOCH with DISPLAY1 selected and DISPLAY2 available.
    fn connected() -> SessionControls {
        let controls = SessionControls::new();
        controls.epoch_registered(EPOCH);
        controls.apply_monitor_list(
            vec![monitor("DISPLAY1", true), monitor("DISPLAY2", false)],
            EPOCH,
        );
        controls
    }

    fn picked(controls: &SessionControls) -> Option<String> {
        controls.take_outbound(EPOCH).pick
    }

    #[test]
    fn pick_before_registration_is_refused_and_stores_nothing() {
        let controls = SessionControls::new();
        controls.apply_monitor_list(vec![monitor("DISPLAY1", true)], EPOCH);
        assert_eq!(controls.pick_monitor("DISPLAY1"), PickOutcome::NotConnected);
        let snapshot = controls.snapshot();
        assert!(
            snapshot.monitors.is_empty(),
            "a list for an unregistered epoch is ignored"
        );
        assert_eq!(snapshot.pending, None);
        assert_eq!(snapshot.default_monitor, None);
    }

    #[test]
    fn valid_pick_queues_one_command_and_becomes_the_default() {
        let controls = connected();
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        let snapshot = controls.snapshot();
        assert_eq!(snapshot.pending.as_deref(), Some("DISPLAY2"));
        assert_eq!(snapshot.default_monitor.as_deref(), Some("DISPLAY2"));
        assert_eq!(picked(&controls).as_deref(), Some("DISPLAY2"));
        assert_eq!(picked(&controls), None, "a command is written once");
    }

    #[test]
    fn second_pick_is_refused_until_a_list_or_error_clears_pending() {
        let controls = connected();
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Pending);
        controls.apply_monitor_error(
            MonitorError {
                selector: Some("DISPLAY2".into()),
                reason: "busy".into(),
            },
            EPOCH,
        );
        assert_eq!(controls.snapshot().pending, None);
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
    }

    #[test]
    fn a_valid_list_clears_pending_and_follows_the_new_selection() {
        let controls = connected();
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        controls.apply_monitor_list(
            vec![monitor("DISPLAY1", false), monitor("DISPLAY2", true)],
            EPOCH,
        );
        let snapshot = controls.snapshot();
        assert_eq!(snapshot.pending, None);
        assert_eq!(snapshot.selected.as_deref(), Some("DISPLAY2"));
        assert_eq!(controls.pick_monitor("DISPLAY1"), PickOutcome::Queued);
    }

    #[test]
    fn a_list_clears_a_pick_that_was_still_queued() {
        let controls = connected();
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        controls.apply_monitor_list(
            vec![monitor("DISPLAY1", true), monitor("DISPLAY2", false)],
            EPOCH,
        );
        assert_eq!(controls.snapshot().pending, None);
        assert_eq!(
            picked(&controls),
            None,
            "the unwritten pick is dropped with the pending state"
        );
    }

    #[test]
    fn an_error_clears_a_pick_that_was_still_queued() {
        let controls = connected();
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        controls.apply_monitor_error(
            MonitorError {
                selector: Some("DISPLAY2".into()),
                reason: "busy".into(),
            },
            EPOCH,
        );
        assert_eq!(controls.snapshot().pending, None);
        assert_eq!(
            picked(&controls),
            None,
            "the unwritten pick is dropped with the pending state"
        );
    }

    #[test]
    fn picking_the_selected_or_an_unknown_monitor_sends_nothing() {
        let controls = connected();
        assert_eq!(
            controls.pick_monitor("DISPLAY1"),
            PickOutcome::AlreadySelected
        );
        assert_eq!(
            controls.pick_monitor("DISPLAY9"),
            PickOutcome::UnknownMonitor
        );
        assert_eq!(controls.snapshot().pending, None);
        assert_eq!(picked(&controls), None);
    }

    #[test]
    fn error_reason_stays_until_the_next_valid_list() {
        let controls = connected();
        controls.apply_monitor_error(
            MonitorError {
                selector: Some("DISPLAY2".into()),
                reason: "desktop unavailable".into(),
            },
            EPOCH,
        );
        let error = controls.snapshot().error.expect("error recorded");
        assert_eq!(error.selector.as_deref(), Some("DISPLAY2"));
        assert_eq!(error.reason, "desktop unavailable");
        controls.apply_monitor_list(vec![monitor("DISPLAY1", true)], EPOCH);
        assert_eq!(controls.snapshot().error, None);
    }

    #[test]
    fn messages_and_commands_from_another_epoch_are_ignored() {
        let controls = connected();
        controls.apply_monitor_list(vec![monitor("DISPLAY2", true)], OTHER);
        controls.apply_monitor_error(
            MonitorError {
                selector: None,
                reason: "stale".into(),
            },
            OTHER,
        );
        let snapshot = controls.snapshot();
        assert_eq!(
            snapshot.selected.as_deref(),
            Some("DISPLAY1"),
            "stale list ignored"
        );
        assert_eq!(snapshot.error, None, "stale error ignored");

        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        assert_eq!(
            controls.take_outbound(OTHER).pick,
            None,
            "a command for EPOCH is not written on OTHER"
        );
        assert_eq!(
            controls.snapshot().pending,
            None,
            "the dropped pick is no longer pending"
        );
    }

    #[test]
    fn epoch_restart_drops_pending_and_queued_commands_but_keeps_the_list() {
        let controls = connected();
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        controls.set_mode(StreamMode::Game);
        controls.epoch_restart();
        let snapshot = controls.snapshot();
        assert_eq!(snapshot.pending, None);
        assert!(!snapshot.connected);
        assert_eq!(snapshot.selected.as_deref(), Some("DISPLAY1"));
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::NotConnected);
        controls.epoch_registered(OTHER);
        let outbound = controls.take_outbound(OTHER);
        assert_eq!(outbound.pick, None);
        assert_eq!(outbound.mode, None);
    }

    #[test]
    fn default_monitor_applies_once_on_the_first_list_of_the_run() {
        let controls = SessionControls::new();
        controls.set_default_monitor(Some("DISPLAY2".into()));
        controls.epoch_registered(EPOCH);
        controls.apply_monitor_list(
            vec![monitor("DISPLAY1", true), monitor("DISPLAY2", false)],
            EPOCH,
        );
        assert_eq!(picked(&controls).as_deref(), Some("DISPLAY2"));
        assert_eq!(controls.snapshot().pending.as_deref(), Some("DISPLAY2"));

        controls.epoch_restart();
        controls.epoch_registered(OTHER);
        controls.apply_monitor_list(
            vec![monitor("DISPLAY1", true), monitor("DISPLAY2", false)],
            OTHER,
        );
        assert_eq!(
            controls.take_outbound(OTHER).pick,
            None,
            "never retried after reconnect"
        );
    }

    #[test]
    fn default_is_not_retried_after_an_error_on_the_first_list() {
        let controls = SessionControls::new();
        controls.set_default_monitor(Some("DISPLAY2".into()));
        controls.epoch_registered(EPOCH);
        controls.apply_monitor_list(
            vec![monitor("DISPLAY1", true), monitor("DISPLAY2", false)],
            EPOCH,
        );
        controls.apply_monitor_error(
            MonitorError {
                selector: Some("DISPLAY2".into()),
                reason: "busy".into(),
            },
            EPOCH,
        );
        controls.apply_monitor_list(
            vec![monitor("DISPLAY1", true), monitor("DISPLAY2", false)],
            EPOCH,
        );
        assert_eq!(picked(&controls), None);
    }

    #[test]
    fn default_missing_from_the_first_list_is_not_applied_later() {
        let controls = SessionControls::new();
        controls.set_default_monitor(Some("DISPLAY9".into()));
        controls.epoch_registered(EPOCH);
        controls.apply_monitor_list(vec![monitor("DISPLAY1", true)], EPOCH);
        controls.set_default_monitor(Some("DISPLAY1".into()));
        controls.epoch_restart();
        controls.epoch_registered(OTHER);
        controls.apply_monitor_list(
            vec![monitor("DISPLAY1", false), monitor("DISPLAY2", true)],
            OTHER,
        );
        assert_eq!(controls.take_outbound(OTHER).pick, None);
    }

    #[test]
    fn live_mode_change_queues_one_target_and_counts_when_written() {
        let controls = connected();
        assert_eq!(controls.set_mode(StreamMode::Game), ModeOutcome::Queued);
        assert_eq!(controls.set_mode(StreamMode::Work), ModeOutcome::Queued);
        assert_eq!(controls.set_mode(StreamMode::Game), ModeOutcome::Queued);
        assert_eq!(controls.snapshot().mode_revision, 0);
        let outbound = controls.take_outbound(EPOCH);
        assert_eq!(outbound.mode, Some(StreamMode::Game));
        controls.mark_mode_written(outbound.mode_request);
        assert_eq!(controls.snapshot().mode_revision, 1);
        assert_eq!(controls.snapshot().mode, StreamMode::Game);
    }

    #[test]
    fn an_older_write_acknowledges_only_its_own_request() {
        let controls = connected();
        assert_eq!(controls.set_mode(StreamMode::Game), ModeOutcome::Queued);
        let older = controls.take_outbound(EPOCH);
        // A newer request is queued while the older write is still in flight.
        assert_eq!(controls.set_mode(StreamMode::Work), ModeOutcome::Queued);
        controls.mark_mode_written(older.mode_request);
        assert_eq!(controls.snapshot().mode_revision, 1);
        assert_eq!(
            controls.mode_acked(),
            older.mode_request,
            "the newer request is not on the wire"
        );

        let newer = controls.take_outbound(EPOCH);
        assert_eq!(newer.mode, Some(StreamMode::Work));
        assert!(newer.mode_request > older.mode_request);
        controls.mark_mode_written(newer.mode_request);
        assert_eq!(controls.mode_acked(), newer.mode_request);
    }

    #[test]
    fn a_socket_that_registers_again_covers_the_requests_made_before_it() {
        let controls = connected();
        let (outcome, requested) = controls.request_mode(StreamMode::Game);
        assert_eq!(outcome, ModeOutcome::Queued);
        controls.epoch_restart();
        assert!(
            controls.mode_acked() < requested,
            "a dropped write is not acknowledged"
        );
        controls.epoch_registered(EPOCH);
        assert_eq!(
            controls.mode_acked(),
            requested,
            "signaling-start sends the configured mode"
        );
        assert_eq!(
            controls.snapshot().mode_revision,
            0,
            "a start mode is not a live write"
        );
    }

    #[test]
    fn same_mode_is_unchanged_and_sends_nothing() {
        let controls = connected();
        assert_eq!(controls.set_mode(StreamMode::Work), ModeOutcome::Unchanged);
        assert_eq!(controls.take_outbound(EPOCH).mode, None);
    }

    #[test]
    fn mode_before_connect_is_stored_for_signaling_start() {
        let controls = SessionControls::new();
        assert_eq!(controls.set_mode(StreamMode::Game), ModeOutcome::Stored);
        let snapshot = controls.snapshot();
        assert_eq!(snapshot.mode, StreamMode::Game);
        assert!(!snapshot.connected);
        assert_eq!(controls.configured_mode(), Some(StreamMode::Game));
    }

    #[test]
    fn mode_target_is_dropped_on_epoch_restart() {
        let controls = connected();
        assert_eq!(controls.set_mode(StreamMode::Game), ModeOutcome::Queued);
        controls.epoch_restart();
        controls.epoch_registered(OTHER);
        assert_eq!(controls.take_outbound(OTHER).mode, None);
    }

    #[test]
    fn seeded_mode_never_overrides_an_explicit_one() {
        let seeded = SessionControls::new();
        seeded.seed_mode(Some("game"));
        assert_eq!(seeded.configured_mode(), Some(StreamMode::Game));

        let explicit = SessionControls::new();
        explicit.initialize_mode(StreamMode::Game);
        explicit.seed_mode(Some("work"));
        assert_eq!(explicit.snapshot().mode, StreamMode::Game);

        let unset = SessionControls::new();
        unset.seed_mode(None);
        assert_eq!(
            unset.configured_mode(),
            None,
            "no mode leaves NetConfig.stream_mode alone"
        );
    }

    #[test]
    fn notifier_runs_without_a_lock_and_reads_the_snapshot_it_was_told_about() {
        let controls = SessionControls::new();
        let seen: Arc<Mutex<Vec<ControlSnapshot>>> = Arc::default();
        let reader = controls.clone();
        let sink = Arc::clone(&seen);
        controls.set_notifier(Arc::new(move || {
            // Re-enters the controls. This deadlocks if the notifier runs under the state lock.
            sink.lock().unwrap().push(reader.snapshot());
        }));

        controls.epoch_registered(EPOCH);
        controls.apply_monitor_list(
            vec![monitor("DISPLAY1", true), monitor("DISPLAY2", false)],
            EPOCH,
        );
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        assert_eq!(controls.set_mode(StreamMode::Game), ModeOutcome::Queued);
        // Unchanged calls and refused picks change nothing, so they must not wake the shell.
        assert_eq!(controls.set_mode(StreamMode::Game), ModeOutcome::Unchanged);
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Pending);
        let outbound = controls.take_outbound(EPOCH);
        controls.mark_mode_written(outbound.mode_request);
        controls.epoch_restart();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 6, "one wake per change, none for the no-ops");
        assert!(seen[0].connected);
        assert_eq!(seen[1].monitors.len(), 2);
        assert_eq!(seen[2].pending.as_deref(), Some("DISPLAY2"));
        assert_eq!(seen[3].mode, StreamMode::Game);
        // Writing the queued commands changes nothing visible; the pick stays pending until a host answer.
        assert_eq!(seen[4].mode_revision, 1);
        assert_eq!(seen[4].pending.as_deref(), Some("DISPLAY2"));
        assert!(!seen[5].connected && seen[5].pending.is_none());
    }

    #[test]
    fn utf16_limits_match_the_browser_string_length() {
        // Two UTF-16 units per character, so 64 characters are 128 units.
        let astral = "\u{1F600}".repeat(64);
        assert!(within_utf16(&astral, MAX_SELECTOR_CHARS));
        assert!(!within_utf16(&format!("{astral}x"), MAX_SELECTOR_CHARS));
        // A surrogate pair is never split by the reason cap.
        let reason = format!("{}\u{1F600}", "a".repeat(MAX_ERROR_CHARS - 1));
        assert_eq!(
            utf16_prefix(&reason, MAX_ERROR_CHARS),
            "a".repeat(MAX_ERROR_CHARS - 1)
        );
    }
}
