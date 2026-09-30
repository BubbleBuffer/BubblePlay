//! WSS media protocol v4: one relay socket per session, carrying media
//! host->client and ACK / keyframe / heartbeat messages client->host.
//!
//! Wire format (host -> client binary, big endian, 28-byte header):
//!
//! | off | size | field                         |
//! |-----|------|-------------------------------|
//! | 0   | 4    | magic `BPV4` (0x42505634)     |
//! | 4   | 1    | version = 4                   |
//! | 5   | 1    | flags: bit0 key, bit1 refine  |
//! | 6   | 1    | codec: 1 HEVC, 2 H.264        |
//! | 7   | 1    | reserved = 0                  |
//! | 8   | 4    | generation                    |
//! | 12  | 4    | sequence                      |
//! | 16  | 8    | capture timestamp (us)        |
//! | 24  | 2    | coded width (0 = unknown)     |
//! | 26  | 2    | coded height (0 = unknown)    |
//! | 28  | ..   | one Annex-B access unit       |
//!
//! Client -> host JSON text on the same socket (validated by the relay):
//! `{"type":"ack","generation","seq","recvTsMs","decodedSeq"?}`,
//! `{"type":"keyframe","reason"}` and `{"type":"ping"}` (answered by the relay).
//!
//! The send policy and the delay-based bitrate controller are pure logic so
//! they can be unit tested without a socket or an encoder.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tracing::{debug, info, warn};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

pub const V4_MAGIC: u32 = 0x4250_5634;
pub const V4_VERSION: u8 = 4;
pub const V4_HEADER_BYTES: usize = 28;
pub const V4_MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
pub const V4_FLAG_KEY: u8 = 0x01;
pub const V4_FLAG_REFINE: u8 = 0x02;
const V4_KNOWN_FLAGS: u8 = V4_FLAG_KEY | V4_FLAG_REFINE;
pub const V4_MAX_REASON_CHARS: usize = 64;

/// In-flight budget: skip (before encode) once un-ACKed bytes exceed this
/// much airtime at the current bitrate, or once this many frames are
/// un-ACKed.
pub const INFLIGHT_BUDGET: Duration = Duration::from_millis(150);
pub const INFLIGHT_MAX_FRAMES: usize = 4;
/// No ACK progress for this long while frames are in flight means the
/// receiver lost its decoder chain (or vanished); restart from a keyframe.
pub const ACK_STALL_LIMIT: Duration = Duration::from_secs(3);
/// Safety keyframe interval, only while delta frames are being sent.
pub const SAFETY_KEYFRAME_INTERVAL: Duration = Duration::from_secs(30);
/// Idle heartbeat on the media socket.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// Declare the socket dead after this long without any inbound message
/// (the relay answers every heartbeat with a pong).
pub const INBOUND_SILENCE_LIMIT: Duration = Duration::from_secs(40);
pub const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(250);
pub const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);
pub const RATE_FLOOR_BPS: u64 = 1_000_000;
/// x0.8 on congestion, x1.1 after a clean interval (tenths).
const RATE_DECREASE_NUM: u64 = 8;
const RATE_INCREASE_NUM: u64 = 11;
const RATE_CLEAN_INTERVAL: Duration = Duration::from_secs(2);
/// A decrease holds off further decreases for this long so one congestion
/// episode (many late ACKs) is not counted as many.
const RATE_DECREASE_HOLD: Duration = Duration::from_millis(500);
const RATE_MIN_RTT_WINDOW: Duration = Duration::from_secs(30);
const RATE_RTT_SLACK_MS: f64 = 40.0;
const RATE_RTT_FACTOR: f64 = 1.5;
const WRITE_STALL_LIMIT: Duration = Duration::from_secs(10);
const LINK_QUEUE_CAPACITY: usize = INFLIGHT_MAX_FRAMES + 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V4FrameHeader {
    pub key: bool,
    pub refine: bool,
    pub codec: u8,
    pub generation: u32,
    pub seq: u32,
    pub capture_ts_us: u64,
    pub width: u16,
    pub height: u16,
}

pub fn encode_packet(header: &V4FrameHeader, payload: &[u8]) -> Result<Vec<u8>> {
    if payload.is_empty() || payload.len() > V4_MAX_PAYLOAD_BYTES {
        bail!("v4 access unit size out of range");
    }
    if !matches!(header.codec, 1 | 2) {
        bail!("v4 codec out of range");
    }
    let mut packet = Vec::with_capacity(V4_HEADER_BYTES + payload.len());
    packet.extend_from_slice(&V4_MAGIC.to_be_bytes());
    packet.push(V4_VERSION);
    let mut flags = 0;
    if header.key {
        flags |= V4_FLAG_KEY;
    }
    if header.refine {
        flags |= V4_FLAG_REFINE;
    }
    packet.push(flags);
    packet.push(header.codec);
    packet.push(0);
    packet.extend_from_slice(&header.generation.to_be_bytes());
    packet.extend_from_slice(&header.seq.to_be_bytes());
    packet.extend_from_slice(&header.capture_ts_us.to_be_bytes());
    packet.extend_from_slice(&header.width.to_be_bytes());
    packet.extend_from_slice(&header.height.to_be_bytes());
    packet.extend_from_slice(payload);
    Ok(packet)
}

