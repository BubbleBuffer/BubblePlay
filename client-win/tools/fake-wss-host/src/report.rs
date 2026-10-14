//! The JSON report: counts, recovery evidence and pass/fail checks. The same
//! document goes to stdout and, with `--stats`, to a file.

use serde_json::{json, Value};

use crate::model::{Injection, Injections, Recovery};
use crate::server::Snapshot;

/// One recording the run offered to the browser.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamInfo {
    pub codec: &'static str,
    pub width: u32,
    pub height: u32,
}

pub struct RunMeta {
    /// The `--stream` constraint, if one was given.
    pub constraint: Option<&'static str>,
    /// Every recording offered; `start` chooses among them.
    pub streams: Vec<StreamInfo>,
    pub fps: u32,
    pub bitrate_bps: u64,
    pub exit_after_s: f64,
    pub drain_ms: u64,
    pub bind: String,
    pub injections: Injections,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub pass: bool,
    pub detail: String,
}

fn check(name: &'static str, pass: bool, detail: String) -> Check {
    Check { name, pass, detail }
}

pub fn checks(meta: &RunMeta, snap: &Snapshot) -> Vec<Check> {
    let c = &snap.counters;
    let accounted =
        c.frames_retired_by_ack + c.frames_lost_to_disconnect + c.frames_forgotten_on_stall;
    let mut out = vec![
        check(
            "frames-sent",
            c.frames_sent > 0,
            format!("{} frames", c.frames_sent),
        ),
        check(
            "frames-accounted-for",
            snap.in_flight_at_exit == 0 && c.frames_sent == accounted,
            format!(
                "sent {} = acked {} + lost-to-disconnect {} + forgotten {}; in flight at exit {}",
                c.frames_sent,
                c.frames_retired_by_ack,
                c.frames_lost_to_disconnect,
                c.frames_forgotten_on_stall,
                snap.in_flight_at_exit
            ),
        ),
        check(
            "no-stall-expiry",
            c.frames_forgotten_on_stall == 0,
            format!("{} forgotten", c.frames_forgotten_on_stall),
        ),
        check(
            "flow-cap",
            c.max_inflight <= 4,
            format!("max in flight {}", c.max_inflight),
        ),
        check(
            "no-outbound-overflow",
            c.outbound_overflow == 0,
            format!("{} overflowed", c.outbound_overflow),
        ),
        check(
            "acks-received",
            c.acks_received > 0,
            format!("{} ACKs", c.acks_received),
        ),
        check(
            "start-accepted",
            c.starts >= 1,
            format!("{} starts", c.starts),
        ),
        check(
            "codec-negotiated",
            c.negotiated_codec
                .as_deref()
                .is_some_and(|codec| meta.streams.iter().any(|s| s.codec == codec)),
            format!(
                "negotiated {}",
                c.negotiated_codec.as_deref().unwrap_or("none")
            ),
        ),
        check(
            "no-unhandled-media-rejects",
            c.rejected_control == 0,
            format!("{} rejected control messages", c.rejected_control),
        ),
    ];
    let injected = [
        (meta.injections.seq_gap_at.is_some(), Injection::SeqGap),
        (
            meta.injections.generation_at.is_some(),
            Injection::GenerationChange,
        ),
        (
            meta.injections.socket_drop_at.is_some(),
            Injection::SocketDrop,
        ),
    ];
    for (configured, kind) in injected {
        if !configured {
            continue;
        }
        let rows: Vec<&Recovery> = snap.recoveries.iter().filter(|r| r.kind == kind).collect();
        let recovered = rows.iter().all(|r| r.recovered_ms.is_some()) && !rows.is_empty();
        out.push(check(
            match kind {
                Injection::SeqGap => "seq-gap-recovered",
                Injection::GenerationChange => "generation-change-recovered",
                Injection::SocketDrop => "socket-drop-recovered",
            },
            recovered,
            format!(
                "{} injected, recovered {}",
                rows.len(),
                rows.iter().filter(|r| r.recovered_ms.is_some()).count()
            ),
        ));
    }
    if meta.injections.socket_drop_at.is_some() {
        let first = snap.epochs.first();
        let last = snap.epochs.last();
        let rotated = snap.epochs.len() >= 2 && first != last;
        out.push(check(
            "new-epoch-after-drop",
            rotated && c.socket_drops == 1,
            format!(
                "{} epochs issued, first != last: {}",
                snap.epochs.len(),
                rotated
            ),
        ));
    }
    out
}

