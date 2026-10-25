//! The monitor surface (`--monitors`) and the stream-mode control on the
//! loopback fake: the list, the switch sequence, scripted errors, mode changes,
//! stale epochs, and a WSS generation change that starts at an IDR. Synthetic
//! recordings and no real capture. The RTC admission across a switch is in
//! `webrtc.rs`, where the offerer lives.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use fake_wss_host::cli;
use fake_wss_host::model::{Injection, Injections, Source};
use fake_wss_host::monitors::Knobs as Monitors;
use fake_wss_host::protocol::ReceiveGate;
use fake_wss_host::signal::{parse_signal, Signal};
use fake_wss_host::stream::Codec;
use serde_json::{json, Value};

const DISPLAY1: &str = r"\\.\DISPLAY1";
const DISPLAY2: &str = r"\\.\DISPLAY2";
const EPOCH: &str = "0123456789abcdef0123456789abcdef";
const WAIT: Duration = Duration::from_secs(2);
/// How long to wait to see that nothing arrives.
const QUIET: Duration = Duration::from_millis(300);

fn source() -> Vec<Arc<dyn Source>> {
    vec![synthetic_codec(Codec::H264, 120, 30)]
}

fn switch(epoch: &str, selector: &str) -> Value {
    json!({"type": "media-control", "action": "monitor", "protocol": 4, "epoch": epoch,
           "monitor": selector, "reason": "user-monitor"})
}

fn mode(epoch: &str, stream_mode: &str) -> Value {
    json!({"type": "media-control", "action": "mode", "protocol": 4, "epoch": epoch,
           "streamMode": stream_mode, "reason": "user-mode"})
}

fn refresh(epoch: &str) -> Value {
    json!({"type": "media-control", "action": "monitors", "protocol": 4, "epoch": epoch,
           "reason": "diagnostic-refresh"})
}

async fn expect_json(ws: &mut Ws) -> Value {
    next_json(ws, WAIT).await.expect("a message from the fake")
}

/// The next message of `kind`; anything else on the socket is skipped.
async fn until_type(ws: &mut Ws, kind: &str) -> Value {
    for _ in 0..16 {
        let value = expect_json(ws).await;
        if value["type"] == kind {
            return value;
        }
    }
    panic!("no {kind} message arrived");
}

#[tokio::test]
async fn hello_lists_two_monitors_with_the_fixture_geometry() {
    let harness = spawn_monitored(
        source(),
        Duration::from_millis(900),
        Duration::from_millis(100),
        Monitors::scripted(),
        None,
    )
    .await;
    let (mut ws, _epoch) = signal_hello(harness.addr).await;
    let list = expect_json(&mut ws).await;
    assert_eq!(list["type"], "monitor-list", "{list}");
    let monitors = list["monitors"].as_array().expect("monitors array");
    assert_eq!(monitors.len(), 2, "two monitors: {list}");
    assert_eq!(monitors[0]["selector"], DISPLAY1);
    assert_eq!(monitors[0]["name"], "Monitor 1");
    assert_eq!(monitors[0]["width"], 3840);
    assert_eq!(monitors[0]["height"], 2160);
    assert_eq!(monitors[0]["origin_x"], 0);
    assert_eq!(monitors[0]["origin_y"], 0);
    assert_eq!(monitors[0]["selected"], true);
    assert_eq!(monitors[1]["selector"], DISPLAY2);
    assert_eq!(monitors[1]["width"], 2560);
    assert_eq!(monitors[1]["origin_x"], 3840);
    assert_eq!(monitors[1]["selected"], false);
    assert!(list.get("follow").is_none(), "follow is not advertised");
    drop(ws);
    let snap = harness.finish().await;
    assert_eq!(snap.counters.monitor_list_sent, 1);
    assert_eq!(snap.counters.monitor_selected.as_deref(), Some(DISPLAY1));
}