pub fn decode_packet(packet: &[u8]) -> Result<(V4FrameHeader, &[u8])> {
    if packet.len() <= V4_HEADER_BYTES || packet.len() > V4_HEADER_BYTES + V4_MAX_PAYLOAD_BYTES {
        bail!("v4 packet size out of range");
    }
    let u32_at = |at: usize| u32::from_be_bytes(packet[at..at + 4].try_into().unwrap());
    let u16_at = |at: usize| u16::from_be_bytes(packet[at..at + 2].try_into().unwrap());
    if u32_at(0) != V4_MAGIC || packet[4] != V4_VERSION {
        bail!("v4 packet magic/version mismatch");
    }
    let flags = packet[5];
    if flags & !V4_KNOWN_FLAGS != 0 || !matches!(packet[6], 1 | 2) || packet[7] != 0 {
        bail!("v4 packet header invalid");
    }
    Ok((
        V4FrameHeader {
            key: flags & V4_FLAG_KEY != 0,
            refine: flags & V4_FLAG_REFINE != 0,
            codec: packet[6],
            generation: u32_at(8),
            seq: u32_at(12),
            capture_ts_us: u64::from_be_bytes(packet[16..24].try_into().unwrap()),
            width: u16_at(24),
            height: u16_at(26),
        },
        &packet[V4_HEADER_BYTES..],
    ))
}

/// Messages the host accepts on its v4 media socket.
#[derive(Debug, Clone, PartialEq)]
pub enum V4Inbound {
    Ack {
        generation: u32,
        seq: u32,
        recv_ts_ms: f64,
        decoded_seq: Option<u32>,
    },
    Keyframe {
        reason: String,
    },
    Ping,
    Pong,
    /// Relay notice that the browser side of the media room joined or left.
    MediaPeer {
        present: bool,
    },
    HelloOk,
}

fn u32_field(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|value| u32::try_from(value).ok())
}

fn valid_reason(reason: &str) -> bool {
    !reason.is_empty()
        && reason.len() <= V4_MAX_REASON_CHARS
        && reason
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.'))
}

fn only_keys(object: &serde_json::Map<String, Value>, keys: &[&str]) -> bool {
    object.keys().all(|key| keys.contains(&key.as_str()))
}

/// Parse a browser-originated v4 control message. Mirrors the relay's
/// validation so the host never trusts a message the relay would drop.
pub fn parse_client_message(text: &str) -> Option<V4Inbound> {
    if text.len() > 512 {
        return None;
    }
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    match object.get("type")?.as_str()? {
        "ack" => {
            if !only_keys(object, &["type", "generation", "seq", "recvTsMs", "decodedSeq"]) {
                return None;
            }
            let recv_ts_ms = object.get("recvTsMs")?.as_f64()?;
            if !recv_ts_ms.is_finite() || recv_ts_ms < 0.0 {
                return None;
            }
            let decoded_seq = match object.get("decodedSeq") {
                None => None,
                Some(value) => Some(u32_field(value)?),
            };
            Some(V4Inbound::Ack {
                generation: u32_field(object.get("generation")?)?,
                seq: u32_field(object.get("seq")?)?,
                recv_ts_ms,
                decoded_seq,
            })
        }
        "keyframe" => {
            if !only_keys(object, &["type", "reason"]) {
                return None;
            }
            let reason = object.get("reason")?.as_str()?;
            valid_reason(reason).then(|| V4Inbound::Keyframe {
                reason: reason.to_owned(),
            })
        }
        "ping" => only_keys(object, &["type"]).then_some(V4Inbound::Ping),
        _ => None,
    }
}

/// Parse anything the relay may put on the host's media socket: validated
/// client messages plus relay-originated pong / peer notices.
pub fn parse_inbound(text: &str) -> Option<V4Inbound> {
    if let Some(message) = parse_client_message(text) {
        return Some(message);
    }
    let value: Value = serde_json::from_str(text).ok()?;
    match value.get("type")?.as_str()? {
        "pong" => Some(V4Inbound::Pong),
        "hello-ok" => Some(V4Inbound::HelloOk),
        "media-peer" => Some(V4Inbound::MediaPeer {
            present: value.get("present")?.as_bool()?,
        }),
        _ => None,
    }
}

fn seq_reached(candidate: u32, target: u32) -> bool {
    candidate.wrapping_sub(target) < 0x8000_0000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Send,
    /// A delta frame cannot be decoded by the receiver; encode an IDR.
    RequireKeyframe,
    /// The in-flight budget is exhausted: skip capture/encode entirely.
    Backpressured,
}

#[derive(Debug, Clone, Copy)]
struct InFlight {
    generation: u32,
    seq: u32,
    bytes: u64,
    sent_at: Instant,
    key: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AckOutcome {
    pub rtt: Duration,
    pub key: bool,
}

/// ACK-based in-flight budget and keyframe policy (latest-wins: frames are
/// never queued behind the budget, they are simply not encoded).
#[derive(Debug)]
pub struct SendPolicy {
    inflight: VecDeque<InFlight>,
    inflight_bytes: u64,
    need_key: bool,
    last_key_sent: Option<Instant>,
    deltas_since_key: u64,
    last_progress: Instant,
}

impl SendPolicy {
    pub fn new(now: Instant) -> Self {
        Self {
            inflight: VecDeque::new(),
            inflight_bytes: 0,
            need_key: true,
            last_key_sent: None,
            deltas_since_key: 0,
            last_progress: now,
        }
    }

