//! Session-level tests: the real `run_with_timing` talks to the fake relay on
//! 127.0.0.1 and the tests read what comes out of the event channel and the relay log.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::config::{Codec, NetConfig};
use super::cookies::CookieJar;
use super::error::NetError;
use super::events::{DecoderFeedback, NetEvent, ResetEvent, VideoFrame};
use super::fake_relay::{FakeRelay, Logged, Scenario, Step};
use super::protocol::{encode_frame, FrameHeader, V4_HEADER_BYTES};
use super::session::{run_with_timing, Timing};
use super::stats::{NetState, TransportStats};

const EPOCH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const EPOCH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const WAIT: Duration = Duration::from_secs(5);
/// A relay log is quiet once nothing arrives for this long.
const QUIET: Duration = Duration::from_millis(150);

fn timing() -> Timing {
    Timing {
        tick: Duration::from_millis(5),
        handshake: Duration::from_secs(2),
        heartbeat: Duration::from_secs(10),
        dead: Duration::from_secs(10),
        first_frame: Duration::from_secs(10),
        keyframe_interval: Duration::from_millis(250),
        backoff_min: Duration::from_millis(10),
        backoff_max: Duration::from_millis(50),
    }
}

fn cookies() -> CookieJar {
    CookieJar::from_set_cookie([
        "__Host-gate=gate-1; Path=/; Secure",
        "__Host-bp-session=sess-1; Path=/; Secure",
    ])
}

fn frame(key: bool, generation: u32, seq: u32, payload: &[u8]) -> Vec<u8> {
    encode_frame(
        &FrameHeader {
            key,
            refine: false,
            codec: Codec::H264,
            generation,
            seq,
            capture_ts_us: 1_000,
            width: 640,
            height: 360,
        },
        payload,
    )
}

/// A keyframe step; the payload names its sequence so tests can check it.
fn key(seq: u32) -> Step {
    Step::Binary(frame(true, 7, seq, format!("payload-{seq}").as_bytes()))
}

fn delta(seq: u32) -> Step {
    Step::Binary(frame(false, 7, seq, format!("payload-{seq}").as_bytes()))
}

fn reset(local_epoch: u64, reason: &str) -> NetEvent {
    NetEvent::Reset(ResetEvent {
        local_epoch,
        reason: reason.to_owned(),
    })
}

fn scenario(epoch: &str, media: Vec<Step>) -> Scenario {
    Scenario {
        media,
        ..Scenario::new(epoch)
    }
}

struct Harness {
    relay: FakeRelay,
    events: Receiver<NetEvent>,
    feedback: Sender<DecoderFeedback>,
    stop: Arc<AtomicBool>,
    stats: Arc<TransportStats>,
    handle: Option<JoinHandle<Result<(), NetError>>>,
}

fn start(scenarios: Vec<Scenario>, timing: Timing, capacity: usize) -> Harness {
    start_audio(scenarios, timing, capacity, None)
}

fn start_audio(
    scenarios: Vec<Scenario>,
    timing: Timing,
    capacity: usize,
    audio: Option<crate::audio::AudioHandle>,
) -> Harness {
    let relay = FakeRelay::start(scenarios);
    let mut config = NetConfig::new(relay.origin.clone(), Codec::H264, cookies());
    config.audio = audio;
    let (event_tx, events) = mpsc::sync_channel(capacity);
    let (feedback, feedback_rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(TransportStats::new());
    let handle = {
        let (stop, stats) = (stop.clone(), stats.clone());
        thread::spawn(move || run_with_timing(config, event_tx, feedback_rx, stop, stats, timing))
    };
    Harness {
        relay,
        events,
        feedback,
        stop,
        stats,
        handle: Some(handle),
    }
}

fn audio_packet(seq: u32, discontinuity: bool) -> Vec<u8> {
    let mut encoder =
        opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio).unwrap();
    let mut payload = [0; 1500];
    let len = encoder.encode_float(&[0.0; 960], &mut payload).unwrap();
    let mut packet = b"BPA1".to_vec();
    packet.extend_from_slice(&[1, u8::from(discontinuity), 1, 2]);
    packet.extend_from_slice(&48_000u32.to_be_bytes());
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(&(u64::from(seq) * 10_000).to_be_bytes());
    packet.extend_from_slice(&480u16.to_be_bytes());
    packet.extend_from_slice(&[0, 0]);
    packet.extend_from_slice(&payload[..len]);
    packet
}

#[test]
fn audio_is_opted_in_and_demuxed_without_video_acks_or_keyframes() {
    let player = crate::audio::AudioPlayer::new(crate::audio::OutputMode::Fake).unwrap();
    let audio = player.handle();
    let steps = vec![
        Step::Pause(Duration::from_millis(30)),
        Step::Binary(audio_packet(0, true)),
        Step::Pause(Duration::from_millis(10)),
        Step::Binary(audio_packet(2, false)),
        Step::Binary(audio_packet(1, false)),
        Step::Binary(b"BPA1malformed".to_vec()),
        Step::Binary(audio_packet(4, false)),
    ];
    let mut first = scenario(EPOCH_A, steps);
    first.host_present = true;
    let h = start_audio(vec![first], timing(), 8, Some(audio.clone()));
    wait_for(|| audio.stats().decoded == 4 && audio.stats().concealed >= 1);
    assert_eq!(audio.stats().received, 5);
    assert_eq!(audio.stats().rejected, 1);
    assert_eq!(audio.stats().late, 0);
    let log = drain_log(&h.relay.log, QUIET);
    let signaling = texts(&log, "/ws");
    assert_eq!(
        signaling.iter().find(|v| v["action"] == "start").unwrap()["audio"],
        1
    );
    assert!(ack_seqs(&log).is_empty());
    assert!(keyframe_reasons(&log).is_empty());
    assert_eq!(h.stats.snapshot().frames_received, 0);
    assert_eq!(h.stats.snapshot().rejected_packets, 0);
    h.finish().unwrap();
}

