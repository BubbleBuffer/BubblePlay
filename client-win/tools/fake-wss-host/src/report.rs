//! The JSON report: counts, recovery evidence and pass/fail checks. The same
//! document goes to stdout and, with `--stats`, to a file.

use serde_json::{json, Value};

use crate::audio::{AudioCounters, Script};
use crate::model::{Injection, Injections, Recovery};
use crate::rtc;
use crate::server::Snapshot;

/// The two input lanes the browser opens, in the order it creates them.
const INPUT_LANES: [&str; 2] = ["input-control", "input-motion"];

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
    /// The audio lane's script, when `--audio` is given.
    pub audio: Option<Script>,
    /// The `--webrtc` knobs, when the RTC answerer ran.
    pub rtc: Option<rtc::Knobs>,
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

/// The checks for the lanes this run used. A run without `--webrtc` is judged
/// on WSS alone, as before. With it, the RTC session is judged on its own, and
/// the WSS checks apply only when WSS carried media (a `start` was accepted,
/// frames went out, or audio was opted in).
pub fn checks(meta: &RunMeta, snap: &Snapshot) -> Vec<Check> {
    let Some(knobs) = meta.rtc else {
        return wss_checks(meta, snap);
    };
    let mut out = rtc_checks(meta, knobs, &snap.rtc);
    if wss_used(snap) {
        out.extend(wss_checks(meta, snap));
    } else {
        out.push(rejects_check(snap));
    }
    out
}

/// Whether the WSS lane carried anything: a start, a frame, or an audio opt-in.
fn wss_used(snap: &Snapshot) -> bool {
    snap.counters.starts >= 1
        || snap.counters.frames_sent > 0
        || snap.audio.as_ref().is_some_and(|a| a.opt_in_starts > 0)
}

/// Video checks for a run with recordings, audio checks when `--audio` is on.
/// An audio-only run has no video to judge, so only the rejects check applies.
fn wss_checks(meta: &RunMeta, snap: &Snapshot) -> Vec<Check> {
    let mut out = if meta.streams.is_empty() {
        vec![rejects_check(snap)]
    } else {
        video_checks(meta, snap)
    };
    if let (Some(script), Some(audio)) = (meta.audio, snap.audio.as_ref()) {
        out.extend(audio_checks(script, audio));
    }
    out
}

fn rejects_check(snap: &Snapshot) -> Check {
    check(
        "no-unhandled-media-rejects",
        snap.counters.rejected_control == 0,
        format!(
            "{} rejected control messages",
            snap.counters.rejected_control
        ),
    )
}

/// The RTC session's checks, with each knob's effect judged as requested. A
/// blocked run must stay silent; a stalled run must send audio but no video; a
/// dropped run must end once with the DTLS close_notify sent.
fn rtc_checks(meta: &RunMeta, knobs: rtc::Knobs, sessions: &[rtc::Report]) -> Vec<Check> {
    let connected = sessions.iter().filter(|s| s.connected_ms.is_some()).count();
    let ended: Vec<&str> = sessions.iter().map(|s| s.ended.as_str()).collect();
    let mut out = vec![
        check(
            "rtc-session-offered",
            !sessions.is_empty(),
            format!("{} sessions, ended {ended:?}", sessions.len()),
        ),
        check(
            "rtc-no-failed-session",
            sessions
                .iter()
                .all(|s| !s.ended.starts_with("failed:") && s.error.is_none()),
            format!("ended {ended:?}"),
        ),
    ];
    let audio_sent: u64 = sessions.iter().map(|s| s.audio_rtp_packets).sum();
    if knobs.block_ice {
        let blocked: u64 = sessions.iter().map(|s| s.blocked_datagrams).sum();
        let silent = sessions.iter().all(|s| {
            s.connected_ms.is_none() && s.video_rtp_packets == 0 && s.audio_rtp_packets == 0
        });
        out.push(check(
            "rtc-blocked-as-requested",
            !sessions.is_empty() && silent && blocked > 0,
            format!("{blocked} datagrams blocked, {connected} connected"),
        ));
    } else {
        out.push(check(
            "rtc-connected",
            connected >= 1,
            format!("{connected} of {} sessions connected", sessions.len()),
        ));
        let lanes_open = sessions.iter().any(|s| {
            INPUT_LANES
                .iter()
                .all(|lane| s.channels.iter().any(|open| open == lane))
        });
        out.push(check(
            "rtc-input-lanes-open",
            lanes_open,
            format!(
                "lanes opened: {:?}",
                sessions.iter().map(|s| &s.channels).collect::<Vec<_>>()
            ),
        ));
        let echoes: u64 = sessions.iter().map(|s| s.echoes).sum();
        let requests: u64 = sessions.iter().map(|s| s.echo_requests).sum();
        out.push(check(
            "rtc-cursor-echoed",
            echoes == requests && sessions.iter().all(|s| s.echo_refused == 0),
            format!(
                "{echoes} cursor echoes for {requests} requests, {} refused",
                sessions.iter().map(|s| s.echo_refused).sum::<u64>()
            ),
        ));
        if knobs.stall_first_frame {
            let held: u64 = sessions.iter().map(|s| s.video_frames_suppressed).sum();
            let video_sent: u64 = sessions.iter().map(|s| s.video_rtp_packets).sum();
            out.push(check(
                "rtc-stalled-as-requested",
                connected >= 1 && video_sent == 0 && held > 0 && audio_sent > 0,
                format!(
                    "{held} frames held back, {video_sent} video and {audio_sent} audio packets"
                ),
            ));
        } else {
            if !meta.streams.is_empty() {
                let admitted: Vec<Option<u32>> =
                    sessions.iter().map(|s| s.admitted_rtp_timestamp).collect();
                let video = sessions.iter().any(|s| {
                    s.video_rtp_packets > 0
                        && s.admitted_rtp_timestamp.is_some()
                        && s.video_codec
                            .as_deref()
                            .is_some_and(|codec| meta.streams.iter().any(|st| st.codec == codec))
                });
                out.push(check(
                    "rtc-video-admitted-on-rtp",
                    video,
                    format!("admitted at RTP timestamps {admitted:?}"),
                ));
            }
            out.push(check(
                "rtc-audio-sent",
                audio_sent > 0,
                format!("{audio_sent} audio RTP packets"),
            ));
        }
    }
    if knobs.drop_at.is_some() {
        let dropped = sessions
            .iter()
            .filter(|s| s.ended == "rtc-dropped" && s.close_notify_sent)
            .count();
        out.push(check(
            "rtc-dropped-with-close-notify",
            dropped == 1,
            format!("{dropped} sessions dropped with a close_notify sent"),
        ));
    }
    out
}