#[tokio::test]
async fn a_switch_is_unavailable_then_active_then_the_list_with_the_new_selection() {
    let harness = spawn_monitored(
        source(),
        Duration::from_millis(1200),
        Duration::from_millis(100),
        Monitors::scripted(),
        None,
    )
    .await;
    let (mut ws, epoch) = signal_hello(harness.addr).await;
    let _hello_list = expect_json(&mut ws).await;
    send_json(&mut ws, switch(&epoch, DISPLAY2)).await;

    let unavailable = expect_json(&mut ws).await;
    assert_eq!(unavailable["type"], "desktop-state", "{unavailable}");
    assert_eq!(unavailable["state"], "unavailable");
    assert_eq!(unavailable["reason"], "monitor-switch");
    let active = expect_json(&mut ws).await;
    assert_eq!(active["type"], "desktop-state", "{active}");
    assert_eq!(active["state"], "active");
    assert_eq!(active["reason"], "monitor-switched");
    let before = unavailable["generation"].as_u64().expect("generation");
    assert_eq!(active["generation"].as_u64(), Some(before + 1), "generation bumped once");
    let list = expect_json(&mut ws).await;
    assert_eq!(list["type"], "monitor-list", "{list}");
    assert_eq!(list["monitors"][0]["selected"], false);
    assert_eq!(list["monitors"][1]["selected"], true);
    drop(ws);

    let snap = harness.finish().await;
    assert_eq!(snap.counters.monitor_switches, 1);
    assert_eq!(snap.counters.monitor_switch_failures, 0);
    assert_eq!(snap.counters.monitor_selected.as_deref(), Some(DISPLAY2));
    assert_eq!(snap.counters.monitor_list_sent, 2, "hello and the switch");
    assert!(
        snap.recoveries.iter().any(|r| r.kind == Injection::MonitorSwitch),
        "the switch opens a recovery row"
    );
}

#[tokio::test]
async fn a_scripted_error_keeps_the_display_and_an_unknown_selector_is_not_found() {
    let mut knobs = Monitors::scripted();
    knobs.failing.push(DISPLAY2.to_owned());
    let harness = spawn_monitored(
        source(),
        Duration::from_millis(1600),
        Duration::from_millis(100),
        knobs,
        None,
    )
    .await;
    let (mut ws, epoch) = signal_hello(harness.addr).await;
    let _hello_list = expect_json(&mut ws).await;

    send_json(&mut ws, switch(&epoch, DISPLAY2)).await;
    let unavailable = until_type(&mut ws, "desktop-state").await;
    assert_eq!(unavailable["state"], "unavailable");
    let error = until_type(&mut ws, "monitor-error").await;
    assert_eq!(error["selector"], DISPLAY2);
    assert_eq!(error["reason"], "scripted monitor error");
    let active = until_type(&mut ws, "desktop-state").await;
    assert_eq!(active["state"], "active", "the previous capture is retained");
    assert_eq!(active["reason"], "monitor-switched");
    assert!(active["generation"].as_u64() > unavailable["generation"].as_u64());
    let list = until_type(&mut ws, "monitor-list").await;
    assert_eq!(list["monitors"][0]["selected"], true, "the previous display is kept");
    assert_eq!(list["monitors"][1]["selected"], false);

    send_json(&mut ws, switch(&epoch, r"\\.\DISPLAY9")).await;
    let error = until_type(&mut ws, "monitor-error").await;
    assert_eq!(error["selector"], r"\\.\DISPLAY9");
    assert_eq!(error["reason"], "monitor not found");
    let _active = until_type(&mut ws, "desktop-state").await;
    let list = until_type(&mut ws, "monitor-list").await;
    assert_eq!(list["monitors"][0]["selected"], true);
    drop(ws);

    let snap = harness.finish().await;
    assert_eq!(snap.counters.monitor_switches, 2);
    assert_eq!(snap.counters.monitor_switch_failures, 2);
    assert_eq!(snap.counters.monitor_selected.as_deref(), Some(DISPLAY1));
}

