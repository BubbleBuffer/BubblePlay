//! The flutter_rust_bridge surface. Plain strings only: Dart owns no copy of the Rust model;
//! it parses the same JSON the Android host sends (see `contract`).

use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Result};
use flutter_rust_bridge::frb;

use crate::backend::{AppBackend, Paths};
use crate::contract::Core;
use crate::ctl_log;
use crate::devices_pipe::DevicesClient;
use crate::frb_generated::StreamSink;
use crate::stream_host;

static CORE: OnceLock<Mutex<Core>> = OnceLock::new();

fn core() -> Result<std::sync::MutexGuard<'static, Core>> {
    CORE.get()
        .ok_or_else(|| anyhow!("the app core is not started"))
        .map(|core| core.lock().unwrap_or_else(|p| p.into_inner()))
}

/// Opens the viewer key and PC list in `%LOCALAPPDATA%\BubblePlay\viewer`. Call once.
pub fn app_start() -> Result<()> {
    let core = Core::new(AppBackend::new(Paths::default_location()?));
    CORE.set(Mutex::new(core))
        .map_err(|_| anyhow!("the app core is already started"))
}

/// The current `AppState` as JSON.
#[frb(sync)]
pub fn app_snapshot() -> Result<String> {
    Ok(core()?.snapshot_json())
}

/// Sends one `Action` (JSON). Returns at once; results arrive as later snapshots.
#[frb(sync)]
pub fn app_act(action: String) -> Result<()> {
    core()?.act_json(&action)
}

/// The stored key as portable `.bpkey` text, for the user to save. Never part of a snapshot.
pub fn app_export_key() -> Result<String> {
    core()?.export_key()
}

/// One event per snapshot change.
pub fn app_changes(sink: StreamSink<()>) -> Result<()> {
    core()?.set_notifier(Arc::new(move || {
        let _ = sink.add(());
    }));
    Ok(())
}

static DEVICES: Mutex<Option<DevicesClient>> = Mutex::new(None);

fn devices() -> std::sync::MutexGuard<'static, Option<DevicesClient>> {
    DEVICES.lock().unwrap_or_else(|p| p.into_inner())
}

/// Connects to the host's Devices pipe and keeps reconnecting while the stream is listened to.
/// Every event is one raw JSON line: the host's lines verbatim, plus `{"type":"connected"}` and
/// `{"type":"disconnected","reason":...}` for the link itself. A second call replaces the first.
pub fn devices_connect(sink: StreamSink<String>) -> Result<()> {
    let old = devices().take();
    drop(old);
    let client = DevicesClient::start_default(Arc::new(move |line| sink.add(line).is_ok()));
    *devices() = Some(client);
    Ok(())
}

/// Sends one command line (a JSON object: open, revoke, startPairing, cancelPairing, allow,
/// deny). Fails when the line is invalid or the host is not connected.
#[frb(sync)]
pub fn devices_send(line: String) -> Result<()> {
    match devices().as_ref() {
        Some(client) => client.send(&line).map_err(|e| anyhow!(e)),
        None => Err(anyhow!("the Devices pipe is not connected")),
    }
}

/// Stops the Devices pipe client.
pub fn devices_disconnect() {
    let old = devices().take();
    drop(old);
}

/// One monitor snapshot from the host (`monitor-previews-v1`).
pub struct MonitorPreviewData {
    /// The `monitor-list` selector it shows.
    pub monitor: String,
    pub width: u32,
    pub height: u32,
    /// A baseline JPEG.
    pub jpeg: Vec<u8>,
}

/// Removes and returns the held snapshot of the monitor `selector`, or null when none is
/// held. Called when that monitor becomes the selected one: its picture is used once.
#[frb(sync)]
pub fn monitor_preview_take(selector: String) -> Option<MonitorPreviewData> {
    let preview = stream_host::previews().take(&selector)?;
    Some(MonitorPreviewData {
        monitor: preview.monitor,
        width: preview.width,
        height: preview.height,
        jpeg: preview.jpeg.to_vec(),
    })
}

/// The media generation of the last video frame shown, 0 before the first. It changes with
/// the first live frame after the host switches monitor.
#[frb(sync)]
pub fn media_generation() -> i64 {
    i64::from(stream_host::presented_generation())
}

/// The control channel's state for the log upload queue.
pub struct CtlLogState {
    /// Connected to a PC that advertised `client-logs-v1`: sends are accepted.
    pub open: bool,
    /// Bytes waiting in the outbound queue.
    pub buffered_bytes: i64,
    /// Milliseconds since the last input packet; the largest int64 if there was none yet.
    pub since_last_input_ms: i64,
}

/// Queues one upload message (`logs-batch`, `feedback-begin` or `feedback-chunk` as JSON) on
/// the active session's control channel. False when there is no connection, the queue is
/// full, or the message is not one of those kinds.
#[frb(sync)]
pub fn ctl_log_send(json: String) -> bool {
    ctl_log::global().send(json)
}

/// The control channel's state; cheap, the Dart bridge polls it while a connection is open.
#[frb(sync)]
pub fn ctl_log_state() -> CtlLogState {
    let state = ctl_log::global().state();
    CtlLogState {
        open: state.open,
        buffered_bytes: state.buffered_bytes,
        since_last_input_ms: state.since_last_input_ms,
    }
}

/// Control channel events for the upload queue, one JSON string each:
/// `{"kind":"opened","pcId":..,"peerCaps":[..]}`, `{"kind":"message","json":{..}}` and
/// `{"kind":"closed"}`. Only one subscription exists: a second call replaces the first (the
/// old stream gets nothing more), and an already open connection is replayed as `opened`.
pub fn ctl_log_events(sink: StreamSink<String>) -> Result<()> {
    ctl_log::global().subscribe(move |line| sink.add(line).is_ok());
    Ok(())
}
