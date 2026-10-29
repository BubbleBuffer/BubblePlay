//! The `--webrtc` answerer against a str0m offerer over loopback UDP. The
//! offerer plays the browser: it sends `offer` on `/ws`, takes the fake's
//! `answer` and `ice`, and then runs ICE, DTLS and SRTP for real. Nothing is
//! mocked on the media path; each test reads the fake's own RTC report back
//! through the snapshot.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{send_json, signal_hello, Ws};
use fake_wss_host::model::Source;
use fake_wss_host::monitors::Knobs as Monitors;
use fake_wss_host::rtc::Knobs;
use fake_wss_host::stream::Codec;
use futures_util::StreamExt;
use serde_json::{json, Value};
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::media::{Direction, MediaKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::time::{sleep_until, Instant as TokioInstant};
use tokio_tungstenite::tungstenite::Message;

/// One cursor-sync (type 8, seq 42, x 10, y 20) on the motion lane.
const SYNC: [u8; 7] = [8, 42, 0, 10, 0, 20, 0];
/// Its cursor-state answer: ack 42, the same position, ACK|IN_BOUNDS|VISIBLE.
const ECHO: [u8; 8] = [128, 42, 0, 10, 0, 20, 0, 7];
/// A second sync and its answer, sent after WSS took over: the lane must still answer.
const SYNC_AFTER: [u8; 7] = [8, 43, 0, 11, 0, 21, 0];
const ECHO_AFTER: [u8; 8] = [128, 43, 0, 11, 0, 21, 0, 7];

/// A recording stand-in: each access unit is one Annex-B NAL, an IDR every
/// `gop` units. The fake only moves these bytes; it never parses them.
struct AnnexB {
    len: usize,
    gop: usize,
}

impl Source for AnnexB {
    fn codec(&self) -> Codec {
        Codec::H264
    }
    fn width(&self) -> u32 {
        1280
    }
    fn height(&self) -> u32 {
        720
    }
    fn len(&self) -> usize {
        self.len
    }
    fn is_key(&self, index: usize) -> bool {
        index.is_multiple_of(self.gop)
    }
    fn has_param_sets(&self, _index: usize) -> bool {
        true
    }
    fn next_key_at_or_after(&self, index: usize) -> usize {
        let next = index.div_ceil(self.gop) * self.gop;
        if next >= self.len {
            0
        } else {
            next
        }
    }
    fn payload(&self, index: usize) -> Vec<u8> {
        let header = if self.is_key(index) { 0x65 } else { 0x41 };
        let mut out = vec![0, 0, 0, 1, header];
        out.extend(std::iter::repeat_n(0x5a, 1200));
        out
    }
}

fn sources() -> Vec<Arc<dyn Source>> {
    vec![Arc::new(AnnexB { len: 120, gop: 30 }) as Arc<dyn Source>]
}

#[derive(Default)]
struct Seen {
    answer: Option<String>,
    connected: bool,
    video_frames: u64,
    audio_frames: u64,
    opened: Vec<String>,
    echoes: Vec<Vec<u8>>,
    ready: bool,
    /// The fake's `first-frame` `admitted` message, as sent.
    admitted: Option<Value>,
    /// When the DTLS close_notify reached this offerer as `Event::Closed`.
    closed_at: Option<Instant>,
    /// Every `desktop-state`, `monitor-list` and `monitor-error` message, in order.
    signals: Vec<Value>,
}

/// The offerer. It owns the sans-IO `Rtc` and drives it from one loop that
/// waits on the UDP socket, its timers and `/ws`.
struct Browser {
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    pending: Option<SdpPendingOffer>,
    video: Mid,
    audio: Mid,
    motion: Option<ChannelId>,
    synced: bool,
    seen: Seen,
}

impl Browser {
    /// Sends the offer: recvonly video and audio, and the two input lanes.
    async fn offer(ws: &mut Ws) -> Self {
        let socket = UdpSocket::bind(("127.0.0.1", 0))
            .await
            .expect("bind client UDP");
        let local = socket.local_addr().expect("client address");
        let mut rtc = Rtc::builder().build(Instant::now());
        rtc.add_local_candidate(Candidate::host(local, "udp").expect("host candidate"));
        let mut change = rtc.sdp_api();
        let video = change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let audio = change.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        // The browser's own configuration (receiver.js): the control lane is
        // ordered and reliable, the motion lane unordered and never retransmitted.
        change.add_channel("input-control".into());
        change.add_channel_with_config(ChannelConfig {
            label: "input-motion".into(),
            ordered: false,
            reliability: Reliability::MaxRetransmits { retransmits: 0 },
            ..ChannelConfig::default()
        });
        let (offer, pending) = change.apply().expect("offer has changes");
        send_json(ws, json!({"type": "offer", "sdp": offer.to_sdp_string()})).await;
        Self {
            rtc,
            socket,
            local,
            pending: Some(pending),
            video,
            audio,
            motion: None,
            synced: false,
            seen: Seen::default(),
        }
    }

    /// Runs until `goal` holds, the deadline passes, or `/ws` closes.
    async fn drive(&mut self, ws: &mut Ws, deadline: Instant, goal: impl Fn(&Seen) -> bool) {
        let mut buf = vec![0u8; 65_536];
        loop {
            if goal(&self.seen) || Instant::now() >= deadline {
                break;
            }
            let next = self.flush().await;
            self.sync_cursor();
            let wake = next
                .min(Instant::now() + Duration::from_millis(20))
                .min(deadline);
            tokio::select! {
                received = self.socket.recv_from(&mut buf) => {
                    if let Ok((len, source)) = received {
                        if let Ok(receive) = Receive::new(Protocol::Udp, source, self.local, &buf[..len]) {
                            let _ = self.rtc.handle_input(Input::Receive(Instant::now(), receive));
                        }
                    }
                }
                _ = sleep_until(TokioInstant::from_std(wake)) => {
                    let _ = self.rtc.handle_input(Input::Timeout(Instant::now()));
                }
                message = ws.next() => match message {
                    Some(Ok(Message::Text(text))) => self.on_signal(&text),
                    // Binary frames on `/ws` (media, cursor) are not signalling; skip them.
                    Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                },
            }
        }
    }

    /// Drains the sans-IO output: datagrams go to the socket, events to `on_event`.
    async fn flush(&mut self) -> Instant {
        loop {
            match self.rtc.poll_output().expect("client output") {
                Output::Timeout(at) => return at,
                Output::Transmit(transmit) => {
                    let _ = self
                        .socket
                        .send_to(&transmit.contents, transmit.destination)
                        .await;
                }
                Output::Event(event) => self.on_event(event),
            }
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Connected
            | Event::IceConnectionStateChange(IceConnectionState::Connected)
            | Event::IceConnectionStateChange(IceConnectionState::Completed) => {
                self.seen.connected = true;
            }
            Event::MediaData(data) => {
                if data.mid == self.video {
                    self.seen.video_frames += 1;
                } else if data.mid == self.audio {
                    self.seen.audio_frames += 1;
                }
            }
            Event::ChannelOpen(id, label) => {
                if label == "input-motion" {
                    self.motion = Some(id);
                }
                self.seen.opened.push(label);
            }
            Event::ChannelData(data) if data.binary => self.seen.echoes.push(data.data),
            Event::Closed => {
                self.seen.closed_at.get_or_insert_with(Instant::now);
            }
            _ => {}
        }
    }

    /// Writes one binary packet on the motion lane; false while the lane is
    /// not open or the write is refused.
    fn write_motion(&mut self, bytes: &[u8]) -> bool {
        let Some(id) = self.motion else { return false };
        match self.rtc.channel(id) {
            Some(mut channel) => matches!(channel.write(true, bytes), Ok(true)),
            None => false,
        }
    }

    /// Sends the cursor-sync once the motion lane is open, and retries until
    /// the write is accepted.
    fn sync_cursor(&mut self) {
        if !self.synced {
            self.synced = self.write_motion(&SYNC);
        }
    }

    /// Takes the fake's `answer` and `ice`; it ignores everything else here.
    fn on_signal(&mut self, text: &str) {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return;
        };
        match value["type"].as_str() {
            Some("answer") => {
                let sdp = value["sdp"].as_str().unwrap_or_default().to_owned();
                if let Some(pending) = self.pending.take() {
                    let answer = SdpAnswer::from_sdp_string(&sdp).expect("answer parses");
                    self.rtc
                        .sdp_api()
                        .accept_answer(pending, answer)
                        .expect("accept answer");
                }
                self.seen.answer = Some(sdp);
            }
            Some("ice") => {
                if let Some(line) = value["candidate"].as_str() {
                    if let Ok(candidate) = Candidate::from_sdp_string(line) {
                        self.rtc.add_remote_candidate(candidate);
                    }
                }
            }
            Some("desktop-state" | "monitor-list" | "monitor-error") => {
                self.seen.signals.push(value.clone());
            }
            Some("first-frame") if value["phase"] == "ready" => self.seen.ready = true,
            Some("first-frame") if value["phase"] == "admitted" => {
                self.seen.admitted = Some(value.clone());
            }
            _ => {}
        }
    }
}