    pub fn budget_bytes(bitrate_bps: u64) -> u64 {
        (bitrate_bps as u128 * INFLIGHT_BUDGET.as_millis() / 8_000) as u64
    }

    pub fn over_budget(&self, bitrate_bps: u64) -> bool {
        self.inflight.len() >= INFLIGHT_MAX_FRAMES
            || (!self.inflight.is_empty() && self.inflight_bytes > Self::budget_bytes(bitrate_bps))
    }

    pub fn safety_keyframe_due(&self, now: Instant) -> bool {
        self.deltas_since_key > 0
            && self
                .last_key_sent
                .is_some_and(|sent| now.saturating_duration_since(sent) >= SAFETY_KEYFRAME_INTERVAL)
    }

    pub fn admit(&self, now: Instant, key: bool, bitrate_bps: u64) -> Admission {
        if self.over_budget(bitrate_bps) {
            Admission::Backpressured
        } else if !key && (self.need_key || self.safety_keyframe_due(now)) {
            Admission::RequireKeyframe
        } else {
            Admission::Send
        }
    }

    pub fn needs_keyframe(&self) -> bool {
        self.need_key
    }

    pub fn key_in_flight(&self) -> bool {
        self.inflight.iter().any(|frame| frame.key)
    }

    pub fn inflight_frames(&self) -> usize {
        self.inflight.len()
    }

    pub fn inflight_bytes(&self) -> u64 {
        self.inflight_bytes
    }

    pub fn on_sent(&mut self, now: Instant, generation: u32, seq: u32, bytes: u64, key: bool) {
        if self.inflight.is_empty() {
            self.last_progress = now;
        }
        self.inflight.push_back(InFlight {
            generation,
            seq,
            bytes,
            sent_at: now,
            key,
        });
        self.inflight_bytes += bytes;
        if key {
            self.need_key = false;
            self.last_key_sent = Some(now);
            self.deltas_since_key = 0;
        } else {
            self.deltas_since_key += 1;
        }
    }

    /// Retire every in-flight frame up to and including `seq`. Returns the
    /// RTT of the exact frame when it was still in flight.
    pub fn on_ack(&mut self, now: Instant, generation: u32, seq: u32) -> Option<AckOutcome> {
        if !self.inflight.iter().any(|frame| frame.generation == generation) {
            // Late ACK for retired frames, or a generation we never sent.
            return None;
        }
        let mut outcome = None;
        while let Some(front) = self.inflight.front().copied() {
            if front.generation == generation && !seq_reached(seq, front.seq) {
                break;
            }
            self.inflight.pop_front();
            self.inflight_bytes -= front.bytes;
            self.last_progress = now;
            if front.generation == generation && front.seq == seq {
                outcome = Some(AckOutcome {
                    rtt: now.saturating_duration_since(front.sent_at),
                    key: front.key,
                });
            }
        }
        outcome
    }

    /// True once the receiver has stopped ACKing frames in flight.
    pub fn expire(&mut self, now: Instant) -> bool {
        if !self.inflight.is_empty()
            && now.saturating_duration_since(self.last_progress) >= ACK_STALL_LIMIT
        {
            self.reset(now);
            return true;
        }
        false
    }

    pub fn request_keyframe(&mut self) {
        self.need_key = true;
    }

    /// The receiver's decoder chain is gone (reconnect / new peer).
    pub fn reset(&mut self, now: Instant) {
        self.inflight.clear();
        self.inflight_bytes = 0;
        self.need_key = true;
        self.last_progress = now;
    }
}

/// Delay-based bitrate controller.
///
/// Congested when an ACK RTT exceeds `1.5 x min + 40 ms` or when the send
/// policy had to skip a frame; congestion multiplies the bitrate by 0.8
/// (floor 1 Mbps). Two clean seconds with ACK traffic multiply it by 1.1,
/// capped at the profile maximum.
#[derive(Debug)]
pub struct DelayRateController {
    bitrate_bps: u64,
    max_bps: u64,
    rtt_samples: VecDeque<(Instant, f64)>,
    last_decrease: Option<Instant>,
    clean_since: Instant,
    last_increase: Instant,
    clean_samples: u32,
}

impl DelayRateController {
    pub fn new(now: Instant, bitrate_bps: u64, max_bps: u64) -> Self {
        let max_bps = max_bps.max(RATE_FLOOR_BPS);
        Self {
            bitrate_bps: bitrate_bps.clamp(RATE_FLOOR_BPS, max_bps),
            max_bps,
            rtt_samples: VecDeque::new(),
            last_decrease: None,
            clean_since: now,
            last_increase: now,
            clean_samples: 0,
        }
    }

    pub fn bitrate_bps(&self) -> u64 {
        self.bitrate_bps
    }

    pub fn max_bps(&self) -> u64 {
        self.max_bps
    }

    pub fn min_rtt_ms(&self) -> Option<f64> {
        self.rtt_samples
            .iter()
            .map(|(_, rtt)| *rtt)
            .fold(None, |min, rtt| Some(min.map_or(rtt, |min: f64| min.min(rtt))))
    }

    /// Profile change: new cap, and the bitrate the encoder now runs at.
    pub fn set_bounds(&mut self, now: Instant, bitrate_bps: u64, max_bps: u64) {
        self.max_bps = max_bps.max(RATE_FLOOR_BPS);
        self.bitrate_bps = bitrate_bps.clamp(RATE_FLOOR_BPS, self.max_bps);
        self.clean_since = now;
        self.last_increase = now;
        self.clean_samples = 0;
    }