pub fn build(meta: &RunMeta, snap: &Snapshot) -> Value {
    let c = &snap.counters;
    let negotiated = c
        .negotiated_codec
        .as_deref()
        .and_then(|codec| meta.streams.iter().find(|s| s.codec == codec));
    let checks = checks(meta, snap);
    let pass = checks.iter().all(|c| c.pass);
    let elapsed_s = snap.elapsed_ms / 1000.0;
    let recoveries: Vec<Value> = snap
        .recoveries
        .iter()
        .map(|r| {
            json!({
                "kind": r.kind.name(),
                "fired_ms": round(r.fired_ms),
                "keyframe_request": r.keyframe_request,
                "keyframe_requested_ms": r.keyframe_requested_ms.map(round),
                "idr": r.idr.map(|(generation, seq)| json!({"generation": generation, "seq": seq})),
                "idr_sent_ms": r.idr_sent_ms.map(round),
                "recovered_ms": r.recovered_ms.map(round),
                "recovery_ms": r.recovery_ms().map(round),
                "acknowledgements": r.acknowledgements.iter().map(|ack| json!({
                    "generation":ack.generation,"seq":ack.seq,"acked_at_ms":round(ack.received_ms)
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "tool": "fake-wss-host",
        "version": env!("CARGO_PKG_VERSION"),
        "stream": {
            "negotiated_codec": c.negotiated_codec,
            "constraint": meta.constraint,
            "available": meta.streams.iter().map(|s| json!({"codec": s.codec, "width": s.width, "height": s.height})).collect::<Vec<_>>(),
            "width": negotiated.map(|s| s.width),
            "height": negotiated.map(|s| s.height),
            "fps": meta.fps,
            "bitrate_bps": meta.bitrate_bps,
        },
        "run": {
            "bind": meta.bind,
            "exit_after_s": meta.exit_after_s,
            "drain_ms": meta.drain_ms,
            "elapsed_ms": round(snap.elapsed_ms),
            "in_flight_at_exit": snap.in_flight_at_exit,
            "hard_stop": snap.hard_stop,
            "fps_observed": round(c.frames_sent as f64 / elapsed_s.max(0.001)),
        },
        "frames": {
            "sent": c.frames_sent,
            "keyframes": c.keyframes_sent,
            "delta": c.delta_frames_sent,
            "parameter_sets_injected": c.parameter_sets_injected,
            "loops": c.loops,
            "max_in_flight": c.max_inflight,
            "backpressure_holds": c.backpressure_holds,
            "keyframe_seeks": c.keyframe_seeks,
            "keyframe_timeouts": c.keyframe_timeouts,
            "outbound_overflow": c.outbound_overflow,
        },
        "acks": {
            "received": c.acks_received,
            "retired": c.frames_retired_by_ack,
            "duplicate_or_stale": c.acks_duplicate_or_stale,
            "unknown_generation": c.acks_unknown_generation,
            "forgotten_on_stall": c.frames_forgotten_on_stall,
            "lost_to_disconnect": c.frames_lost_to_disconnect,
        },
        "epochs": {
            "issued": c.epochs_issued,
            "list": snap.epochs,
        },
        "media": {
            "attaches": c.media_attaches,
            "detaches": c.media_detaches,
            "pings": c.media_pings,
            "stale_epoch_refused": c.stale_epoch_control,
        },
        "control": {
            "starts": c.starts,
            "stops": c.stops,
            "codec_mismatches": c.codec_mismatches,
            "first_frame_requests": c.first_frame_requests,
            "first_frame_presented": c.first_frame_presented,
            "keyframe_requests": c.keyframe_requests,
            "unhandled": c.unhandled_control,
            "rejected": c.rejected_control,
        },
        "injections": {
            "seq_gaps": c.seq_gaps_injected,
            "generation_changes": c.generation_changes_injected,
            "socket_drops": c.socket_drops,
        },
        "recoveries": recoveries,
        "checks": checks.iter().map(|c| json!({"name": c.name, "pass": c.pass, "detail": c.detail})).collect::<Vec<_>>(),
        "pass": pass,
    })
}

fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Counters;
    use std::time::Duration;

    fn meta(injections: Injections) -> RunMeta {
        RunMeta {
            constraint: None,
            streams: vec![
                StreamInfo {
                    codec: "h264",
                    width: 1920,
                    height: 1080,
                },
                StreamInfo {
                    codec: "hevc",
                    width: 1920,
                    height: 1080,
                },
            ],
            fps: 60,
            bitrate_bps: 20_000_000,
            exit_after_s: 30.0,
            drain_ms: 1000,
            bind: "127.0.0.1:0".into(),
            injections,
        }
    }

    fn healthy() -> Snapshot {
        let counters = Counters {
            frames_sent: 100,
            frames_retired_by_ack: 100,
            acks_received: 100,
            max_inflight: 1,
            starts: 1,
            negotiated_codec: Some("h264".into()),
            ..Counters::default()
        };
        Snapshot {
            counters,
            recoveries: Vec::new(),
            epochs: vec!["a".repeat(32)],
            in_flight_at_exit: 0,
            elapsed_ms: 2000.0,
            hard_stop: false,
        }
    }

    fn failed(name: &str, list: &[Check]) -> bool {
        list.iter().any(|c| c.name == name && !c.pass)
    }

    #[test]
    fn a_clean_run_passes_every_check() {
        let list = checks(&meta(Injections::default()), &healthy());
        assert!(list.iter().all(|c| c.pass), "{list:?}");
    }

    #[test]
    fn an_unaccounted_frame_fails_the_accounting_check() {
        let mut snap = healthy();
        snap.counters.frames_sent = 101;
        snap.in_flight_at_exit = 1;
        assert!(failed(
            "frames-accounted-for",
            &checks(&meta(Injections::default()), &snap)
        ));
    }

    #[test]
    fn a_frame_lost_to_the_socket_drop_is_accounted_not_missing() {
        let mut snap = healthy();
        snap.counters.frames_sent = 103;
        snap.counters.frames_lost_to_disconnect = 3;
        snap.counters.frames_retired_by_ack = 100;
        let injections = Injections {
            socket_drop_at: Some(Duration::from_secs(1)),
            ..Injections::default()
        };
        snap.counters.socket_drops = 1;
        snap.epochs = vec!["a".repeat(32), "b".repeat(32)];
        snap.recoveries = vec![Recovery {
            kind: Injection::SocketDrop,
            fired_ms: 1000.0,
            keyframe_request: None,
            keyframe_requested_ms: None,
            idr: Some((3, 500)),
            idr_sent_ms: Some(1010.0),
            recovered_ms: Some(1040.0),
            acknowledgements: Vec::new(),
        }];
        let list = checks(&meta(injections), &snap);
        assert!(list.iter().all(|c| c.pass), "{list:?}");
    }

    #[test]
    fn a_missing_epoch_change_fails_after_a_drop() {
        let mut snap = healthy();
        snap.counters.socket_drops = 1;
        let injections = Injections {
            socket_drop_at: Some(Duration::from_secs(1)),
            ..Injections::default()
        };
        assert!(failed(
            "new-epoch-after-drop",
            &checks(&meta(injections), &snap)
        ));
    }

    #[test]
    fn the_report_carries_the_recovery_evidence() {
        let snap = healthy();
        let report = build(&meta(Injections::default()), &snap);
        assert_eq!(report["pass"], json!(true));
        assert_eq!(report["frames"]["sent"], json!(100));
        assert_eq!(report["stream"]["width"], json!(1920));
        assert_eq!(report["stream"]["height"], json!(1080));
        assert_eq!(report["stream"]["negotiated_codec"], json!("h264"));
        assert_eq!(report["stream"]["constraint"], json!(null));
        assert_eq!(report["stream"]["available"].as_array().unwrap().len(), 2);
        assert!(report["checks"].as_array().unwrap().len() >= 9);
    }

    #[test]
    fn a_run_that_never_negotiated_a_codec_fails_the_check() {
        let mut snap = healthy();
        snap.counters.negotiated_codec = None;
        assert!(failed(
            "codec-negotiated",
            &checks(&meta(Injections::default()), &snap)
        ));
    }

    #[test]
    fn a_negotiated_codec_that_was_not_offered_fails_the_check() {
        let mut snap = healthy();
        snap.counters.negotiated_codec = Some("av1".into());
        assert!(failed(
            "codec-negotiated",
            &checks(&meta(Injections::default()), &snap)
        ));
    }

    #[test]
    fn the_report_names_the_codec_that_was_negotiated_last() {
        let mut snap = healthy();
        snap.counters.negotiated_codec = Some("hevc".into());
        let report = build(&meta(Injections::default()), &snap);
        assert_eq!(report["stream"]["negotiated_codec"], json!("hevc"));
        assert_eq!(report["stream"]["width"], json!(1920));
    }
}