#[tokio::test]
async fn stale_epoch_monitor_and_mode_controls_are_refused_silently() {
    let harness = spawn_monitored(
        source(),
        Duration::from_millis(900),
        Duration::from_millis(100),
        Monitors::scripted(),
        None,
    )
    .await;
    let (mut ws, _epoch) = signal_hello(harness.addr).await;
    let _hello_list = expect_json(&mut ws).await;
    let stale = "f".repeat(32);
    send_json(&mut ws, switch(&stale, DISPLAY2)).await;
    send_json(&mut ws, mode(&stale, "game")).await;
    send_json(&mut ws, refresh(&stale)).await;
    assert!(
        next_json(&mut ws, QUIET).await.is_none(),
        "no reply to stale-epoch controls"
    );
    drop(ws);

    let snap = harness.finish().await;
    assert_eq!(snap.counters.stale_epoch_control, 3);
    assert_eq!(snap.counters.monitor_switches, 0);
    assert_eq!(snap.counters.monitor_refreshes, 0);
    assert_eq!(snap.counters.stream_mode_changes, 0);
    assert_eq!(snap.counters.stream_mode.as_deref(), Some("work"));
    assert_eq!(snap.counters.monitor_selected.as_deref(), Some(DISPLAY1));
}

#[tokio::test]
async fn mode_changes_are_counted_repeats_are_noops_and_unknown_modes_are_unhandled() {
    let harness = spawn_monitored(
        source(),
        Duration::from_millis(900),
        Duration::from_millis(100),
        Monitors::scripted(),
        None,
    )
    .await;
    let (mut ws, epoch) = signal_hello(harness.addr).await;
    let _hello_list = expect_json(&mut ws).await;
    send_json(&mut ws, mode(&epoch, "game")).await;
    send_json(&mut ws, mode(&epoch, "game")).await;
    send_json(&mut ws, mode(&epoch, "turbo")).await;
    // A mode control has no reply, so the refresh's list is the next message.
    send_json(&mut ws, refresh(&epoch)).await;
    let list = expect_json(&mut ws).await;
    assert_eq!(list["type"], "monitor-list", "{list}");
    drop(ws);

    let snap = harness.finish().await;
    assert_eq!(snap.counters.stream_mode.as_deref(), Some("game"));
    assert_eq!(snap.counters.stream_mode_changes, 1, "the repeat is a no-op");
    assert_eq!(snap.counters.unhandled_control.get("mode"), Some(&1));
    assert_eq!(snap.counters.monitor_refreshes, 1);
    assert_eq!(snap.counters.monitor_list_sent, 2, "hello and the refresh");
}

#[tokio::test]
async fn without_the_knob_the_list_and_the_monitor_actions_stay_unhandled() {
    let harness = spawn(
        synthetic_codec(Codec::H264, 120, 30),
        Injections::default(),
        Duration::from_millis(900),
        Duration::from_millis(100),
    )
    .await;
    let (mut ws, epoch) = signal_hello(harness.addr).await;
    send_json(&mut ws, switch(&epoch, DISPLAY2)).await;
    send_json(&mut ws, refresh(&epoch)).await;
    assert!(
        next_json(&mut ws, QUIET).await.is_none(),
        "without --monitors the monitor controls send nothing back"
    );
    drop(ws);

    let snap = harness.finish().await;
    assert_eq!(snap.counters.monitor_list_sent, 0);
    assert_eq!(snap.counters.monitor_switches, 0);
    assert_eq!(snap.counters.unhandled_control.get("monitor"), Some(&1));
    assert_eq!(snap.counters.unhandled_control.get("monitors"), Some(&1));
}

