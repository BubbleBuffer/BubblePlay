//! The flutter_rust_bridge surface. Plain strings only: Dart owns no copy of the Rust model;
//! it parses the same JSON the Android host sends (see `contract`).

use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Result};
use flutter_rust_bridge::frb;

use crate::backend::{AppBackend, Paths};
use crate::contract::Core;
use crate::devices_pipe::DevicesClient;
use crate::frb_generated::StreamSink;

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