#[test]
fn audio_survives_video_regates_and_reconnect_discards_the_old_epoch() {
    let player = crate::audio::AudioPlayer::new(crate::audio::OutputMode::Fake).unwrap();
    let audio = player.handle();
    let mut first = scenario(
        EPOCH_A,
        vec![
            Step::Binary(audio_packet(0, true)),
            Step::Text(json!({"type":"media-peer","present":true}).to_string()),
            Step::Binary(frame(false, 9, 88, b"video gap")),
            Step::Pause(Duration::from_millis(20)),
            Step::Binary(audio_packet(1, false)),
        ],
    );
    first.host_present = true;
    let mut second = scenario(
        EPOCH_B,
        vec![
            Step::Binary(audio_packet(0, true)),
            Step::Binary(audio_packet(1, false)),
            Step::Binary(audio_packet(2, false)),
        ],
    );
    second.host_present = true;
    let mut short = timing();
    short.dead = Duration::from_millis(180);
    short.heartbeat = Duration::from_secs(10);
    let h = start_audio(vec![first, second], short, 16, Some(audio.clone()));
    wait_for(|| audio.stats().epoch >= 2 && audio.stats().decoded >= 4);
    let epoch = audio.stats().epoch;
    assert!(!audio.push(epoch - 1, &audio_packet(100, false), Instant::now()));
    assert_eq!(audio.stats().decode_errors, 0);
    let log = drain_log(&h.relay.log, QUIET);
    let starts: Vec<_> = texts(&log, "/ws")
        .into_iter()
        .filter(|v| v["action"] == "start")
        .collect();
    assert!(starts.len() >= 2);
    assert!(starts.iter().all(|v| v["audio"] == 1));
    assert!(starts.iter().any(|v| v["epoch"] == EPOCH_B));
    h.finish().unwrap();
}

#[test]
fn video_generation_and_sequence_gaps_do_not_reset_audio_playout() {
    let player = crate::audio::AudioPlayer::new(crate::audio::OutputMode::Fake).unwrap();
    let audio = player.handle();
    let mut steps = Vec::new();
    for seq in 0..15 {
        // Bounded batches avoid relying on the OS's 10 ms sleep precision.
        if seq == 10 {
            steps.push(Step::Pause(Duration::from_millis(60)));
            steps.push(key(1));
            steps.push(Step::Binary(frame(false, 7, 99, b"sequence gap")));
            steps.push(Step::Binary(frame(true, 8, 1, b"generation change")));
        }
        steps.push(Step::Binary(audio_packet(seq, seq == 0)));
    }
    let mut first = scenario(EPOCH_A, steps);
    first.host_present = true;
    let h = start_audio(vec![first], timing(), 16, Some(audio.clone()));
    wait_for(|| audio.stats().received >= 15 && audio.stats().decoded >= 10);
    let sample = audio.stats();
    assert_eq!(sample.epoch, 1);
    assert_eq!(
        sample.transport_resets, 1,
        "only begin_epoch may reset audio: {sample:?}"
    );
    assert_eq!(sample.concealed, 0);
    // Scheduler lateness may drop PCM under machine contention; the contract
    // here is that video gaps never flush or reject the independent audio lane.
    assert_eq!(sample.rejected, 0);
    assert_eq!(sample.received, 15);
    h.finish().unwrap();
}

impl Harness {
    /// Stop the run and return its result.
    fn finish(mut self) -> Result<(), NetError> {
        self.stop.store(true, Relaxed);
        self.join()
    }

    fn join(&mut self) -> Result<(), NetError> {
        let deadline = Instant::now() + WAIT;
        while !self.handle.as_ref().expect("run handle").is_finished() {
            assert!(Instant::now() < deadline, "the run did not return in time");
            thread::sleep(Duration::from_millis(5));
        }
        self.handle
            .take()
            .unwrap()
            .join()
            .expect("run thread panicked")
    }

    fn drop_events(&mut self) {
        let (_, disconnected) = mpsc::channel();
        drop(std::mem::replace(&mut self.events, disconnected));
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        // Closing both relay lanes wakes a client even inside a socket await.
        self.relay.shutdown();
        if let Some(handle) = self.handle.take() {
            let deadline = Instant::now() + WAIT;
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if !handle.is_finished() {
                if thread::panicking() {
                    eprintln!("net test run did not stop within five seconds");
                    return;
                }
                panic!("net test run did not stop within five seconds");
            }
            if let Err(error) = handle.join() {
                if !thread::panicking() {
                    std::panic::resume_unwind(error);
                }
            }
        }
    }
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "condition was not met in time");
        thread::sleep(Duration::from_millis(5));
    }
}