#[tokio::test]
async fn a_switch_moves_the_wss_stream_to_a_new_generation_that_starts_at_an_idr() {
    let harness = spawn_monitored(
        source(),
        Duration::from_millis(3200),
        Duration::from_millis(100),
        Monitors::scripted(),
        None,
    )
    .await;
    let (mut signal, epoch, ticket) = signal_hello_ticket(harness.addr).await;
    let _hello_list = expect_json(&mut signal).await;
    let mut media = media_open(harness.addr, &epoch, &ticket)
        .await
        .expect("media upgrade");
    let reply = expect_json(&mut media).await;
    assert_eq!(reply["type"], "hello-ok", "{reply}");
    let peer = expect_json(&mut media).await;
    assert_eq!(peer["type"], "media-peer", "{peer}");
    send_json(&mut signal, start_message(&epoch, "h264")).await;

    let mut gate = ReceiveGate::new();
    let mut before = Receiver::default();
    ack_until(
        &mut media,
        Instant::now() + Duration::from_millis(900),
        &mut before,
        &mut gate,
    )
    .await;
    assert!(!before.frames.is_empty(), "frames before the switch");
    let old = before.frames[0].generation;

    send_json(&mut signal, switch(&epoch, DISPLAY2)).await;
    let unavailable = until_type(&mut signal, "desktop-state").await;
    assert_eq!(unavailable["generation"].as_u64(), Some(u64::from(old)));
    let active = until_type(&mut signal, "desktop-state").await;
    let new = u32::try_from(active["generation"].as_u64().expect("generation")).expect("u32");
    assert_eq!(new, old + 1);

    let mut after = Receiver::default();
    ack_until(
        &mut media,
        Instant::now() + Duration::from_millis(900),
        &mut after,
        &mut gate,
    )
    .await;
    // Frames the fake queued before the switch may still arrive first, at the
    // old generation. From the first new-generation frame on, there is none
    // of the old one, and that frame is an IDR.
    let start = after
        .frames
        .iter()
        .position(|f| f.generation == new)
        .expect("frames at the new generation");
    assert!(after.frames[start].key, "the new generation starts at an IDR");
    assert!(
        after.frames[start..].iter().all(|f| f.generation == new),
        "no old-generation frame after the switch"
    );
    drop(signal);
    drop(media);

    let snap = harness.finish().await;
    let recovery = snap
        .recoveries
        .iter()
        .find(|r| r.kind == Injection::MonitorSwitch)
        .expect("the switch is a recovery row");
    assert_eq!(recovery.idr.map(|(generation, _)| generation), Some(new));
    assert!(recovery.recovered_ms.is_some(), "the IDR was acknowledged");
    assert_eq!(snap.counters.monitor_switches, 1);
}

#[test]
fn monitor_and_stream_mode_fields_are_read_from_media_control() {
    let monitor = json!({"type": "media-control", "action": "monitor", "protocol": 4,
                         "epoch": EPOCH, "monitor": DISPLAY2});
    match parse_signal(&monitor.to_string()) {
        Ok(Signal::MediaControl(control)) => {
            assert_eq!(control.action, "monitor");
            assert_eq!(control.monitor.as_deref(), Some(DISPLAY2));
            assert_eq!(control.stream_mode, None);
        }
        other => panic!("expected media-control, got {other:?}"),
    }
    let mode_value = json!({"type": "media-control", "action": "mode", "protocol": 4,
                            "epoch": EPOCH, "streamMode": "game"});
    match parse_signal(&mode_value.to_string()) {
        Ok(Signal::MediaControl(control)) => {
            assert_eq!(control.stream_mode.as_deref(), Some("game"));
            assert_eq!(control.monitor, None);
        }
        other => panic!("expected media-control, got {other:?}"),
    }
}

#[test]
fn the_monitor_flags_parse_and_need_the_knob() {
    let on = cli::parse(&[
        "--monitors".to_owned(),
        "--monitor-error".to_owned(),
        DISPLAY2.to_owned(),
    ])
    .expect("parses")
    .expect("not help");
    assert!(on.monitors.enabled);
    assert_eq!(on.monitors.monitors.len(), 2);
    assert_eq!(on.monitors.failing, vec![DISPLAY2.to_owned()]);

    let off = cli::parse(&[]).expect("parses").expect("not help");
    assert!(!off.monitors.enabled);
    assert!(off.monitors.failing.is_empty());

    assert!(
        cli::parse(&["--monitor-error".to_owned(), DISPLAY2.to_owned()]).is_err(),
        "--monitor-error needs --monitors"
    );
}