fn audio_checks(script: Script, c: &AudioCounters) -> Vec<Check> {
    let accounted =
        c.packets_sent + c.lost_by_script + c.queue_dropped + c.send_failed + c.discarded;
    let mut out = vec![
        check(
            "audio-opted-in",
            c.opt_in_starts >= 1,
            format!("{} starts with audio:1", c.opt_in_starts),
        ),
        check(
            "audio-sent",
            c.packets_sent > 0,
            format!("{} packets, {} bytes", c.packets_sent, c.bytes_sent),
        ),
        check(
            "audio-accounted-for",
            c.packets_encoded == accounted,
            format!(
                "encoded {} = sent {} + lost-by-script {} + queue-dropped {} + send-failed {} + discarded {}",
                c.packets_encoded,
                c.packets_sent,
                c.lost_by_script,
                c.queue_dropped,
                c.send_failed,
                c.discarded
            ),
        ),
        check(
            "audio-no-send-failures",
            c.send_failed == 0,
            format!("{} refused by the socket", c.send_failed),
        ),
        check(
            "audio-discontinuity-marked",
            c.packets_sent == 0 || c.discontinuities_sent >= 1,
            format!("{} sent with the discontinuity flag", c.discontinuities_sent),
        ),
    ];
    if let Some(n) = script.loss_every {
        out.push(check(
            "audio-loss-script",
            c.lost_by_script == c.packets_encoded / n,
            format!(
                "{} lost of {} encoded, every {n}",
                c.lost_by_script, c.packets_encoded
            ),
        ));
    }
    if let Some(n) = script.reorder_every {
        out.push(check(
            "audio-reorder-script",
            c.packets_encoded < n || c.reordered > 0,
            format!("{} held back for reorder, every {n}", c.reordered),
        ));
    }
    out
}

fn video_checks(meta: &RunMeta, snap: &Snapshot) -> Vec<Check> {
    let c = &snap.counters;
    // A monitor switch retires the in-flight frames of the old display on purpose
    // (`Model::switch_monitor`), so they are accounted, not missing.
    let accounted = c.frames_retired_by_ack
        + c.frames_lost_to_disconnect
        + c.frames_forgotten_on_stall
        + c.frames_dropped_on_switch;
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
                "sent {} = acked {} + lost-to-disconnect {} + forgotten {} + dropped-on-switch {}; in flight at exit {}",
                c.frames_sent,
                c.frames_retired_by_ack,
                c.frames_lost_to_disconnect,
                c.frames_forgotten_on_stall,
                c.frames_dropped_on_switch,
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
                Injection::MonitorSwitch => "monitor-switch-recovered",
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
        "monitor": {
            "list_sent": c.monitor_list_sent,
            "refreshes": c.monitor_refreshes,
            "switches": c.monitor_switches,
            "switch_failures": c.monitor_switch_failures,
            "selected": c.monitor_selected,
            "frames_dropped_on_switch": c.frames_dropped_on_switch,
        },
        "stream_mode": {
            "current": c.stream_mode,
            "changes": c.stream_mode_changes,
        },
        "recoveries": recoveries,
        "audio": audio_json(meta, snap),
        "checks": checks.iter().map(|c| json!({"name": c.name, "pass": c.pass, "detail": c.detail})).collect::<Vec<_>>(),
        "pass": pass,
    })
}

