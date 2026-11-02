//! What the network, the input adapter and the session view share, and how the network's
//! monitor and mode state is laid over the session state the UI shows. Used by the egui client
//! and by the Flutter app.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::{
    model::{Action, AppState, Monitor, PcKind, RemotePc},
    net::{MonitorEntry, SessionControls},
};

use super::live_input::{input_mode, LiveInputControl};

/// What the session backend shares with the network and the input adapter.
/// `controls` is the same handle as `NetConfig.controls`; `input` is the
/// handle of the shell's live capture.
#[derive(Clone)]
pub struct SessionHandles {
    pub controls: SessionControls,
    pub input: LiveInputControl,
    /// The PC being viewed. Its saved default monitor is shown on its entry in `pcs`.
    pub pc_id: String,
    pub pc_name: String,
}

/// Puts the network's view over the session's state: the mode, the monitor
/// list with its selection, pending and error, and the saved default on the
/// viewed PC. Media numbers in `state` are left alone.
///
/// The viewed PC is updated in place: only `last_mode` and `default_monitor`
/// change, and the other PCs are kept. A PC missing from `state` is appended.
pub fn overlay_controls(state: &mut AppState, handles: &SessionHandles) {
    let controls = handles.controls.snapshot();
    match state.pcs.iter_mut().find(|pc| pc.id == handles.pc_id) {
        Some(pc) => {
            pc.last_mode = Some(controls.mode);
            pc.default_monitor = controls.default_monitor.clone();
        }
        None => state.pcs.push(RemotePc {
            id: handles.pc_id.clone(),
            name: handles.pc_name.clone(),
            kind: PcKind::Desktop,
            online: true,
            last_mode: Some(controls.mode),
            default_monitor: controls.default_monitor.clone(),
            ..Default::default()
        }),
    }
    if let Some(session) = state.session.as_mut() {
        session.mode = controls.mode;
        session.pc_id = handles.pc_id.clone();
        session.pc_name = handles.pc_name.clone();
        session.captured = handles
            .input
            .status()
            .is_some_and(|status| status.snapshot().captured);
        // The list is the network's, even when empty: an empty list is valid
        // and clears the monitors. Before the first list there are none.
        session.monitors = controls.monitors.iter().map(monitor_view).collect();
        session.monitor = controls
            .monitors
            .iter()
            .position(|m| m.selected)
            .unwrap_or(0);
        session.monitor_pending = controls.pending.is_some();
        session.monitor_error = controls.error.as_ref().map(|error| error.reason.clone());
        // Quality and stream sound have no live path yet.
        session.quality_controls_available = false;
        session.audio_controls_available = false;
    }
}

/// Maps the host's monitor geometry to the picker. The wire has no refresh
/// rate; it is 0 here, which means unknown. Windows puts the primary monitor
/// at the desktop origin, so that is how `primary` is read.
fn monitor_view(entry: &MonitorEntry) -> Monitor {
    Monitor {
        selector: entry.selector.clone(),
        name: entry.name.clone(),
        width: entry.width,
        height: entry.height,
        refresh_hz: 0,
        x: entry.origin_x,
        y: entry.origin_y,
        primary: entry.origin_x == 0 && entry.origin_y == 0,
    }
}

/// Applies one session action: leave, mode and display. Quality and stream sound have no live
/// path yet. Returns true when the caller should tell the UI something changed.
pub fn act(handles: &SessionHandles, stop: &AtomicBool, action: Action) -> bool {
    match action {
        Action::Disconnect => {
            stop.store(true, Ordering::Release);
            true
        }
        Action::SetMode(mode) => {
            // Same mode is a no-op, as in the browser's setPointerMode.
            if handles.controls.snapshot().mode == mode {
                return false;
            }
            // Queues the network's mode and moves the local capture once the mode write has
            // landed (see live_input).
            handles.input.set_mode(input_mode(mode));
            true
        }
        Action::SetMonitor(index) => {
            let snapshot = handles.controls.snapshot();
            // One pick at a time: the host has not answered the last one yet.
            if snapshot.pending.is_some() {
                return false;
            }
            let Some(entry) = snapshot.monitors.get(index) else {
                return false;
            };
            if entry.selector.is_empty() {
                return false;
            }
            if entry.selected {
                // Choosing the current display still saves the user's default, with no
                // redundant wire switch.
                handles
                    .controls
                    .set_default_monitor(Some(entry.selector.clone()));
                return true;
            }
            handles.controls.pick_monitor(&entry.selector);
            true
        }
        _ => false,
    }
}