fn request_first_frame(request_id: &str) -> Value {
    json!({"type": "first-frame", "protocol": 1, "phase": "request", "requestId": request_id})
}

async fn assert_ws_alive(ws: &mut Ws) {
    send_json(ws, json!({"type": "ping"})).await;
    let reply = common::next_json(ws, Duration::from_secs(2))
        .await
        .expect("pong on /ws");
    assert_eq!(reply["type"], "pong", "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn offer_is_answered_and_media_echo_and_first_frame_flow() {
    let harness = common::spawn_webrtc(sources(), Duration::from_secs(6), Knobs::default()).await;
    let (mut ws, _epoch) = signal_hello(harness.addr).await;
    let mut browser = Browser::offer(&mut ws).await;
    send_json(&mut ws, request_first_frame("ff-1")).await;
    browser
        .drive(&mut ws, Instant::now() + Duration::from_secs(5), |seen| {
            seen.connected
                && seen.video_frames > 0
                && seen.audio_frames > 0
                && !seen.echoes.is_empty()
                && seen.admitted.is_some()
        })
        .await;

    let answer = browser.seen.answer.clone().expect("the fake answered");
    assert!(answer.contains("m=video"), "{answer}");
    assert!(answer.contains("m=audio"), "{answer}");
    assert!(answer.lines().any(|line| line == "a=sendonly"), "{answer}");
    assert!(browser.seen.connected, "ICE and DTLS completed");
    assert!(browser.seen.video_frames > 0, "video reached the browser");
    assert!(browser.seen.audio_frames > 0, "tone reached the browser");
    assert!(browser.seen.echoes.iter().any(|echo| echo == &ECHO));
    // The RTC lane admits the first frame when its keyframe's first RTP packet
    // leaves: `admitted`, carrying that packet's RTP timestamp, and never `ready`.
    let admitted = browser
        .seen
        .admitted
        .clone()
        .expect("first-frame admitted on the RTC lane");
    assert_eq!(admitted["type"], "first-frame", "{admitted}");
    assert_eq!(admitted["protocol"], 1, "{admitted}");
    assert_eq!(admitted["requestId"], "ff-1", "{admitted}");
    assert_eq!(admitted["transport"], "webrtc", "{admitted}");
    assert_eq!(admitted["codec"], "h264", "{admitted}");
    assert!(
        admitted["generation"].as_u64().is_some_and(|g| g > 0),
        "{admitted}"
    );
    assert!(admitted["rtpTimestamp"].is_u64(), "{admitted}");
    assert!(!browser.seen.ready, "the RTC lane never sends WSS ready");
    for label in ["input-control", "input-motion"] {
        assert!(browser.seen.opened.iter().any(|open| open == label));
    }
    assert_ws_alive(&mut ws).await;

    // Each Opus payload is 10 ms of PCM. Observe wall-clock delivery after
    // connection so skipped late timer ticks cannot silently slow its RTP clock.
    let audio_before = browser.seen.audio_frames;
    browser
        .drive(&mut ws, Instant::now() + Duration::from_secs(1), |_| false)
        .await;
    assert!(
        browser.seen.audio_frames - audio_before >= 90,
        "10 ms Opus cadence: {} packets in one established second",
        browser.seen.audio_frames - audio_before
    );

    let snapshot = harness.finish().await;
    assert_eq!(snapshot.rtc.len(), 1, "one session, one report");
    let report = &snapshot.rtc[0];
    assert_eq!(report.ended, "run-complete");
    assert!(report.connected_ms.is_some());
    assert!(report.first_keyframe_ms.is_some());
    assert!(
        report.admitted_rtp_timestamp.is_some(),
        "the admission names a sent packet"
    );
    assert_eq!(
        browser
            .seen
            .admitted
            .as_ref()
            .and_then(|a| a["rtpTimestamp"].as_u64()),
        report.admitted_rtp_timestamp.map(u64::from),
        "the admitted timestamp is the RTP header timestamp that left"
    );
    assert!(report.video_rtp_packets > 0, "video RTP was sent");
    assert!(report.audio_rtp_packets > 0, "audio RTP was sent");
    assert!(report.echoes >= 1);
    assert!(report.channels.iter().any(|label| label == "input-motion"));
}

#[tokio::test(flavor = "multi_thread")]
async fn block_ice_answers_but_never_connects_and_ws_survives() {
    let knobs = Knobs {
        block_ice: true,
        ..Knobs::default()
    };
    let harness = common::spawn_webrtc(sources(), Duration::from_secs(4), knobs).await;
    let (mut ws, _epoch) = signal_hello(harness.addr).await;
    // A candidate with no session to take it is dropped, not an error.
    send_json(
        &mut ws,
        json!({"type": "ice", "candidate": "candidate:1 1 udp 2130706431 127.0.0.1 9 typ host"}),
    )
    .await;
    let mut browser = Browser::offer(&mut ws).await;
    browser
        .drive(
            &mut ws,
            Instant::now() + Duration::from_millis(2500),
            |_| false,
        )
        .await;

    assert!(browser.seen.answer.is_some(), "the offer is still answered");
    assert!(!browser.seen.connected, "no datagram gets through");
    assert_ws_alive(&mut ws).await;

    let snapshot = harness.finish().await;
    let report = &snapshot.rtc[0];
    assert!(report.connected_ms.is_none());
    assert!(
        report.blocked_datagrams > 0,
        "the fake saw and dropped the checks"
    );
    assert_eq!(report.video_rtp_packets, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn stall_first_frame_keeps_channels_and_audio_without_video() {
    let knobs = Knobs {
        stall_first_frame: true,
        ..Knobs::default()
    };
    let harness = common::spawn_webrtc(sources(), Duration::from_secs(5), knobs).await;
    let (mut ws, _epoch) = signal_hello(harness.addr).await;
    let mut browser = Browser::offer(&mut ws).await;
    // Browser receiver.js:342 retries RTC with a null media epoch.
    send_json(
        &mut ws,
        json!({"type":"media-control","action":"keyframe","protocol":4,
        "epoch":null,"reason":"first-frame-timeout","codec":"h264"}),
    )
    .await;
    send_json(&mut ws, request_first_frame("ff-2")).await;
    browser
        .drive(
            &mut ws,
            Instant::now() + Duration::from_millis(3500),
            |seen| seen.connected && seen.audio_frames > 0 && !seen.echoes.is_empty(),
        )
        .await;

    assert!(browser.seen.connected);
    assert!(browser.seen.audio_frames > 0, "audio still flows");
    assert!(browser.seen.echoes.iter().any(|echo| echo == &ECHO));
    assert_eq!(browser.seen.video_frames, 0, "no video is written");
    assert!(
        browser.seen.admitted.is_none() && !browser.seen.ready,
        "no keyframe leaves, so nothing is admitted or readied"
    );

    let snapshot = harness.finish().await;
    let report = &snapshot.rtc[0];
    assert_eq!(report.video_rtp_packets, 0);
    assert_eq!(report.video_frames, 0);
    assert!(report.video_frames_suppressed > 0);
    assert!(report.first_keyframe_ms.is_none());
    assert!(report.admitted_rtp_timestamp.is_none());
    assert!(report.audio_rtp_packets > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn rtc_drop_at_sends_close_notify_and_the_offerer_sees_closed() {
    let knobs = Knobs {
        drop_at: Some(Duration::from_millis(1000)),
        ..Knobs::default()
    };
    let harness = common::spawn_webrtc(sources(), Duration::from_secs(4), knobs).await;
    let (mut ws, _epoch) = signal_hello(harness.addr).await;
    let started = Instant::now();
    let mut browser = Browser::offer(&mut ws).await;
    browser
        .drive(
            &mut ws,
            Instant::now() + Duration::from_millis(3000),
            |seen| seen.closed_at.is_some(),
        )
        .await;

    // The drop is at 1 s; the close_notify must reach the offerer soon after it.
    let closed_at = browser
        .seen
        .closed_at
        .expect("the offerer got Event::Closed from the close_notify");
    assert!(
        closed_at.duration_since(started) < Duration::from_millis(2500),
        "Closed came {:?} after the offer, not promptly after the 1 s drop",
        closed_at.duration_since(started)
    );
    assert_ws_alive(&mut ws).await;

    let snapshot = harness.finish().await;
    assert_eq!(
        snapshot.rtc.len(),
        1,
        "the drop does not start a new session"
    );
    assert_eq!(snapshot.rtc[0].ended, "rtc-dropped");
    assert!(
        snapshot.rtc[0].close_notify_sent,
        "the fake sent its DTLS close_notify"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wss_start_stops_rtp_and_keeps_the_peer_and_channels_alive() {
    let harness = common::spawn_webrtc(sources(), Duration::from_secs(10), Knobs::default()).await;
    let (mut ws, epoch) = signal_hello(harness.addr).await;
    let mut browser = Browser::offer(&mut ws).await;
    browser
        .drive(&mut ws, Instant::now() + Duration::from_secs(4), |seen| {
            seen.connected
                && seen.video_frames > 0
                && seen.audio_frames > 0
                && !seen.echoes.is_empty()
        })
        .await;
    assert!(
        browser.seen.connected,
        "the RTC session is up before the switch"
    );

    send_json(&mut ws, common::start_message(&epoch, "h264")).await;
    // Let packets already in flight land, then freeze the counters.
    browser
        .drive(&mut ws, Instant::now() + Duration::from_millis(400), |_| {
            false
        })
        .await;
    let (video, audio) = (browser.seen.video_frames, browser.seen.audio_frames);
    browser
        .drive(
            &mut ws,
            Instant::now() + Duration::from_millis(1200),
            |_| false,
        )
        .await;
    assert_eq!(
        browser.seen.video_frames, video,
        "WSS took over: no RTP video after start"
    );
    assert_eq!(
        browser.seen.audio_frames, audio,
        "WSS took over: no RTP audio after start"
    );
    assert!(browser.seen.closed_at.is_none(), "the peer stays connected");

    // The motion lane is still open on the same channel and still answers.
    let mut sent = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !sent && Instant::now() < deadline {
        sent = browser.write_motion(&SYNC_AFTER);
        browser
            .drive(&mut ws, Instant::now() + Duration::from_millis(50), |_| {
                false
            })
            .await;
    }
    assert!(sent, "the motion lane still accepts writes");
    browser
        .drive(&mut ws, Instant::now() + Duration::from_secs(2), |seen| {
            seen.echoes.iter().any(|echo| echo == &ECHO_AFTER)
        })
        .await;
    assert!(
        browser.seen.echoes.iter().any(|echo| echo == &ECHO_AFTER),
        "the lane still answers after the switch"
    );
    assert_ws_alive(&mut ws).await;

    let snapshot = harness.finish().await;
    assert_eq!(snapshot.rtc.len(), 1);
    let report = &snapshot.rtc[0];
    assert!(report.media_stopped, "the report records the takeover");
    assert_eq!(report.ended, "run-complete", "the start did not end RTC");
    assert!(
        report.echoes >= 2,
        "the echo before and the echo after the switch"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_webrtc_the_offer_and_ice_are_counted_and_ws_answers() {
    let harness = common::spawn(
        common::synthetic(10, 5),
        common::no_injections(),
        Duration::from_millis(1500),
        Duration::ZERO,
    )
    .await;
    let (mut ws, _epoch) = signal_hello(harness.addr).await;
    send_json(&mut ws, json!({"type": "offer", "sdp": "v=0\r\n"})).await;
    send_json(
        &mut ws,
        json!({"type": "ice", "candidate": "candidate:1 1 udp 1 127.0.0.1 9 typ host"}),
    )
    .await;
    assert_ws_alive(&mut ws).await;

    let snapshot = harness.finish().await;
    let unhandled: HashMap<&str, u64> = snapshot
        .counters
        .unhandled_control
        .iter()
        .map(|(kind, count)| (kind.as_str(), *count))
        .collect();
    assert_eq!(unhandled.get("offer"), Some(&1));
    assert_eq!(unhandled.get("ice"), Some(&1));
    assert!(snapshot.rtc.is_empty());
}

/// A monitor switch on a live RTC session keeps the peer and channels, and the
/// next keyframe is admitted under the new generation with its own timestamp.
#[tokio::test(flavor = "multi_thread")]
async fn a_monitor_switch_admits_the_new_keyframe_on_the_same_peer() {
    let harness = common::spawn_monitored(
        sources(),
        Duration::from_secs(9),
        Duration::ZERO,
        Monitors::scripted(),
        Some(Knobs::default()),
    )
    .await;
    let (mut ws, epoch) = signal_hello(harness.addr).await;
    let mut browser = Browser::offer(&mut ws).await;
    send_json(&mut ws, request_first_frame("ff-1")).await;
    browser
        .drive(&mut ws, Instant::now() + Duration::from_secs(5), |seen| {
            seen.connected && seen.admitted.is_some()
        })
        .await;
    let first = browser.seen.admitted.clone().expect("first admission");
    assert_eq!(first["requestId"], "ff-1", "{first}");
    let first_ts = first["rtpTimestamp"].as_u64().expect("rtpTimestamp");

    send_json(
        &mut ws,
        json!({"type": "media-control", "action": "monitor", "protocol": 4, "epoch": epoch,
               "monitor": r"\\.\DISPLAY2", "reason": "user-monitor"}),
    )
    .await;
    browser
        .drive(&mut ws, Instant::now() + Duration::from_secs(3), |seen| {
            seen.signals
                .iter()
                .any(|m| m["reason"] == "monitor-switched")
                && seen
                    .signals
                    .iter()
                    .filter(|m| m["type"] == "monitor-list")
                    .count()
                    == 2
        })
        .await;

    let signals = browser.seen.signals.clone();
    let states: Vec<(&str, &str)> = signals
        .iter()
        .filter(|m| m["type"] == "desktop-state")
        .map(|m| {
            (
                m["state"].as_str().unwrap_or_default(),
                m["reason"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        states,
        vec![
            ("unavailable", "monitor-switch"),
            ("active", "monitor-switched")
        ],
        "{signals:?}"
    );
    let switched = signals
        .iter()
        .find(|m| m["type"] == "desktop-state" && m["state"] == "active")
        .expect("desktop-state active");
    let new_generation =
        u32::try_from(switched["generation"].as_u64().expect("generation")).expect("u32");
    assert_ne!(
        Some(u64::from(new_generation)),
        first["generation"].as_u64(),
        "the switch moves to a new generation"
    );
    let list = signals
        .iter()
        .rfind(|m| m["type"] == "monitor-list")
        .expect("monitor-list after the switch");
    assert_eq!(list["monitors"][1]["selected"], true, "{list}");

    // The browser asks again for a first frame; the switch's keyframe answers it.
    send_json(&mut ws, request_first_frame("ff-2")).await;
    browser
        .drive(&mut ws, Instant::now() + Duration::from_secs(4), |seen| {
            seen.admitted
                .as_ref()
                .is_some_and(|a| a["requestId"] == "ff-2")
        })
        .await;
    let second = browser.seen.admitted.clone().expect("second admission");
    assert_eq!(second["requestId"], "ff-2", "{second}");
    assert_eq!(second["transport"], "webrtc", "{second}");
    assert_eq!(
        second["generation"].as_u64(),
        Some(u64::from(new_generation)),
        "{second}"
    );
    assert_ne!(
        second["rtpTimestamp"].as_u64(),
        Some(first_ts),
        "the new keyframe carries its own timestamp"
    );
    assert!(!browser.seen.ready, "the RTC lane never sends WSS ready");
    assert_ws_alive(&mut ws).await;

    let snapshot = harness.finish().await;
    assert_eq!(snapshot.rtc.len(), 1, "one peer across the switch");
    assert_eq!(snapshot.counters.monitor_switches, 1);
    let report = &snapshot.rtc[0];
    assert_eq!(report.reseeks, 1, "the switch is the only reseek");
    assert!(
        report.admissions >= 2,
        "one admission per keyframe asked for"
    );
    assert_eq!(report.admitted_generation, Some(new_generation));
    assert_eq!(
        report.admitted_rtp_timestamp.map(u64::from),
        second["rtpTimestamp"].as_u64(),
        "the last admission names the post-switch keyframe"
    );
}