    pub fn is_congested_rtt(min_rtt_ms: f64, rtt_ms: f64) -> bool {
        rtt_ms > RATE_RTT_FACTOR * min_rtt_ms + RATE_RTT_SLACK_MS
    }

    pub fn on_rtt(&mut self, now: Instant, rtt_ms: f64) -> Option<u64> {
        if !rtt_ms.is_finite() || rtt_ms < 0.0 {
            return None;
        }
        while self
            .rtt_samples
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > RATE_MIN_RTT_WINDOW)
        {
            self.rtt_samples.pop_front();
        }
        self.rtt_samples.push_back((now, rtt_ms));
        let min = self.min_rtt_ms().unwrap_or(rtt_ms);
        if Self::is_congested_rtt(min, rtt_ms) {
            self.congested(now)
        } else {
            self.clean_samples = self.clean_samples.saturating_add(1);
            None
        }
    }

    pub fn on_skip(&mut self, now: Instant) -> Option<u64> {
        self.congested(now)
    }

    fn congested(&mut self, now: Instant) -> Option<u64> {
        self.clean_since = now;
        self.clean_samples = 0;
        if self
            .last_decrease
            .is_some_and(|at| now.saturating_duration_since(at) < RATE_DECREASE_HOLD)
        {
            return None;
        }
        self.last_decrease = Some(now);
        let next = (self.bitrate_bps * RATE_DECREASE_NUM / 10).max(RATE_FLOOR_BPS);
        (next != self.bitrate_bps).then(|| {
            self.bitrate_bps = next;
            next
        })
    }

    pub fn tick(&mut self, now: Instant) -> Option<u64> {
        if self.bitrate_bps >= self.max_bps
            || self.clean_samples == 0
            || now.saturating_duration_since(self.clean_since) < RATE_CLEAN_INTERVAL
            || now.saturating_duration_since(self.last_increase) < RATE_CLEAN_INTERVAL
        {
            return None;
        }
        self.last_increase = now;
        self.clean_samples = 0;
        let next = (self.bitrate_bps * RATE_INCREASE_NUM / 10).min(self.max_bps);
        (next != self.bitrate_bps).then(|| {
            self.bitrate_bps = next;
            next
        })
    }
}

/// Unwraps the 32-bit 90 kHz media clock into a monotonic microsecond clock.
#[derive(Debug, Default)]
pub struct TimestampExtender {
    last: Option<u32>,
    high: u64,
}

impl TimestampExtender {
    pub fn extend_us(&mut self, timestamp90k: u32) -> u64 {
        if let Some(last) = self.last {
            if timestamp90k < last && last - timestamp90k > 0x8000_0000 {
                self.high += 1 << 32;
            }
        }
        self.last = Some(timestamp90k);
        (self.high + u64::from(timestamp90k)) * 100 / 9
    }
}

/// Exponential reconnect backoff: 250 ms doubling up to 5 s.
#[derive(Debug)]
pub struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            next: RECONNECT_BACKOFF_MIN,
        }
    }
}

impl Backoff {
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(RECONNECT_BACKOFF_MAX);
        delay
    }
    pub fn reset(&mut self) {
        self.next = RECONNECT_BACKOFF_MIN;
    }
}

/// Events the producer loop must act on.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct V4Events {
    /// The receiver needs a keyframe (reason), even if the screen is static.
    pub keyframe: Option<String>,
    /// New encoder bitrate from the delay controller.
    pub bitrate_bps: Option<u64>,
    /// Highest (generation, seq) ACKed by the receiver since the last poll.
    pub latest_ack: Option<(u32, u32)>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct V4Stats {
    pub connected: bool,
    pub peer_present: bool,
    pub frames_sent: u64,
    pub bytes_sent: u64,
    pub keyframes_sent: u64,
    pub refines_sent: u64,
    pub skipped_budget: u64,
    pub acks: u64,
    pub heartbeats: u64,
    pub reconnects: u64,
    pub inflight_frames: usize,
    pub inflight_bytes: u64,
    pub bitrate_bps: u64,
    pub min_rtt_ms: f64,
    pub last_rtt_ms: f64,
}

struct Shared {
    connected: bool,
    peer_present: bool,
    policy: SendPolicy,
    rate: DelayRateController,
    events: V4Events,
    timestamps: TimestampExtender,
    frame_size: (u16, u16),
    stats: V4Stats,
}