fn audio_json(meta: &RunMeta, snap: &Snapshot) -> Value {
    match (meta.audio, snap.audio.as_ref()) {
        (Some(script), Some(c)) => json!({
            "enabled": true,
            "script": {"loss_every": script.loss_every, "reorder_every": script.reorder_every},
            "opt_in_starts": c.opt_in_starts,
            "starts": c.starts,
            "stops": c.stops,
            "resyncs": c.resyncs,
            "packets_encoded": c.packets_encoded,
            "packets_sent": c.packets_sent,
            "bytes_sent": c.bytes_sent,
            "discontinuities_sent": c.discontinuities_sent,
            "lost_by_script": c.lost_by_script,
            "reordered": c.reordered,
            "queue_dropped": c.queue_dropped,
            "send_failed": c.send_failed,
            "discarded": c.discarded,
            "encode_errors": c.encode_errors,
        }),
        _ => json!({"enabled": false}),
    }
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
            audio: None,
            rtc: None,
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
            audio: None,
            rtc: Vec::new(),
        }
    }

    fn audio_meta(script: Script) -> RunMeta {
        RunMeta {
            streams: Vec::new(),
            audio: Some(script),
            ..meta(Injections::default())
        }
    }

    fn audio_counters(encoded: u64, sent: u64, lost: u64) -> AudioCounters {
        AudioCounters {
            opt_in_starts: 1,
            starts: 1,
            packets_encoded: encoded,
            packets_sent: sent,
            bytes_sent: sent * 100,
            discontinuities_sent: 1,
            lost_by_script: lost,
            ..AudioCounters::default()
        }
    }

    #[test]
    fn an_audio_only_run_is_judged_on_audio_alone() {
        let mut snap = healthy();
        snap.counters = Counters::default();
        snap.audio = Some(audio_counters(10, 10, 0));
        let list = checks(&audio_meta(Script::default()), &snap);
        assert!(list.iter().all(|c| c.pass), "{list:?}");
        assert!(list.iter().any(|c| c.name == "audio-sent"));
        assert!(
            !list.iter().any(|c| c.name == "frames-sent"),
            "no video checks"
        );
    }

    #[test]
    fn audio_accounting_and_the_loss_script_are_checked() {
        let mut snap = healthy();
        snap.audio = Some(audio_counters(10, 8, 2));
        let script = Script::new(Some(5), None).expect("script");
        let list = checks(&meta_with_audio(script), &snap);
        assert!(list.iter().all(|c| c.pass), "{list:?}");
        // One packet unaccounted for: the accounting check fails.
        snap.audio = Some(audio_counters(10, 6, 2));
        assert!(failed(
            "audio-accounted-for",
            &checks(&meta_with_audio(script), &snap)
        ));
        // Two lost where the script names one per five: the loss check fails.
        snap.audio = Some(audio_counters(10, 5, 5));
        assert!(failed(
            "audio-loss-script",
            &checks(&meta_with_audio(script), &snap)
        ));
    }

    fn meta_with_audio(script: Script) -> RunMeta {
        RunMeta {
            audio: Some(script),
            ..meta(Injections::default())
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
    fn a_frame_retired_by_a_monitor_switch_is_accounted_not_missing() {
        let mut snap = healthy();
        snap.counters.frames_sent = 101;
        snap.counters.frames_retired_by_ack = 100;
        snap.counters.frames_dropped_on_switch = 1;
        snap.counters.monitor_switches = 1;
        let list = checks(&meta(Injections::default()), &snap);
        assert!(!failed("frames-accounted-for", &list), "{list:#?}");
    }

    #[test]
    fn a_frame_missing_beside_a_monitor_switch_still_fails() {
        let mut snap = healthy();
        snap.counters.frames_sent = 102;
        snap.counters.frames_retired_by_ack = 100;
        snap.counters.frames_dropped_on_switch = 1;
        snap.counters.monitor_switches = 1;
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

    fn rtc_meta(knobs: rtc::Knobs) -> RunMeta {
        RunMeta {
            rtc: Some(knobs),
            ..meta(Injections::default())
        }
    }

    /// A connected session that sent video and audio and echoed the cursor.
    fn rtc_session() -> rtc::Report {
        rtc::Report {
            ended: "run-complete".into(),
            video_codec: Some("h264".into()),
            video_pt: Some(96),
            audio_pt: Some(111),
            connected_ms: Some(120.0),
            first_keyframe_ms: Some(180.0),
            admitted_rtp_timestamp: Some(90_000),
            channels: INPUT_LANES.iter().map(|lane| lane.to_string()).collect(),
            video_frames: 60,
            video_rtp_packets: 600,
            audio_frames: 300,
            audio_rtp_packets: 300,
            echoes: 2,
            echo_requests: 2,
            ..rtc::Report::default()
        }
    }

    /// An RTC run with no WSS traffic: the counters a pure `--webrtc` run has.
    fn rtc_snapshot(sessions: Vec<rtc::Report>) -> Snapshot {
        let mut snap = healthy();
        snap.counters = Counters::default();
        snap.rtc = sessions;
        snap
    }

    #[test]
    fn a_pure_rtc_run_is_judged_on_rtc_not_wss() {
        let list = checks(
            &rtc_meta(rtc::Knobs::default()),
            &rtc_snapshot(vec![rtc_session()]),
        );
        assert!(list.iter().all(|c| c.pass), "{list:#?}");
        assert!(list.iter().any(|c| c.name == "rtc-video-admitted-on-rtp"));
        assert!(!list
            .iter()
            .any(|c| c.name == "frames-sent" || c.name == "acks-received"));
    }

    #[test]
    fn a_pure_rtc_run_with_no_session_fails() {
        let list = checks(&rtc_meta(rtc::Knobs::default()), &rtc_snapshot(Vec::new()));
        assert!(failed("rtc-session-offered", &list));
    }

    #[test]
    fn wss_checks_join_in_only_when_wss_carried_media() {
        let mut snap = rtc_snapshot(vec![rtc_session()]);
        snap.counters.starts = 1;
        snap.counters.frames_sent = 0;
        let list = checks(&rtc_meta(rtc::Knobs::default()), &snap);
        assert!(
            failed("frames-sent", &list),
            "a WSS start with no frames fails"
        );
        snap.counters = Counters {
            frames_sent: 100,
            frames_retired_by_ack: 100,
            acks_received: 100,
            starts: 1,
            negotiated_codec: Some("h264".into()),
            ..Counters::default()
        };
        snap.in_flight_at_exit = 0;
        let list = checks(&rtc_meta(rtc::Knobs::default()), &snap);
        assert!(list.iter().all(|c| c.pass), "{list:#?}");
    }

    #[test]
    fn a_blocked_run_must_stay_silent() {
        let knobs = rtc::Knobs {
            block_ice: true,
            ..rtc::Knobs::default()
        };
        let blocked = rtc::Report {
            ended: "run-complete".into(),
            blocked_datagrams: 40,
            ..rtc::Report::default()
        };
        let list = checks(&rtc_meta(knobs), &rtc_snapshot(vec![blocked.clone()]));
        assert!(list.iter().all(|c| c.pass), "{list:#?}");
        let connected = rtc::Report {
            connected_ms: Some(50.0),
            ..blocked
        };
        let list = checks(&rtc_meta(knobs), &rtc_snapshot(vec![connected]));
        assert!(failed("rtc-blocked-as-requested", &list));
    }

    #[test]
    fn a_stalled_run_sends_audio_and_no_video() {
        let knobs = rtc::Knobs {
            stall_first_frame: true,
            ..rtc::Knobs::default()
        };
        let stalled = rtc::Report {
            ended: "run-complete".into(),
            connected_ms: Some(120.0),
            channels: INPUT_LANES.iter().map(|lane| lane.to_string()).collect(),
            video_frames_suppressed: 90,
            audio_rtp_packets: 300,
            echoes: 1,
            echo_requests: 1,
            ..rtc::Report::default()
        };
        let list = checks(&rtc_meta(knobs), &rtc_snapshot(vec![stalled.clone()]));
        assert!(list.iter().all(|c| c.pass), "{list:#?}");
        let leaked = rtc::Report {
            video_rtp_packets: 5,
            ..stalled
        };
        let list = checks(&rtc_meta(knobs), &rtc_snapshot(vec![leaked]));
        assert!(failed("rtc-stalled-as-requested", &list));
    }

    #[test]
    fn a_drop_must_send_the_close_notify() {
        let knobs = rtc::Knobs {
            drop_at: Some(Duration::from_secs(1)),
            ..rtc::Knobs::default()
        };
        let dropped = rtc::Report {
            ended: "rtc-dropped".into(),
            close_notify_sent: true,
            ..rtc_session()
        };
        let list = checks(&rtc_meta(knobs), &rtc_snapshot(vec![dropped.clone()]));
        assert!(list.iter().all(|c| c.pass), "{list:#?}");
        let silent = rtc::Report {
            close_notify_sent: false,
            ..dropped
        };
        let list = checks(&rtc_meta(knobs), &rtc_snapshot(vec![silent]));
        assert!(failed("rtc-dropped-with-close-notify", &list));
    }
}