fn next_event(events: &Receiver<NetEvent>) -> NetEvent {
    events
        .recv_timeout(WAIT)
        .expect("expected an event from the net")
}

/// Read events until a frame arrives. Returns the events before it and the frame.
fn until_frame(events: &Receiver<NetEvent>) -> (Vec<NetEvent>, VideoFrame) {
    let mut before = Vec::new();
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("frame deadline elapsed");
        match events
            .recv_timeout(remaining)
            .expect("expected a frame from the net")
        {
            NetEvent::Frame(frame) => return (before, frame),
            other => before.push(other),
        }
    }
}

/// Everything the relay logs until it is quiet for `quiet`.
fn drain_log(log: &Receiver<Logged>, quiet: Duration) -> Vec<Logged> {
    let mut out = Vec::new();
    // Heartbeat tests can deliberately produce messages more often than
    // `quiet`; cap collection independently of activity so cleanup is reached.
    let deadline = Instant::now() + Duration::from_millis(500);
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match log.recv_timeout(quiet.min(remaining)) {
            Ok(item) => out.push(item),
            Err(_) => break,
        }
    }
    out
}

/// A positive wire assertion must wait for its message, even when the scripted
/// relay is paused. Quiet collection alone can finish before it starts reading.
fn collect_until(log: &Receiver<Logged>, matches: impl Fn(&Logged) -> bool) -> Vec<Logged> {
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("relay message deadline elapsed");
        let item = log.recv_timeout(remaining).expect("expected relay message");
        let found = matches(&item);
        seen.push(item);
        if found {
            return seen;
        }
    }
}

/// Parsed JSON of every text message on `path`, in arrival order.
fn texts(seen: &[Logged], path: &str) -> Vec<Value> {
    seen.iter()
        .filter_map(|item| match item {
            Logged::Text {
                path: sent_on,
                text,
            } if sent_on == path => serde_json::from_str(text).ok(),
            _ => None,
        })
        .collect()
}

fn ack_seqs(seen: &[Logged]) -> Vec<u64> {
    texts(seen, "/media-ws")
        .iter()
        .filter(|message| message["type"] == "ack")
        .map(|message| message["seq"].as_u64().expect("ack seq"))
        .collect()
}

fn of_type<'a>(messages: &'a [Value], kind: &str) -> Vec<&'a Value> {
    messages
        .iter()
        .filter(|message| message["type"] == kind)
        .collect()
}

