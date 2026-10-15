//! The recorded spike streams end to end: parse, verify against evidence.json,
//! offer both codecs from one run, and check every frame ACKed. No codec is
//! passed on the command line; each run chooses it through `start`. Skipped
//! (with a SKIPPED line) when the F: recordings are not present.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use fake_wss_host::evidence::{self, Evidence};
use fake_wss_host::model::{Injections, Source};
use fake_wss_host::protocol::{CODEC_H264, CODEC_HEVC};
use fake_wss_host::report::{checks, RunMeta, StreamInfo};
use fake_wss_host::stream::{Codec, Stream};

const DIR: &str = "F:/build/spike-streams";

/// Both recordings, each verified against evidence.json, or `None` with a
/// SKIPPED line when either is missing.
fn recordings(test: &str) -> Option<Vec<(Evidence, Arc<dyn Source>)>> {
    let dir = Path::new(DIR);
    if !dir.join("evidence.json").exists() {
        eprintln!("SKIPPED {test}: no evidence.json in {DIR}");
        return None;
    }
    let mut out = Vec::new();
    for codec in [Codec::H264, Codec::Hevc] {
        if !dir.join(codec.file_name()).exists() {
            eprintln!(
                "SKIPPED {test}: no {} recording in {DIR}",
                codec.wire_name()
            );
            return None;
        }
        let evidence = evidence::load(dir, codec).expect("evidence.json entry");
        let stream = Stream::load(codec, &dir.join(codec.file_name())).expect("parse recording");
        evidence::verify(&stream, &evidence).expect("recording agrees with evidence.json");
        assert_eq!(evidence.fps, 60);
        assert_eq!(evidence.bitrate_bps, 20_000_000);
        out.push((evidence, Arc::new(stream) as Arc<dyn Source>));
    }
    Some(out)
}

fn run_meta(
    recs: &[(Evidence, Arc<dyn Source>)],
    injections: Injections,
    exit_after_s: f64,
    bind: String,
) -> RunMeta {
    RunMeta {
        constraint: None,
        streams: recs
            .iter()
            .map(|(_, s)| StreamInfo {
                codec: s.codec().wire_name(),
                width: s.width(),
                height: s.height(),
            })
            .collect(),
        fps: recs[0].0.fps,
        bitrate_bps: recs[0].0.bitrate_bps,
        exit_after_s,
        drain_ms: 500,
        bind,
        injections,
        audio: None,
        rtc: None,
    }
}

fn codec_byte(codec: Codec) -> u8 {
    match codec {
        Codec::H264 => CODEC_H264,
        Codec::Hevc => CODEC_HEVC,
    }
}

/// Offer both recordings, start `start` with injections, and check the run.
async fn run_recorded(start: Codec) {
    let test = format!(
        "recorded_{}_starts_without_a_cli_selection",
        start.wire_name()
    );
    let Some(recs) = recordings(&test) else {
        return;
    };
    let sources: Vec<Arc<dyn Source>> = recs.iter().map(|(_, s)| s.clone()).collect();
    let name = start.wire_name();

    let injections = Injections {
        seq_gap_at: Some(Duration::from_millis(500)),
        generation_at: Some(Duration::from_millis(1000)),
        socket_drop_at: Some(Duration::from_millis(1500)),
    };
    let harness = spawn_many(
        sources,
        injections,
        Duration::from_millis(3500),
        Duration::from_millis(500),
    )
    .await;
    let addr = harness.addr;
    let (first, epoch1) =
        ack_session(addr, name, Instant::now() + Duration::from_millis(2500)).await;
    let (second, epoch2) =
        ack_session(addr, name, Instant::now() + Duration::from_millis(1800)).await;
    let snap = harness.finish().await;

    assert!(first.closed, "{name}: the drop closed the media socket");
    assert_ne!(epoch1, epoch2, "{name}: a new epoch after the drop");
    assert!(
        first.frames.len() > 60,
        "{name}: {} frames in session one",
        first.frames.len()
    );
    assert!(
        second.frames.first().is_some_and(|f| f.key),
        "{name}: the new session starts at an IDR"
    );
    assert!(
        first
            .frames
            .iter()
            .chain(&second.frames)
            .all(|f| f.codec == codec_byte(start)),
        "{name}: every frame carries the codec that start chose"
    );
    assert_eq!(snap.counters.negotiated_codec.as_deref(), Some(name));
    assert_eq!(snap.in_flight_at_exit, 0, "{name}: nothing left in flight");

    let meta = run_meta(&recs, injections, 3.5, addr.to_string());
    let list = checks(&meta, &snap);
    for check in &list {
        eprintln!(
            "{name} {}: {} ({})",
            check.name,
            if check.pass { "pass" } else { "FAIL" },
            check.detail
        );
    }
    for recovery in &snap.recoveries {
        eprintln!(
            "{name} recovery {}: {:?} ms",
            recovery.kind.name(),
            recovery.recovery_ms()
        );
    }
    assert!(list.iter().all(|c| c.pass), "{list:#?}");
}

#[tokio::test]
async fn recorded_h264_starts_without_a_cli_selection_and_every_frame_is_acked() {
    run_recorded(Codec::H264).await;
}

#[tokio::test]
async fn recorded_hevc_starts_without_a_cli_selection_and_every_frame_is_acked() {
    run_recorded(Codec::Hevc).await;
}

#[tokio::test]
async fn recorded_codec_switches_on_start_within_one_run() {
    let Some(recs) = recordings("recorded_codec_switches_on_start_within_one_run") else {
        return;
    };
    let sources: Vec<Arc<dyn Source>> = recs.iter().map(|(_, s)| s.clone()).collect();
    let harness = spawn_many(
        sources,
        Injections::default(),
        Duration::from_millis(3000),
        Duration::from_millis(300),
    )
    .await;
    let addr = harness.addr;
    let (first, _) = ack_session(addr, "h264", Instant::now() + Duration::from_millis(1000)).await;
    let (second, _) = ack_session(addr, "hevc", Instant::now() + Duration::from_millis(1000)).await;
    let snap = harness.finish().await;

    assert!(!first.frames.is_empty(), "session one streamed");
    assert!(
        first.frames.iter().all(|f| f.codec == CODEC_H264),
        "session one is H.264"
    );
    assert!(!second.frames.is_empty(), "session two streamed");
    assert!(
        second.frames.iter().all(|f| f.codec == CODEC_HEVC),
        "session two is HEVC"
    );
    assert_eq!(snap.counters.negotiated_codec.as_deref(), Some("hevc"));
    let list = checks(
        &run_meta(&recs, Injections::default(), 3.0, addr.to_string()),
        &snap,
    );
    assert!(list.iter().all(|c| c.pass), "{list:#?}");
}
