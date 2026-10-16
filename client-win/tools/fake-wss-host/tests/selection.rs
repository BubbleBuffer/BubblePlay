//! Both codecs offered in one run, with no codec named on the command line:
//! `media-control start` chooses between them. Synthetic sources, so these run
//! without the F: recordings.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use fake_wss_host::model::{Injections, Source};
use fake_wss_host::protocol::{CODEC_H264, CODEC_HEVC};
use fake_wss_host::stream::Codec;

fn both_codecs() -> Vec<Arc<dyn Source>> {
    vec![
        synthetic_codec(Codec::H264, 120, 30),
        synthetic_codec(Codec::Hevc, 120, 30),
    ]
}

#[tokio::test]
async fn each_codec_starts_without_a_cli_selection() {
    for (name, byte) in [("h264", CODEC_H264), ("hevc", CODEC_HEVC)] {
        let harness = spawn_many(
            both_codecs(),
            Injections::default(),
            Duration::from_millis(1200),
            Duration::from_millis(100),
        )
        .await;
        let (log, _) = ack_session(
            harness.addr,
            name,
            Instant::now() + Duration::from_millis(600),
        )
        .await;
        let snap = harness.finish().await;

        assert!(!log.frames.is_empty(), "{name}: frames arrived");
        assert!(
            log.frames.iter().all(|f| f.codec == byte),
            "{name}: every frame carries the started codec"
        );
        assert_eq!(
            snap.counters.negotiated_codec.as_deref(),
            Some(name),
            "{name}: negotiated codec"
        );
        assert_eq!(snap.counters.codec_mismatches, 0, "{name}: no mismatch");
        assert_eq!(snap.in_flight_at_exit, 0, "{name}: nothing left in flight");
    }
}

#[tokio::test]
async fn codec_switches_back_and_forth_within_one_run() {
    let harness = spawn_many(
        both_codecs(),
        Injections::default(),
        Duration::from_millis(1800),
        Duration::from_millis(100),
    )
    .await;
    let addr = harness.addr;
    let (a, _) = ack_session(addr, "h264", Instant::now() + Duration::from_millis(400)).await;
    let (b, _) = ack_session(addr, "hevc", Instant::now() + Duration::from_millis(400)).await;
    let (c, _) = ack_session(addr, "h264", Instant::now() + Duration::from_millis(400)).await;
    let snap = harness.finish().await;

    assert!(
        !a.frames.is_empty() && a.frames.iter().all(|f| f.codec == CODEC_H264),
        "first session is H.264"
    );
    assert!(
        !b.frames.is_empty() && b.frames.iter().all(|f| f.codec == CODEC_HEVC),
        "second session is HEVC"
    );
    assert!(
        !c.frames.is_empty() && c.frames.iter().all(|f| f.codec == CODEC_H264),
        "third session is H.264"
    );
    assert!(
        c.frames.first().is_some_and(|f| f.key),
        "a switch starts at an IDR"
    );
    assert_eq!(snap.counters.negotiated_codec.as_deref(), Some("h264"));
    assert_eq!(snap.counters.starts, 3);
}