struct Outgoing {
    packet: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V4Reserve {
    Send,
    RequireKeyframe,
    Backpressured,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V4Submit {
    Queued,
    DroppedRequireIdr,
    Stopped,
}

pub type Connector =
    Box<dyn Fn() -> Result<WebSocket<MaybeTlsStream<TcpStream>>> + Send + 'static>;

/// Owns the single v4 media socket on a dedicated thread.
pub struct V4Link {
    shared: Arc<Mutex<Shared>>,
    tx: Option<SyncSender<Outgoing>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl V4Link {
    pub fn spawn(connector: Connector, bitrate_bps: u64, max_bps: u64) -> Result<Self> {
        let now = Instant::now();
        let shared = Arc::new(Mutex::new(Shared {
            connected: false,
            peer_present: false,
            policy: SendPolicy::new(now),
            rate: DelayRateController::new(now, bitrate_bps, max_bps),
            events: V4Events::default(),
            timestamps: TimestampExtender::default(),
            frame_size: (0, 0),
            stats: V4Stats::default(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(LINK_QUEUE_CAPACITY);
        let thread_shared = shared.clone();
        let thread_stop = stop.clone();
        let join = thread::Builder::new()
            .name("bubbleplay-wss-v4".into())
            .spawn(move || link_loop(connector, rx, thread_shared, thread_stop))
            .context("spawn v4 media link")?;
        Ok(Self {
            shared,
            tx: Some(tx),
            stop,
            join: Some(join),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn reserve(&self, key: bool) -> V4Reserve {
        if self.stop.load(Ordering::Acquire) {
            return V4Reserve::Stopped;
        }
        let now = Instant::now();
        let mut shared = self.lock();
        if !shared.connected || !shared.peer_present {
            return V4Reserve::Backpressured;
        }
        let bitrate = shared.rate.bitrate_bps();
        match shared.policy.admit(now, key, bitrate) {
            Admission::Send => V4Reserve::Send,
            Admission::RequireKeyframe => V4Reserve::RequireKeyframe,
            Admission::Backpressured => {
                shared.stats.skipped_budget += 1;
                // A keyframe in flight legitimately exceeds the airtime
                // budget; only count skips behind delta frames as congestion.
                if !shared.policy.key_in_flight() {
                    if let Some(next) = shared.rate.on_skip(now) {
                        shared.events.bitrate_bps = Some(next);
                    }
                }
                V4Reserve::Backpressured
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        &self,
        codec: u8,
        generation: u32,
        seq: u32,
        timestamp90k: u32,
        key: bool,
        refine: bool,
        payload: &[u8],
    ) -> V4Submit {
        let Some(tx) = self.tx.as_ref() else {
            return V4Submit::Stopped;
        };
        let now = Instant::now();
        let mut shared = self.lock();
        if !shared.connected || !shared.peer_present || (!key && shared.policy.needs_keyframe()) {
            shared.policy.request_keyframe();
            return V4Submit::DroppedRequireIdr;
        }
        let (width, height) = shared.frame_size;
        let capture_ts_us = shared.timestamps.extend_us(timestamp90k);
        let packet = match encode_packet(
            &V4FrameHeader {
                key,
                refine,
                codec,
                generation,
                seq,
                capture_ts_us,
                width,
                height,
            },
            payload,
        ) {
            Ok(packet) => packet,
            Err(_) => {
                shared.policy.request_keyframe();
                return V4Submit::DroppedRequireIdr;
            }
        };
        let bytes = packet.len() as u64;
        match tx.try_send(Outgoing { packet }) {
            Ok(()) => {
                shared.policy.on_sent(now, generation, seq, bytes, key);
                shared.stats.frames_sent += 1;
                shared.stats.bytes_sent += bytes;
                shared.stats.keyframes_sent += u64::from(key);
                shared.stats.refines_sent += u64::from(refine);
                V4Submit::Queued
            }
            Err(TrySendError::Full(_)) => {
                shared.policy.request_keyframe();
                V4Submit::DroppedRequireIdr
            }
            Err(TrySendError::Disconnected(_)) => V4Submit::Stopped,
        }
    }

    pub fn take_events(&self) -> V4Events {
        std::mem::take(&mut self.lock().events)
    }

    pub fn request_keyframe(&self) {
        self.lock().policy.request_keyframe();
    }

    pub fn bitrate_bps(&self) -> u64 {
        self.lock().rate.bitrate_bps()
    }

    pub fn set_bitrate_bounds(&self, bitrate_bps: u64, max_bps: u64) {
        self.lock()
            .rate
            .set_bounds(Instant::now(), bitrate_bps, max_bps);
    }

    pub fn set_frame_size(&self, width: u32, height: u32) {
        self.lock().frame_size = (
            u16::try_from(width).unwrap_or(0),
            u16::try_from(height).unwrap_or(0),
        );
    }

    pub fn stats(&self) -> V4Stats {
        let shared = self.lock();
        V4Stats {
            connected: shared.connected,
            peer_present: shared.peer_present,
            inflight_frames: shared.policy.inflight_frames(),
            inflight_bytes: shared.policy.inflight_bytes(),
            bitrate_bps: shared.rate.bitrate_bps(),
            min_rtt_ms: shared.rate.min_rtt_ms().unwrap_or(0.0),
            ..shared.stats
        }
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.tx = None;
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for V4Link {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn set_nonblocking(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> Result<()> {
    let tcp: &TcpStream = match ws.get_mut() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::NativeTls(stream) => stream.get_ref(),
        other => bail!("unsupported TLS stream variant: {other:?}"),
    };
    tcp.set_read_timeout(None).context("clear read timeout")?;
    tcp.set_write_timeout(None).context("clear write timeout")?;
    tcp.set_nodelay(true).context("enable TCP_NODELAY")?;
    tcp.set_nonblocking(true).context("set_nonblocking")?;
    Ok(())
}

fn would_block(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(io) if io.kind() == ErrorKind::WouldBlock)
}

fn sleep_unless_stopped(stop: &AtomicBool, delay: Duration) {
    let deadline = Instant::now() + delay;
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
}

fn link_loop(
    connector: Connector,
    rx: Receiver<Outgoing>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
) {
    // WSS epochs pace frames with 1 ms sleeps; hold the system timer period
    // for the lifetime of the media link.
    let _timer = crate::rtc::RtcTimerPeriod::acquire();
    let lock = || shared.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut backoff = Backoff::default();
    let mut first = true;
    while !stop.load(Ordering::Acquire) {
        let mut ws = match connector().and_then(|mut ws| {
            set_nonblocking(&mut ws)?;
            Ok(ws)
        }) {
            Ok(ws) => ws,
            Err(error) => {
                let delay = backoff.next_delay();
                warn!(error = %format!("{error:#}"), ?delay, "WSS v4 media connect failed");
                sleep_unless_stopped(&stop, delay);
                continue;
            }
        };
        backoff.reset();
        while rx.try_recv().is_ok() {}
        {
            let mut state = lock();
            let now = Instant::now();
            state.connected = true;
            // Presence is announced by the relay right after hello.
            state.peer_present = false;
            state.policy.reset(now);
            if !first {
                state.stats.reconnects += 1;
            }
        }
        first = false;
        info!("WSS v4 media socket connected");
        let result = serve_socket(&mut ws, &rx, &shared, &stop);
        {
            let mut state = lock();
            state.connected = false;
            state.peer_present = false;
            state.policy.reset(Instant::now());
        }
        let _ = ws.close(None);
        let _ = ws.flush();
        if let Err(error) = result {
            let delay = backoff.next_delay();
            warn!(error = %format!("{error:#}"), ?delay, "WSS v4 media socket lost; reconnecting");
            sleep_unless_stopped(&stop, delay);
        }
    }
}

fn flush_blocking(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
) -> Result<()> {
    let started = Instant::now();
    loop {
        match ws.flush() {
            Ok(()) => return Ok(()),
            Err(error) if would_block(&error) => {
                // Keep draining ACKs while the send buffer is full so RTT
                // samples are not delayed by our own write.
                read_available(ws, shared)?;
                if stop.load(Ordering::Acquire) {
                    bail!("stopping");
                }
                if started.elapsed() >= WRITE_STALL_LIMIT {
                    bail!("v4 media write stalled");
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error).context("flush v4 media socket"),
        }
    }
}

fn send_message(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    message: Message,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
) -> Result<()> {
    match ws.write(message) {
        Ok(()) => {}
        Err(error) if would_block(&error) => {}
        Err(error) => return Err(error).context("write v4 media socket"),
    }
    flush_blocking(ws, shared, stop)
}

/// Returns true when any inbound message arrived.
fn read_available(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    shared: &Mutex<Shared>,
) -> Result<bool> {
    let mut any = false;
    loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                any = true;
                handle_inbound(text.as_str(), shared);
            }
            Ok(Message::Close(_)) => bail!("v4 media socket closed by relay"),
            Ok(_) => any = true,
            Err(error) if would_block(&error) => return Ok(any),
            Err(error) => return Err(error).context("read v4 media socket"),
        }
    }
}

fn handle_inbound(text: &str, shared: &Mutex<Shared>) {
    let Some(message) = parse_inbound(text) else {
        debug!("ignored v4 media control message");
        return;
    };
    let now = Instant::now();
    let mut state = shared.lock().unwrap_or_else(|poison| poison.into_inner());
    match message {
        V4Inbound::Ack {
            generation, seq, ..
        } => {
            state.stats.acks += 1;
            if let Some(outcome) = state.policy.on_ack(now, generation, seq) {
                let rtt_ms = outcome.rtt.as_secs_f64() * 1_000.0;
                state.stats.last_rtt_ms = rtt_ms;
                // Keyframes carry their own serialization delay; only deltas
                // are representative delay samples.
                if !outcome.key {
                    if let Some(next) = state.rate.on_rtt(now, rtt_ms) {
                        state.events.bitrate_bps = Some(next);
                    }
                }
            }
            let newer = state.events.latest_ack.is_none_or(|(ack_generation, ack_seq)| {
                ack_generation != generation || seq_reached(seq, ack_seq)
            });
            if newer {
                state.events.latest_ack = Some((generation, seq));
            }
        }
        V4Inbound::Keyframe { reason } => {
            state.policy.request_keyframe();
            state.events.keyframe = Some(reason);
        }
        V4Inbound::MediaPeer { present } => {
            state.peer_present = present;
            state.policy.reset(now);
            if present {
                state.events.keyframe = Some("peer-joined".into());
            }
        }
        V4Inbound::Ping | V4Inbound::Pong | V4Inbound::HelloOk => {}
    }
}

fn serve_socket(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    rx: &Receiver<Outgoing>,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
) -> Result<()> {
    let mut last_outbound = Instant::now();
    let mut last_inbound = Instant::now();
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(outgoing) => {
                send_message(ws, Message::binary(outgoing.packet), shared, stop)?;
                last_outbound = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if read_available(ws, shared)? {
            last_inbound = Instant::now();
        }
        let now = Instant::now();
        if now.saturating_duration_since(last_outbound) >= HEARTBEAT_INTERVAL {
            send_message(ws, Message::text(r#"{"type":"ping"}"#), shared, stop)?;
            last_outbound = now;
            shared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .stats
                .heartbeats += 1;
        }
        if now.saturating_duration_since(last_inbound) >= INBOUND_SILENCE_LIMIT {
            bail!("v4 media socket silent");
        }
        let mut state = shared.lock().unwrap_or_else(|poison| poison.into_inner());
        if state.policy.expire(now) {
            state.events.keyframe = Some("ack-timeout".into());
        }
        if let Some(next) = state.rate.tick(now) {
            state.events.bitrate_bps = Some(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../fixtures/wss-v4-contract.json")).unwrap()
    }

    fn hex(value: &str) -> Vec<u8> {
        (0..value.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&value[at..at + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn fixture_packets_round_trip() {
        let contract = fixture();
        assert_eq!(contract["headerBytes"].as_u64().unwrap() as usize, V4_HEADER_BYTES);
        assert_eq!(
            contract["maxPayloadBytes"].as_u64().unwrap() as usize,
            V4_MAX_PAYLOAD_BYTES
        );
        for case in contract["packets"].as_array().unwrap() {
            let bytes = hex(case["hex"].as_str().unwrap());
            let accepted = case["accepted"].as_bool().unwrap();
            let decoded = decode_packet(&bytes);
            assert_eq!(decoded.is_ok(), accepted, "{}", case["name"]);
            if !accepted {
                continue;
            }
            let (header, payload) = decoded.unwrap();
            let expected = V4FrameHeader {
                key: case["key"].as_bool().unwrap(),
                refine: case["refine"].as_bool().unwrap(),
                codec: case["codec"].as_u64().unwrap() as u8,
                generation: case["generation"].as_u64().unwrap() as u32,
                seq: case["seq"].as_u64().unwrap() as u32,
                capture_ts_us: case["captureTsUs"].as_str().unwrap().parse().unwrap(),
                width: case["width"].as_u64().unwrap() as u16,
                height: case["height"].as_u64().unwrap() as u16,
            };
            assert_eq!(header, expected);
            assert_eq!(payload, hex(case["payloadHex"].as_str().unwrap()).as_slice());
            assert_eq!(encode_packet(&expected, payload).unwrap(), bytes);
        }
    }

    #[test]
    fn fixture_client_messages_match_relay_contract() {
        let contract = fixture();
        for case in contract["clientMessages"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            let parsed = parse_client_message(text);
            assert_eq!(parsed.is_some(), case["accepted"].as_bool().unwrap(), "{text}");
            if let Some(kind) = case.get("kind").and_then(Value::as_str) {
                let matches = match (kind, parsed.unwrap()) {
                    ("ack", V4Inbound::Ack { .. }) => true,
                    ("keyframe", V4Inbound::Keyframe { .. }) => true,
                    ("ping", V4Inbound::Ping) => true,
                    _ => false,
                };
                assert!(matches, "{text}");
            }
        }
    }

    #[test]
    fn relay_notices_parse() {
        assert_eq!(parse_inbound(r#"{"type":"pong"}"#), Some(V4Inbound::Pong));
        assert_eq!(
            parse_inbound(r#"{"type":"media-peer","present":true}"#),
            Some(V4Inbound::MediaPeer { present: true })
        );
        assert_eq!(parse_inbound(r#"{"type":"media-peer"}"#), None);
    }

    #[test]
    fn encode_rejects_empty_and_oversized_payloads() {
        let header = V4FrameHeader {
            key: true,
            refine: false,
            codec: 1,
            generation: 1,
            seq: 1,
            capture_ts_us: 0,
            width: 0,
            height: 0,
        };
        assert!(encode_packet(&header, &[]).is_err());
        assert!(encode_packet(&header, &vec![0; V4_MAX_PAYLOAD_BYTES + 1]).is_err());
        assert!(encode_packet(&V4FrameHeader { codec: 3, ..header }, &[1]).is_err());
    }

    #[test]
    fn policy_requires_key_first_then_admits_deltas() {
        let now = Instant::now();
        let mut policy = SendPolicy::new(now);
        assert_eq!(policy.admit(now, false, 6_000_000), Admission::RequireKeyframe);
        assert_eq!(policy.admit(now, true, 6_000_000), Admission::Send);
        policy.on_sent(now, 1, 1, 10_000, true);
        assert_eq!(policy.admit(now, false, 6_000_000), Admission::Send);
    }

    #[test]
    fn policy_budget_bytes_and_frames() {
        let now = Instant::now();
        // 6 Mbps x 150 ms = 112_500 bytes.
        assert_eq!(SendPolicy::budget_bytes(6_000_000), 112_500);
        let mut policy = SendPolicy::new(now);
        policy.on_sent(now, 1, 1, 112_500, true);
        assert_eq!(policy.admit(now, false, 6_000_000), Admission::Send);
        policy.on_sent(now, 1, 2, 1, false);
        assert_eq!(policy.admit(now, false, 6_000_000), Admission::Backpressured);
        policy.on_ack(now, 1, 2);
        assert_eq!(policy.inflight_frames(), 0);
        assert_eq!(policy.inflight_bytes(), 0);
        for seq in 3..7 {
            assert_eq!(policy.admit(now, false, 6_000_000), Admission::Send);
            policy.on_sent(now, 1, seq, 100, false);
        }
        assert_eq!(policy.inflight_frames(), INFLIGHT_MAX_FRAMES);
        assert_eq!(policy.admit(now, false, 6_000_000), Admission::Backpressured);
        let outcome = policy.on_ack(now + Duration::from_millis(30), 1, 4).unwrap();
        assert_eq!(outcome.rtt, Duration::from_millis(30));
        assert!(!outcome.key);
        assert_eq!(policy.inflight_frames(), 2);
        assert_eq!(policy.admit(now, false, 6_000_000), Admission::Send);
    }

    #[test]
    fn policy_ignores_foreign_generation_acks() {
        let now = Instant::now();
        let mut policy = SendPolicy::new(now);
        policy.on_sent(now, 5, 10, 100, true);
        assert_eq!(policy.on_ack(now, 9, 10), None);
        assert_eq!(policy.inflight_frames(), 1);
        // ACK of the next generation retires the previous generation.
        policy.on_sent(now, 6, 1, 100, true);
        assert!(policy.on_ack(now, 6, 1).is_some());
        assert_eq!(policy.inflight_frames(), 0);
    }

    #[test]
    fn policy_safety_keyframe_only_while_sending_deltas() {
        let start = Instant::now();
        let mut policy = SendPolicy::new(start);
        policy.on_sent(start, 1, 1, 100, true);
        policy.on_ack(start, 1, 1);
        let later = start + SAFETY_KEYFRAME_INTERVAL + Duration::from_secs(1);
        // Static screen: nothing sent since the key, no safety key.
        assert!(!policy.safety_keyframe_due(later));
        assert_eq!(policy.admit(later, true, 6_000_000), Admission::Send);
        policy.on_sent(start + Duration::from_secs(1), 1, 2, 100, false);
        policy.on_ack(start + Duration::from_secs(1), 1, 2);
        assert!(!policy.safety_keyframe_due(start + Duration::from_secs(29)));
        assert_eq!(policy.admit(later, false, 6_000_000), Admission::RequireKeyframe);
    }

    #[test]
    fn policy_stall_resets_and_requires_key() {
        let start = Instant::now();
        let mut policy = SendPolicy::new(start);
        policy.on_sent(start, 1, 1, 100, true);
        assert!(!policy.expire(start + Duration::from_secs(2)));
        assert!(policy.expire(start + ACK_STALL_LIMIT));
        assert!(policy.needs_keyframe());
        assert_eq!(policy.inflight_frames(), 0);
    }

    #[test]
    fn rate_decreases_on_delay_and_skip_with_floor() {
        let start = Instant::now();
        let mut rate = DelayRateController::new(start, 6_000_000, 6_000_000);
        assert_eq!(rate.on_rtt(start, 20.0), None);
        // 1.5 x 20 + 40 = 70 ms threshold.
        assert_eq!(rate.on_rtt(start + Duration::from_millis(10), 70.0), None);
        assert_eq!(
            rate.on_rtt(start + Duration::from_millis(20), 71.0),
            Some(4_800_000)
        );
        // Held for 500 ms.
        assert_eq!(rate.on_skip(start + Duration::from_millis(100)), None);
        assert_eq!(rate.on_skip(start + Duration::from_millis(600)), Some(3_840_000));
        let mut at = start + Duration::from_millis(600);
        for _ in 0..20 {
            at += Duration::from_secs(1);
            rate.on_skip(at);
        }
        assert_eq!(rate.bitrate_bps(), RATE_FLOOR_BPS);
    }

    #[test]
    fn rate_increases_after_two_clean_seconds_capped() {
        let start = Instant::now();
        let mut rate = DelayRateController::new(start, 5_000_000, 6_000_000);
        // No ACK traffic: no probing upward.
        assert_eq!(rate.tick(start + Duration::from_secs(3)), None);
        rate.on_rtt(start + Duration::from_secs(3), 20.0);
        assert_eq!(rate.tick(start + Duration::from_millis(3_500)), Some(5_500_000));
        rate.on_rtt(start + Duration::from_secs(4), 20.0);
        assert_eq!(rate.tick(start + Duration::from_secs(5)), None);
        assert_eq!(rate.tick(start + Duration::from_millis(5_500)), Some(6_000_000));
        rate.on_rtt(start + Duration::from_secs(6), 20.0);
        assert_eq!(rate.tick(start + Duration::from_secs(9)), None);
        // Congestion restarts the clean window.
        rate.on_skip(start + Duration::from_secs(10));
        rate.on_rtt(start + Duration::from_secs(11), 20.0);
        assert_eq!(rate.tick(start + Duration::from_millis(11_900)), None);
        assert_eq!(rate.tick(start + Duration::from_secs(12)), Some(5_280_000));
    }

    #[test]
    fn rate_bounds_clamp() {
        let start = Instant::now();
        let mut rate = DelayRateController::new(start, 500_000, 800_000);
        assert_eq!(rate.bitrate_bps(), RATE_FLOOR_BPS);
        rate.set_bounds(start, 9_000_000, 3_000_000);
        assert_eq!(rate.bitrate_bps(), 3_000_000);
        assert_eq!(rate.max_bps(), 3_000_000);
    }

    #[test]
    fn timestamps_unwrap_monotonically() {
        let mut ext = TimestampExtender::default();
        assert_eq!(ext.extend_us(90_000), 1_000_000);
        let before = ext.extend_us(u32::MAX);
        let after = ext.extend_us(9);
        assert!(after > before);
        assert_eq!(after, ((1u64 << 32) + 9) * 100 / 9);
    }

    #[test]
    fn backoff_doubles_to_five_seconds_and_resets() {
        let mut backoff = Backoff::default();
        let delays: Vec<_> = (0..7).map(|_| backoff.next_delay().as_millis()).collect();
        assert_eq!(delays, vec![250, 500, 1_000, 2_000, 4_000, 5_000, 5_000]);
        backoff.reset();
        assert_eq!(backoff.next_delay(), RECONNECT_BACKOFF_MIN);
    }
}