/// The query, origin and cookie of the first upgrade on `path`.
fn first_connect(seen: &[Logged], path: &str) -> (String, Option<String>, Option<String>) {
    seen.iter()
        .find_map(|item| match item {
            Logged::Connect {
                path: on,
                query,
                origin,
                cookie,
            } if on == path => Some((query.clone(), origin.clone(), cookie.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no upgrade on {path}"))
}

fn connect_count(seen: &[Logged], path: &str) -> usize {
    seen.iter()
        .filter(|item| matches!(item, Logged::Connect { path: on, .. } if on == path))
        .count()
}

fn keyframe_reasons(seen: &[Logged]) -> Vec<String> {
    texts(seen, "/media-ws")
        .iter()
        .filter(|message| message["type"] == "keyframe")
        .map(|message| message["reason"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn handshake_start_and_first_frame_then_stream() {
    let h = start(vec![scenario(EPOCH_A, vec![key(1)])], timing(), 8);
    let (before, frame) = until_frame(&h.events);
    assert_eq!(before, vec![reset(1, "connect")]);
    assert_eq!((frame.local_epoch, frame.seq, frame.generation), (1, 1, 7));
    assert!(frame.key && !frame.refine);
    assert_eq!(frame.codec, Codec::H264);
    assert_eq!(frame.data, b"payload-1".to_vec());
    assert_eq!(
        (frame.width, frame.height, frame.capture_ts_us),
        (640, 360, 1_000)
    );
    wait_for(|| h.stats.state() == NetState::Streaming);

    let origin = h.relay.origin.clone();
    let seen = drain_log(&h.relay.log, QUIET);

    let (query, signal_origin, signal_cookie) = first_connect(&seen, "/ws");
    assert_eq!(query, "");
    assert_eq!(signal_origin.as_deref(), Some(origin.as_str()));
    let cookie = signal_cookie.expect("/ws cookie");
    assert!(
        cookie.contains("__Host-gate=gate-1") && cookie.contains("__Host-bp-session=sess-1"),
        "{cookie}"
    );
    let (query, media_origin, media_cookie) = first_connect(&seen, "/media-ws");
    assert_eq!(query, format!("epoch={EPOCH_A}"));
    assert_eq!(media_origin.as_deref(), Some(origin.as_str()));
    assert_eq!(
        media_cookie,
        Some(cookie),
        "both sockets send the same cookies"
    );

    let signal = texts(&seen, "/ws");
    assert_eq!(signal.len(), 3, "{signal:?}");
    assert_eq!(signal[0], json!({ "type": "hello", "role": "client" }));
    assert_eq!(
        signal[1],
        json!({
            "type": "media-control",
            "action": "start",
            "protocol": 4,
            "epoch": EPOCH_A,
            "codec": "h264",
            "streamMode": "work",
            "reason": "client-start",
            "quality": "high",
        }),
        "start copies the browser values and carries no audio key"
    );
    assert_eq!(signal[2]["type"], "first-frame");
    assert_eq!(signal[2]["phase"], "request");
    assert_eq!(signal[2]["protocol"], 1);
    assert_eq!(signal[2]["transport"], "wss");
    assert_eq!(signal[2]["epoch"], EPOCH_A);
    let request_id = signal[2]["requestId"].as_str().expect("requestId");
    assert!((8..=64).contains(&request_id.len()));

    let media = texts(&seen, "/media-ws");
    assert_eq!(media[0], json!({ "type": "hello", "role": "media-client" }));
    assert_eq!(media.len(), 2, "the hello, then one ACK: {media:?}");
    assert_eq!(media[1]["type"], "ack");
    assert_eq!(
        (media[1]["generation"].as_u64(), media[1]["seq"].as_u64()),
        (Some(7), Some(1))
    );
    assert!(media[1]["recvTsMs"]
        .as_f64()
        .is_some_and(|value| value >= 0.0));
    assert!(media[1].get("decodedSeq").is_none(), "nothing decoded yet");

    let snapshot = h.stats.snapshot();
    assert_eq!(
        (
            snapshot.attempts,
            snapshot.established,
            snapshot.local_epoch
        ),
        (1, 1, 1)
    );
    assert_eq!(
        (
            snapshot.frames_received,
            snapshot.frames_accepted,
            snapshot.acks_sent
        ),
        (1, 1, 1)
    );
    h.finish().expect("clean stop");
}

#[test]
fn every_valid_frame_is_acked_even_when_the_gate_drops_it() {
    let steps = vec![
        key(1),
        delta(2),
        delta(4),
        delta(5),
        Step::Pause(Duration::from_millis(100)),
        key(9),
    ];
    let h = start(vec![scenario(EPOCH_A, steps)], timing(), 8);

    let (before, first) = until_frame(&h.events);
    assert_eq!(before, vec![reset(1, "connect")]);
    assert_eq!(first.seq, 1);
    assert_eq!(until_frame(&h.events).1.seq, 2);
    let (between, key9) = until_frame(&h.events);
    assert_eq!(
        between,
        vec![reset(1, "sequence-gap")],
        "the gap resets once, before the next keyframe"
    );
    assert_eq!((key9.seq, key9.key), (9, true));

    let seen = drain_log(&h.relay.log, QUIET);
    assert_eq!(
        ack_seqs(&seen),
        vec![1, 2, 4, 5, 9],
        "dropped deltas are still ACKed"
    );
    assert_eq!(keyframe_reasons(&seen), vec!["sequence-gap".to_owned()]);
    let snapshot = h.stats.snapshot();
    assert_eq!(
        (
            snapshot.frames_received,
            snapshot.gate_drops,
            snapshot.frames_accepted
        ),
        (5, 2, 3)
    );
    assert_eq!(snapshot.acks_sent, 5);
    h.finish().expect("clean stop");
}

#[test]
fn bpa1_audio_is_ignored_without_an_ack() {
    let mut audio = b"BPA1".to_vec();
    audio.extend_from_slice(&[0u8; 32]);
    let steps = vec![Step::Binary(audio), key(1)];
    let h = start(vec![scenario(EPOCH_A, steps)], timing(), 8);
    let (_, frame) = until_frame(&h.events);
    assert_eq!(frame.seq, 1);
    let snapshot = h.stats.snapshot();
    assert_eq!(snapshot.bpa1_ignored, 1);
    assert_eq!(snapshot.frames_received, 1, "BPA1 is not a video frame");
    let seen = drain_log(&h.relay.log, QUIET);
    assert_eq!(ack_seqs(&seen), vec![1]);
    h.finish().expect("clean stop");
}

#[test]
fn malformed_binary_packets_are_refused_and_never_acked() {
    let valid = frame(true, 7, 1, b"ok");
    let mut bad_magic = valid.clone();
    bad_magic[0] = b'X';
    let mut bad_version = valid.clone();
    bad_version[4] = 3;
    let mut unknown_flags = valid.clone();
    unknown_flags[5] = 0x80;
    let mut bad_codec = valid.clone();
    bad_codec[6] = 3;
    let mut reserved = valid.clone();
    reserved[7] = 1;
    let header_only = valid[..V4_HEADER_BYTES].to_vec();
    let mut oversized = valid[..V4_HEADER_BYTES].to_vec();
    oversized.extend(vec![0u8; 8 * 1024 * 1024 + 1]);
    let steps = vec![
        Step::Binary(bad_magic),
        Step::Binary(bad_version),
        Step::Binary(unknown_flags),
        Step::Binary(bad_codec),
        Step::Binary(reserved),
        Step::Binary(header_only),
        Step::Binary(oversized),
        Step::Binary(valid),
    ];
    let h = start(vec![scenario(EPOCH_A, steps)], timing(), 8);
    let (_, frame) = until_frame(&h.events);
    assert_eq!(frame.data, b"ok".to_vec());
    let snapshot = h.stats.snapshot();
    assert_eq!(snapshot.rejected_packets, 7);
    assert_eq!(snapshot.frames_received, 1);
    let seen = drain_log(&h.relay.log, QUIET);
    assert_eq!(ack_seqs(&seen), vec![1], "refused packets get no ACK");
    h.finish().expect("clean stop");
}

#[test]
fn strict_controls_on_the_media_socket_are_counted_and_dropped() {
    let steps = vec![
        Step::Text(r#"{"type":"pong"}"#.to_owned()),
        Step::Text(r#"{"type":"ping"}"#.to_owned()),
        Step::Text(r#"{"type":"mystery"}"#.to_owned()),
        Step::Text("not json".to_owned()),
        Step::Text(r#"{"type":"media-peer","present":"yes"}"#.to_owned()),
        Step::Text(r#"{"type":"error","message":"x"}"#.to_owned()),
        key(1),
    ];
    let h = start(vec![scenario(EPOCH_A, steps)], timing(), 8);
    let (_, frame) = until_frame(&h.events);
    assert_eq!(frame.seq, 1, "a bad control does not stop the stream");
    let snapshot = h.stats.snapshot();
    assert_eq!(
        snapshot.rejected_controls, 4,
        "mystery, not json, bad media-peer, relay error"
    );
    assert_eq!(snapshot.frames_accepted, 1);
    h.finish().expect("clean stop");
}

#[test]
fn host_joining_resets_the_gate_with_host_joined() {
    let steps = vec![
        Step::Text(r#"{"type":"media-peer","present":true}"#.to_owned()),
        key(1),
    ];
    let h = start(vec![scenario(EPOCH_A, steps)], timing(), 8);
    let (before, frame) = until_frame(&h.events);
    assert_eq!(frame.seq, 1);
    assert_eq!(before.last(), Some(&reset(1, "host-joined")), "{before:?}");
    assert!(!before
        .iter()
        .any(|event| matches!(event, NetEvent::Frame(_))));
    let seen = drain_log(&h.relay.log, QUIET);
    assert!(
        keyframe_reasons(&seen).is_empty(),
        "the browser waits for the host's keyframe"
    );
    h.finish().expect("clean stop");
}

#[test]
fn decoded_feedback_sets_decoded_seq_and_presents_once_and_stale_feedback_is_ignored() {
    let steps = vec![key(1), Step::Pause(Duration::from_millis(300)), delta(2)];
    let h = start(vec![scenario(EPOCH_A, steps)], timing(), 8);
    let (_, first) = until_frame(&h.events);
    assert_eq!(first.seq, 1);
    h.feedback
        .send(DecoderFeedback::Decoded {
            local_epoch: 1,
            generation: 7,
            seq: 1,
        })
        .unwrap();
    h.feedback
        .send(DecoderFeedback::Decoded {
            local_epoch: 0,
            generation: 7,
            seq: 99,
        })
        .unwrap();
    wait_for(|| h.stats.snapshot().stale_feedback == 1);
    // A stale generation in the current epoch must not overwrite public decode
    // counters or the decodedSeq placed on the next ACK.
    h.feedback
        .send(DecoderFeedback::Decoded {
            local_epoch: 1,
            generation: 6,
            seq: 98,
        })
        .unwrap();
    wait_for(|| h.stats.snapshot().stale_feedback == 2);
    let (_, second) = until_frame(&h.events);
    assert_eq!(second.seq, 2);

    let mut seen = drain_log(&h.relay.log, QUIET);
    assert!(texts(&seen, "/ws")
        .iter()
        .all(|message| message["phase"] != "presented"));
    for _ in 0..2 {
        h.feedback
            .send(DecoderFeedback::Presented {
                local_epoch: 1,
                generation: 7,
                seq: 1,
            })
            .unwrap();
    }
    seen.extend(drain_log(&h.relay.log, QUIET));
    let media = texts(&seen, "/media-ws");
    let ack_two = media
        .iter()
        .find(|message| message["type"] == "ack" && message["seq"] == 2)
        .expect("ack for seq 2");
    assert_eq!(
        ack_two["decodedSeq"], 1,
        "decodedSeq comes from current feedback only"
    );

    let signal = texts(&seen, "/ws");
    let request_id = of_type(&signal, "first-frame")
        .iter()
        .find(|message| message["phase"] == "request")
        .expect("first-frame request")["requestId"]
        .clone();
    let presented = of_type(&signal, "first-frame")
        .into_iter()
        .filter(|message| message["phase"] == "presented")
        .collect::<Vec<_>>();
    assert_eq!(presented.len(), 1);
    assert_eq!(presented[0]["requestId"], request_id);
    assert_eq!(presented[0]["generation"], 7);

    let snapshot = h.stats.snapshot();
    assert_eq!((snapshot.decoded, snapshot.last_decoded_seq), (1, Some(1)));
    h.finish().expect("clean stop");
}

#[test]
fn decode_error_resets_the_decoder_and_requests_a_sanitized_keyframe() {
    let steps = vec![key(1), Step::Pause(Duration::from_millis(300))];
    let h = start(vec![scenario(EPOCH_A, steps)], timing(), 8);
    let _ = until_frame(&h.events);
    h.feedback
        .send(DecoderFeedback::DecodeError {
            local_epoch: 1,
            reason: "bad codec!".to_owned(),
        })
        .unwrap();
    assert_eq!(next_event(&h.events), reset(1, "bad-codec-"));
    wait_for(|| h.stats.snapshot().keyframe_requests >= 1);
    let seen = collect_until(&h.relay.log, |item| match item {
        Logged::Text { path, text } if path == "/media-ws" => serde_json::from_str::<Value>(text)
            .is_ok_and(|message| {
                message["type"] == "keyframe" && message["reason"] == "bad-codec-"
            }),
        _ => false,
    });
    assert_eq!(keyframe_reasons(&seen), vec!["bad-codec-".to_owned()]);
    h.finish().expect("clean stop");
}

#[test]
fn keyframe_requests_are_throttled_and_repeat_while_the_gate_waits() {
    let steps = vec![
        key(1),
        delta(3),
        Step::Pause(Duration::from_millis(100)),
        delta(5),
        Step::Pause(Duration::from_millis(100)),
        delta(7),
        Step::Pause(Duration::from_millis(400)),
    ];
    let h = start(vec![scenario(EPOCH_A, steps)], timing(), 16);
    let _ = until_frame(&h.events);
    wait_for(|| h.stats.snapshot().keyframe_requests >= 1);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        h.stats.snapshot().keyframe_requests,
        1,
        "a second request inside 250 ms is throttled"
    );
    // The repeat timer can fire before the last scripted delta is received.
    // Await both observed outcomes instead of assuming a keyframe proves all
    // three gate drops have already happened.
    wait_for(|| {
        let snapshot = h.stats.snapshot();
        snapshot.keyframe_requests >= 2 && snapshot.gate_drops >= 3
    });
    assert!(h.stats.snapshot().gate_drops >= 3);
    h.finish().expect("clean stop");
}

#[test]
fn dead_media_socket_reconnects_with_a_new_ws_epoch() {
    let first = Scenario {
        media_silent: true,
        ..Scenario::new(EPOCH_A)
    };
    let second = scenario(EPOCH_B, vec![key(1)]);
    let timing = Timing {
        heartbeat: Duration::from_millis(50),
        dead: Duration::from_millis(300),
        ..timing()
    };
    let h = start(vec![first, second], timing, 8);

    let (before, frame) = until_frame(&h.events);
    assert!(before.contains(&reset(1, "connect")), "{before:?}");
    assert_eq!(before.last(), Some(&reset(2, "reconnect")), "{before:?}");
    assert_eq!((frame.local_epoch, frame.seq), (2, 1));

    let seen = drain_log(&h.relay.log, QUIET);
    assert_eq!(connect_count(&seen, "/ws"), 2);
    let media_queries: Vec<String> = seen
        .iter()
        .filter_map(|item| match item {
            Logged::Connect { path, query, .. } if path == "/media-ws" => Some(query.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        media_queries,
        vec![format!("epoch={EPOCH_A}"), format!("epoch={EPOCH_B}")]
    );
    let snapshot = h.stats.snapshot();
    assert_eq!(
        (
            snapshot.attempts,
            snapshot.established,
            snapshot.local_epoch
        ),
        (2, 2, 2)
    );
    assert!(snapshot.heartbeats_sent >= 1);
    assert!(
        h.stats.last_error().unwrap_or_default().contains("media"),
        "{:?}",
        h.stats.last_error()
    );

    // Feedback from the old connection must not mark the new one as presented.
    h.feedback
        .send(DecoderFeedback::Decoded {
            local_epoch: 1,
            generation: 7,
            seq: 1,
        })
        .unwrap();
    wait_for(|| h.stats.snapshot().stale_feedback == 1);
    let later = drain_log(&h.relay.log, QUIET);
    assert!(
        !texts(&later, "/ws")
            .iter()
            .any(|message| message["phase"] == "presented"),
        "stale feedback sent a presented"
    );
    h.finish().expect("clean stop");
}

#[test]
fn first_frame_watchdog_resends_then_reconnects() {
    let timing = Timing {
        first_frame: Duration::from_millis(100),
        ..timing()
    };
    let h = start(vec![Scenario::new(EPOCH_A)], timing, 8);
    // Two attempts each send a request and a retry, so the third attempt proves the second timed out.
    wait_for(|| h.stats.snapshot().attempts >= 3);
    let seen = drain_log(&h.relay.log, QUIET);
    let signal = texts(&seen, "/ws");
    let requests = of_type(&signal, "first-frame")
        .into_iter()
        .filter(|message| message["phase"] == "request")
        .count();
    assert!(
        requests >= 4,
        "the request goes out again on each attempt's retry: {requests}"
    );
    let timeouts = of_type(&signal, "media-control")
        .into_iter()
        .filter(|message| {
            message["action"] == "keyframe" && message["reason"] == "first-frame-timeout"
        })
        .count();
    assert!(timeouts >= 2, "{timeouts}");
    assert!(
        h.stats
            .last_error()
            .unwrap_or_default()
            .contains("first frame"),
        "{:?}",
        h.stats.last_error()
    );
    h.finish().expect("clean stop");
}

#[test]
fn a_full_queue_drops_the_frame_and_the_reset_comes_before_the_next_keyframe() {
    let steps = vec![
        key(0),
        delta(1),
        delta(2),
        Step::Pause(Duration::from_millis(400)),
        key(10),
    ];
    let h = start(vec![scenario(EPOCH_A, steps)], timing(), 2);
    // Let the queue fill before the decoder reads anything.
    wait_for(|| h.stats.snapshot().queue_overflows >= 1);

    let mut kinds = Vec::new();
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("replacement keyframe deadline elapsed");
        match h
            .events
            .recv_timeout(remaining)
            .expect("replacement keyframe event")
        {
            NetEvent::Reset(reset) => kinds.push(format!("reset:{}", reset.reason)),
            NetEvent::Frame(frame) => {
                kinds.push(format!("frame:{}", frame.seq));
                if frame.seq == 10 {
                    break;
                }
            }
        }
    }
    assert_eq!(
        kinds,
        [
            "reset:connect",
            "frame:0",
            "reset:queue-overflow",
            "frame:10"
        ]
    );
    let snapshot = h.stats.snapshot();
    assert_eq!(snapshot.queue_overflows, 1);
    let seen = drain_log(&h.relay.log, QUIET);
    assert!(keyframe_reasons(&seen).contains(&"queue-overflow".to_owned()));
    h.finish().expect("clean stop");
}

#[test]
fn panic_cleanup_stops_client_and_silent_relay_connections() {
    let mut address = String::new();
    let mut stats = None;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let h = start(
            vec![Scenario {
                media_silent: true,
                ..Scenario::new(EPOCH_A)
            }],
            timing(),
            8,
        );
        wait_for(|| h.stats.snapshot().established == 1);
        address = h.relay.origin.trim_start_matches("http://").to_owned();
        stats = Some(h.stats.clone());
        panic!("synthetic failure to verify scoped net test cleanup");
    }));
    assert!(outcome.is_err());
    assert_eq!(stats.unwrap().state(), NetState::Stopped);
    assert!(
        std::net::TcpStream::connect(&address).is_err(),
        "fake relay listener survived cleanup"
    );
}

#[test]
fn stop_flag_ends_a_running_session_cleanly() {
    let h = start(vec![scenario(EPOCH_A, vec![])], timing(), 8);
    wait_for(|| h.stats.state() == NetState::Starting || h.stats.state() == NetState::Streaming);
    h.finish().expect("stop is a clean end");
}

#[test]
fn dropping_the_event_receiver_ends_the_run_cleanly() {
    let mut h = start(vec![scenario(EPOCH_A, vec![key(1)])], timing(), 8);
    h.drop_events();
    assert_eq!(h.join(), Ok(()));
    assert_eq!(h.stats.state(), NetState::Stopped);
}

#[test]
fn an_auth_refusal_is_fatal_and_is_not_retried() {
    let refused = Scenario {
        refuse: Some(401),
        ..Scenario::new(EPOCH_A)
    };
    let mut h = start(vec![refused], timing(), 8);
    assert_eq!(h.join(), Err(NetError::Auth(401)));
    assert_eq!(h.stats.snapshot().attempts, 1);
    assert_eq!(h.stats.state(), NetState::Stopped);
}

#[test]
fn a_relay_error_on_signaling_is_fatal() {
    let failing = Scenario {
        signal_error: Some("room full".to_owned()),
        ..Scenario::new(EPOCH_A)
    };
    let mut h = start(vec![failing], timing(), 8);
    assert_eq!(h.join(), Err(NetError::RelayError("room full".to_owned())));
}

#[test]
fn a_transient_handshake_refusal_reconnects() {
    let refused = Scenario {
        refuse: Some(503),
        ..Scenario::new(EPOCH_A)
    };
    let h = start(vec![refused, scenario(EPOCH_A, vec![key(1)])], timing(), 8);
    let (_, frame) = until_frame(&h.events);
    assert_eq!(frame.seq, 1);
    let snapshot = h.stats.snapshot();
    assert_eq!((snapshot.attempts, snapshot.established), (2, 1));
    assert!(h.stats.last_error().unwrap_or_default().contains("503"));
    h.finish().expect("clean stop");
}

#[test]
fn plaintext_and_malformed_relays_are_refused_before_any_socket() {
    let cases = [
        ("http://relay.example", "plain"),
        ("http://10.0.0.5:8787", "plain"),
        ("ws://10.0.0.5:8787", "plain"),
        ("http://localhost.example:8787", "plain"),
        ("https://relay.example/path", "malformed"),
    ];
    for (relay, kind) in cases {
        let (event_tx, _events) = mpsc::sync_channel(1);
        let (_feedback, feedback_rx) = mpsc::channel();
        let stats = Arc::new(TransportStats::new());
        let config = NetConfig::new(relay, Codec::H264, CookieJar::new());
        let result = run_with_timing(
            config,
            event_tx,
            feedback_rx,
            Arc::new(AtomicBool::new(false)),
            stats.clone(),
            timing(),
        );
        let matched = match kind {
            "plain" => matches!(result, Err(NetError::PlaintextRefused(_))),
            _ => matches!(result, Err(NetError::InvalidRelayUrl(_))),
        };
        assert!(matched, "{relay}: {result:?}");
        assert_eq!(stats.snapshot().attempts, 0, "{relay} must not connect");
    }
}

/// An exact stop-controlled relay: the tail is sent only after the stop
/// command reaches /ws, so it cannot accidentally arrive before shutdown.
#[test]
fn shutdown_stops_then_acks_inflight_video_without_decoding_and_closes_both_lanes() {
    shutdown_relay_case(false);
}

#[test]
fn shutdown_has_a_hard_bound_when_the_host_keeps_sending_video() {
    shutdown_relay_case(true);
}

fn shutdown_relay_case(keep_sending: bool) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    struct StopOnDrop(Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Relaxed);
        }
    }

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let origin = format!("http://{address}");
    listener.set_nonblocking(true).unwrap();
    let config = NetConfig::new(origin, Codec::H264, cookies());
    let (event_tx, events) = mpsc::sync_channel(8);
    let (feedback, feedback_rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let _cleanup = StopOnDrop(stop.clone());
    let stats = Arc::new(TransportStats::new());
    let client = {
        let stop = stop.clone();
        let stats = stats.clone();
        thread::spawn(move || run_with_timing(config, event_tx, feedback_rx, stop, stats, timing()))
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(WAIT, async {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let (signal, _) = listener.accept().await.unwrap();
            let mut signal = tokio_tungstenite::accept_async(signal).await.unwrap();
            assert!(matches!(
                signal.next().await.unwrap().unwrap(),
                Message::Text(_)
            ));
            signal
                .send(Message::Text(
                    json!({"type":"hello-ok", "mediaEpoch": EPOCH_A})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let (media, _) = listener.accept().await.unwrap();
            let mut media = tokio_tungstenite::accept_async(media).await.unwrap();
            assert!(matches!(
                media.next().await.unwrap().unwrap(),
                Message::Text(_)
            ));
            media
                .send(Message::Text(json!({"type":"hello-ok"}).to_string().into()))
                .await
                .unwrap();
            media
                .send(Message::Text(
                    json!({"type":"media-peer", "present":true})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            // Registration followed by the usual start and first-frame request.
            for expected in ["media-control", "first-frame"] {
                let Message::Text(text) = signal.next().await.unwrap().unwrap() else {
                    panic!("expected start control")
                };
                let value: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(value["type"], expected);
            }
            media
                .send(Message::Binary(frame(true, 7, 1, b"first").into()))
                .await
                .unwrap();
            let Message::Text(ack) = media.next().await.unwrap().unwrap() else {
                panic!("expected initial ACK")
            };
            assert_eq!(serde_json::from_str::<Value>(&ack).unwrap()["seq"], 1);
            // ACK precedes event delivery; wait until the baseline frame exists.
            while stats.snapshot().frames_accepted < 1 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            stop.store(true, Relaxed);
            let Message::Text(text) = signal.next().await.unwrap().unwrap() else {
                panic!("expected stop")
            };
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap(),
                json!({
                    "type":"media-control", "action":"stop", "protocol":4, "epoch":EPOCH_A,
                })
            );
            // Pending feedback must never create a keyframe/presented control.
            feedback
                .send(DecoderFeedback::KeyframeNeeded {
                    local_epoch: 1,
                    reason: "shutdown-test".into(),
                })
                .unwrap();
            let started = Instant::now();
            media
                .send(Message::Binary(b"BPA1ignored".to_vec().into()))
                .await
                .unwrap();
            media
                .send(Message::Binary(b"malformed".to_vec().into()))
                .await
                .unwrap();
            let mut sent = 1;
            let mut acked = 1;
            let mut next_send = tokio::time::Instant::now() + Duration::from_millis(30);
            loop {
                tokio::select! {
                    biased;
                    item = media.next() => match item {
                        Some(Ok(Message::Text(text))) => {
                            let ack: Value = serde_json::from_str(&text).unwrap();
                            assert_eq!(ack["type"], "ack");
                            assert_eq!(ack["seq"], acked + 1);
                            acked += 1;
                        }
                        Some(Ok(Message::Close(_))) => {
                            let _ = media.flush().await;
                            break;
                        }
                        other => panic!("expected ACK or graceful close, got {other:?}"),
                    },
                    _ = tokio::time::sleep_until(next_send), if keep_sending || sent < 4 => {
                        sent += 1;
                        media.send(Message::Binary(frame(false, 7, sent, b"inflight").into())).await.unwrap();
                        next_send = tokio::time::Instant::now() + Duration::from_millis(30);
                    }
                }
            }
            if !keep_sending {
                assert_eq!(acked, sent, "all in-flight video ACKed");
            } else {
                assert!(acked >= 20, "continuous video must keep the quiet timer active");
                assert!(started.elapsed() >= Duration::from_millis(900), "continuous traffic must reach the hard cap");
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "shutdown hard cap exceeded"
            );
            assert!(
                matches!(signal.next().await.unwrap().unwrap(), Message::Close(_)),
                "no decoder feedback/control after stop"
            );
            let _ = signal.flush().await;
        })
        .await
        .expect("shutdown relay timed out");
    });
    wait_for(|| client.is_finished());
    assert_eq!(client.join().unwrap(), Ok(()));
    assert_eq!(stats.state(), NetState::Stopped);
    assert_eq!(stats.snapshot().attempts, 1, "shutdown cannot reconnect");
    let delivered: Vec<_> = events
        .try_iter()
        .filter_map(|event| match event {
            NetEvent::Frame(frame) => Some(frame.seq),
            _ => None,
        })
        .collect();
    assert_eq!(delivered, vec![1], "in-flight tail is ACK-only");
    assert_eq!(stats.snapshot().bpa1_ignored, 1);
    assert_eq!(stats.snapshot().rejected_packets, 1);
    assert!(
        std::net::TcpStream::connect(address).is_err(),
        "relay listener must be dropped"
    );
}
