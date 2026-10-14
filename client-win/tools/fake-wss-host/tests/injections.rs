//! The three deterministic injections, each once, on a synthetic stream with
//! a receiver that ACKs and asks for keyframes through `ReceiveGate`.

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use common::*;
use fake_wss_host::model::Injections;
use fake_wss_host::report::{checks, RunMeta, StreamInfo};

fn meta(injections: Injections) -> RunMeta {
    RunMeta {
        constraint: None,
        streams: vec![StreamInfo {
            codec: "h264",
            width: 1920,
            height: 1080,
        }],
        fps: 60,
        bitrate_bps: 20_000_000,
        exit_after_s: 3.5,
        drain_ms: 500,
        bind: String::new(),
        injections,
        audio: None,
    }
}

#[tokio::test]
async fn seq_gap_generation_change_and_socket_drop_each_fire_once_and_recover() {
    let injections = Injections {
        seq_gap_at: Some(Duration::from_millis(300)),
        generation_at: Some(Duration::from_millis(600)),
        socket_drop_at: Some(Duration::from_millis(1200)),
    };
    let harness = spawn(
        synthetic(120, 30),
        injections,
        Duration::from_millis(3500),
        Duration::from_millis(500),
    )
    .await;
    let addr = harness.addr;

    let (first, epoch1) =
        ack_session(addr, "h264", Instant::now() + Duration::from_millis(2500)).await;
    assert!(first.closed, "the socket drop closed the media socket");
    let (second, epoch2) =
        ack_session(addr, "h264", Instant::now() + Duration::from_millis(1800)).await;
    let snap = harness.finish().await;

    // Each injection fired exactly once.
    assert_eq!(snap.counters.seq_gaps_injected, 1);
    assert_eq!(snap.counters.generation_changes_injected, 1);
    assert_eq!(snap.counters.socket_drops, 1);

    // The gap is one missing sequence number, seen by the receiver.
    let gaps = first
        .frames
        .windows(2)
        .filter(|w| w[1].seq != w[0].seq.wrapping_add(1))
        .count();
    assert_eq!(gaps, 1, "one sequence gap on the wire");
    let generations: BTreeSet<u32> = first.frames.iter().map(|f| f.generation).collect();
    assert!(
        generations.len() >= 2,
        "the generation changed mid-session: {generations:?}"
    );

    // A new epoch after the drop, and the second session streams from a keyframe.
    assert_ne!(epoch1, epoch2, "the drop rotated the epoch");
    assert_eq!(snap.epochs, vec![epoch1, epoch2]);
    assert!(
        second.frames.first().is_some_and(|f| f.key),
        "the new session starts at an IDR"
    );

    // Every recovery closed on an ACKed keyframe, well inside the keyframe timeout.
    assert_eq!(snap.recoveries.len(), 3);
    for recovery in &snap.recoveries {
        let ms = recovery
            .recovery_ms()
            .unwrap_or_else(|| panic!("not recovered: {recovery:?}"));
        assert!(ms > 0.0 && ms < 1500.0, "recovery {ms} ms: {recovery:?}");
    }

    // Every frame ACKed or accounted for, and the report's checks all pass.
    assert_eq!(snap.in_flight_at_exit, 0);
    let list = checks(&meta(injections), &snap);
    assert!(list.iter().all(|c| c.pass), "{list:#?}");
}

#[tokio::test]
async fn a_silent_receiver_is_sought_to_an_idr_after_the_keyframe_timeout() {
    // Seq gap at 0.3 s on a receiver that never asks for a keyframe: the
    // fake seeks anyway, 3 s later, and counts it.
    let injections = Injections {
        seq_gap_at: Some(Duration::from_millis(300)),
        ..Injections::default()
    };
    let harness = spawn(
        synthetic(120, 30),
        injections,
        Duration::from_millis(4300),
        Duration::from_millis(300),
    )
    .await;
    let (signal, mut media, _epoch) = streaming_session(harness.addr, "h264").await;
    let mut log = Receiver::default();
    // The receiver ACKs but never sends a keyframe request.
    let mut quiet = fake_wss_host::protocol::ReceiveGate::new();
    ack_until_without_requests(
        &mut media,
        Instant::now() + Duration::from_millis(4000),
        &mut log,
        &mut quiet,
    )
    .await;
    drop(signal);
    let snap = harness.finish().await;
    assert_eq!(
        snap.counters.keyframe_timeouts, 1,
        "the timeout seek ran once"
    );
    assert!(snap.recoveries[0].keyframe_request.is_none());
    assert!(
        snap.recoveries[0].recovered_ms.is_some(),
        "the seek's IDR was ACKed"
    );
}

/// `ack_until` sends keyframe requests; a receiver that never asks ACKs
/// frames only. Kept here so the silence is explicit in the test.
async fn ack_until_without_requests(
    media: &mut Ws,
    until: Instant,
    log: &mut Receiver,
    gate: &mut fake_wss_host::protocol::ReceiveGate,
) {
    use futures_util::StreamExt;
    use tokio::time::timeout;
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let Some(remaining) = until.checked_duration_since(Instant::now()) else {
            return;
        };
        match timeout(remaining, media.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                let (header, _) = fake_wss_host::protocol::decode_frame(&bytes).expect("frame");
                send_json(
                    media,
                    serde_json::json!({"type": "ack", "generation": header.generation, "seq": header.seq, "recvTsMs": 0}),
                )
                .await;
                log.frames.push(Got {
                    generation: header.generation,
                    seq: header.seq,
                    key: header.key,
                    codec: header.codec,
                });
                let _ = gate.accept(&header);
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) => {
                log.closed = true;
                return;
            }
            Ok(Some(Ok(_))) => {}
            Err(_) => return,
        }
    }
}
