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

use std::collections::{HashSet, VecDeque};
use std::io::ErrorKind;
use std::net::TcpStream;
use std::os::windows::io::{AsRawSocket, RawSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::activity::Activity;
use crate::audio::AudioFrame;
use crate::frame_damage::DamageRect;
use crate::link_capacity::{Capacity, CapacityEstimator};
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

/// In-flight budget: skip (before encode) once un-ACKed bytes exceed the
/// airtime budget at the current bitrate, or once the frame cap of un-ACKed
/// frames is reached. Both scale with the link's baseline RTT
/// ([`inflight_window`]); these are the floors (no RTT sample yet, or a
/// low-latency link) and the caps.
pub const INFLIGHT_BUDGET: Duration = Duration::from_millis(150);
pub const INFLIGHT_MAX_FRAMES: usize = 4;
pub const INFLIGHT_BUDGET_CAP: Duration = Duration::from_millis(400);
pub const INFLIGHT_MAX_FRAMES_CAP: usize = 12;
/// Frame cap = ceil((rtt + this) / frame interval) + 2; airtime budget =
/// rtt + [`INFLIGHT_BUDGET_RTT_EXTRA_MS`].
const INFLIGHT_FRAMES_RTT_EXTRA_MS: f64 = 40.0;
const INFLIGHT_FRAMES_SLACK: usize = 2;
const INFLIGHT_BUDGET_RTT_EXTRA_MS: f64 = 120.0;
/// Frame interval assumed until the producer reports the stream's fps.
const DEFAULT_FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
/// No ACK progress for this long while frames are in flight: forget the
/// in-flight frames so sending can resume. This never forces a keyframe; a
/// receiver whose decoder chain broke asks for one (seq gap / decode error).
pub const ACK_STALL_LIMIT: Duration = Duration::from_secs(3);
/// Audio (BPA1) lane: pending packets are bounded to this many (10 ms each);
/// the oldest is dropped when a new one arrives.
pub const AUDIO_QUEUE_FRAMES: usize = 10;
/// An oldest unacked video frame older than this suspends audio sends until an
/// ACK retires video. Audio never enters video admission, budget or keyframes.
pub const AUDIO_STALL_LIMIT: Duration = Duration::from_secs(1);
/// Idle heartbeat on the media socket.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// Declare the socket dead after this long without any inbound message
/// (the relay answers every heartbeat with a pong).
pub const INBOUND_SILENCE_LIMIT: Duration = Duration::from_secs(40);
pub const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(250);
pub const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);
/// Consecutive relay refusals of the media hello for the still-active epoch
/// after which the link stops and reports [`V4LinkEnd::Refused`].
pub const MEDIA_REFUSAL_LIMIT: u32 = 5;
/// Bitrate floor at 1080p; other frame sizes scale it by pixel count.
pub const RATE_FLOOR_1080P_BPS: u64 = 2_500_000;
/// Absolute floor for tiny frames (and profiles capped below the floor).
pub const RATE_FLOOR_MIN_BPS: u64 = 500_000;
const PIXELS_1080P: u64 = 1920 * 1080;
/// A new capacity target moves the bitrate only when it differs from the
/// current target by at least this much (percent); smaller moves would
/// reconfigure NVENC on every ACK.
const CAPACITY_TARGET_DEADBAND_PCT: u64 = 5;
/// x0.8 on sustained congestion, x1.15 after a clean interval (percent).
const RATE_DECREASE_PCT: u64 = 80;
const RATE_INCREASE_PCT: u64 = 115;
const RATE_CLEAN_INTERVAL: Duration = Duration::from_secs(1);
/// RTT growth (or ACK-less skipping) must persist this long before a cut;
/// after a cut the next one needs another full window.
const RATE_SUSTAIN: Duration = Duration::from_millis(500);
/// Baseline = minimum delta-frame ACK RTT over this window.
const RATE_MIN_RTT_WINDOW: Duration = Duration::from_secs(10);
const RATE_RTT_SLACK_MS: f64 = 60.0;
const RATE_RTT_FACTOR: f64 = 2.0;
const WRITE_STALL_LIMIT: Duration = Duration::from_secs(10);
const LINK_QUEUE_CAPACITY: usize = INFLIGHT_MAX_FRAMES_CAP + 2;
/// Longest `V4Link::shutdown` waits for its worker before abandoning it. A
/// worker stuck in DNS, TCP, TLS or upgrade finishes on its own later.
const LINK_SHUTDOWN_WAIT: Duration = Duration::from_millis(400);
/// Process-wide cap on media link workers, abandoned ones included.
const MAX_LINK_WORKERS: usize = 16;
static LINK_WORKERS: AtomicUsize = AtomicUsize::new(0);

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
            if !only_keys(
                object,
                &["type", "generation", "seq", "recvTsMs", "decodedSeq"],
            ) {
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

/// The in-flight limits for one link: frame cap and airtime budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InflightWindow {
    pub max_frames: usize,
    pub budget: Duration,
}

impl InflightWindow {
    /// Today's fixed window: used until an RTT sample exists.
    pub const FLOOR: Self = Self {
        max_frames: INFLIGHT_MAX_FRAMES,
        budget: INFLIGHT_BUDGET,
    };
}

/// RTT-scaled in-flight window. `min_rtt_ms` is the controller's baseline
/// (minimum delta-frame ACK RTT over 10 s, so queueing delay we cause does
/// not inflate the window); `None` means no sample yet.
///
/// frames = clamp(ceil((rtt + 40 ms) / frame interval) + 2, 4, 12);
/// budget = clamp(rtt + 120 ms, 150 ms, 400 ms).
pub fn inflight_window(min_rtt_ms: Option<f64>, frame_interval: Duration) -> InflightWindow {
    let Some(rtt_ms) = min_rtt_ms.filter(|rtt| rtt.is_finite() && *rtt >= 0.0) else {
        return InflightWindow::FLOOR;
    };
    let interval_ms = (frame_interval.as_secs_f64() * 1_000.0).max(1.0);
    let frames = ((rtt_ms + INFLIGHT_FRAMES_RTT_EXTRA_MS) / interval_ms).ceil() as usize
        + INFLIGHT_FRAMES_SLACK;
    let budget_ms = rtt_ms + INFLIGHT_BUDGET_RTT_EXTRA_MS;
    InflightWindow {
        max_frames: frames.clamp(INFLIGHT_MAX_FRAMES, INFLIGHT_MAX_FRAMES_CAP),
        budget: Duration::from_secs_f64(budget_ms / 1_000.0)
            .clamp(INFLIGHT_BUDGET, INFLIGHT_BUDGET_CAP),
    }
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
/// never queued behind the budget, they are simply not encoded). Keyframes
/// are required only at start, after a reset (reconnect / new peer) and on an
/// explicit receiver request; there is no periodic or safety keyframe.
#[derive(Debug)]
pub struct SendPolicy {
    inflight: VecDeque<InFlight>,
    inflight_bytes: u64,
    need_key: bool,
    last_progress: Instant,
    window: InflightWindow,
}

impl SendPolicy {
    pub fn new(now: Instant) -> Self {
        Self {
            inflight: VecDeque::new(),
            inflight_bytes: 0,
            need_key: true,
            last_progress: now,
            window: InflightWindow::FLOOR,
        }
    }

    /// Airtime budget at the floor window (150 ms).
    pub fn budget_bytes(bitrate_bps: u64) -> u64 {
        Self::budget_bytes_for(bitrate_bps, INFLIGHT_BUDGET)
    }

    fn budget_bytes_for(bitrate_bps: u64, budget: Duration) -> u64 {
        (bitrate_bps as u128 * budget.as_millis() / 8_000) as u64
    }

    pub fn set_window(&mut self, window: InflightWindow) {
        self.window = window;
    }

    pub fn window(&self) -> InflightWindow {
        self.window
    }

    pub fn over_budget(&self, bitrate_bps: u64) -> bool {
        self.inflight.len() >= self.window.max_frames
            || (!self.inflight.is_empty()
                && self.inflight_bytes > Self::budget_bytes_for(bitrate_bps, self.window.budget))
    }

    pub fn admit(&self, key: bool, bitrate_bps: u64) -> Admission {
        if self.over_budget(bitrate_bps) {
            Admission::Backpressured
        } else if !key && self.need_key {
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

    /// The oldest video frame still awaiting its ACK: (generation, seq, age).
    pub fn oldest_unacked(&self, now: Instant) -> Option<(u32, u32, Duration)> {
        self.inflight.front().map(|frame| {
            (
                frame.generation,
                frame.seq,
                now.saturating_duration_since(frame.sent_at),
            )
        })
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
        }
    }

    /// Retire every in-flight frame up to and including `seq`. Returns the
    /// RTT of the exact frame when it was still in flight.
    pub fn on_ack(&mut self, now: Instant, generation: u32, seq: u32) -> Option<AckOutcome> {
        if !self
            .inflight
            .iter()
            .any(|frame| frame.generation == generation)
        {
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

    /// True once the receiver has stopped ACKing frames in flight; those
    /// frames are forgotten (no keyframe is required).
    pub fn expire(&mut self, now: Instant) -> bool {
        if !self.inflight.is_empty()
            && now.saturating_duration_since(self.last_progress) >= ACK_STALL_LIMIT
        {
            self.inflight.clear();
            self.inflight_bytes = 0;
            self.last_progress = now;
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

/// Why the delay controller changed the bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateReason {
    /// ACK RTT stayed above `2 x min + 60 ms` for the sustain window.
    RttGrowth,
    /// Frames were skipped for the sustain window without a clean ACK.
    SustainedSkips,
    /// A clean interval with ACK traffic: step up.
    Clean,
    /// The measured link capacity moved (constrained link only).
    Capacity,
}

impl RateReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RttGrowth => "rtt-growth",
            Self::SustainedSkips => "sustained-skips",
            Self::Clean => "clean",
            Self::Capacity => "capacity",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateDecision {
    pub bitrate_bps: u64,
    pub previous_bps: u64,
    pub reason: RateReason,
}

impl RateDecision {
    /// Worth an info log line: a delay or skip signal, or a cut to under 80 % of the previous
    /// rate (a collapse onto a constrained link). Stepping up and gentle capacity tracking are
    /// routine and log at debug.
    pub fn is_notable(&self) -> bool {
        !matches!(self.reason, RateReason::Clean | RateReason::Capacity)
            || self.bitrate_bps.saturating_mul(5) < self.previous_bps.saturating_mul(4)
    }
}

/// Bitrate floor for a frame size: 2.5 Mbps at 1080p, scaled by pixels.
pub fn rate_floor_for_pixels(width: u32, height: u32) -> u64 {
    let pixels = u64::from(width) * u64::from(height);
    if pixels == 0 {
        return RATE_FLOOR_1080P_BPS;
    }
    (RATE_FLOOR_1080P_BPS * pixels / PIXELS_1080P).max(RATE_FLOOR_MIN_BPS)
}

/// Delay-based bitrate controller.
///
/// The baseline is the minimum delta-frame ACK RTT over 10 s. Only sustained
/// growth reacts: RTT above `2 x min + 60 ms` for 500 ms multiplies the
/// bitrate by 0.8, never below a floor of 2.5 Mbps at 1080p (scaled by
/// pixels). Budget skips are the first response to a burst; they cut the
/// bitrate only when they persist for 500 ms without a clean ACK. One clean
/// second with ACK traffic steps up by x1.15, capped at the profile maximum.
///
/// On a constrained link the measured capacity sets a target (80 % of it):
/// the bitrate follows the target, never steps above it, and the floor gives
/// way to it (a measured 2 Mbps link cannot carry a 2.5 Mbps floor). With no
/// target (unconstrained) the controller behaves as above.
#[derive(Debug)]
pub struct DelayRateController {
    bitrate_bps: u64,
    max_bps: u64,
    capacity_target_bps: Option<u64>,
    floor_for_frame_bps: u64,
    rtt_samples: VecDeque<(Instant, f64)>,
    high_rtt_since: Option<Instant>,
    skipping_since: Option<Instant>,
    clean_since: Instant,
    last_increase: Instant,
    clean_samples: u32,
}

impl DelayRateController {
    pub fn new(now: Instant, bitrate_bps: u64, max_bps: u64) -> Self {
        let mut controller = Self {
            bitrate_bps,
            max_bps,
            capacity_target_bps: None,
            floor_for_frame_bps: RATE_FLOOR_1080P_BPS,
            rtt_samples: VecDeque::new(),
            high_rtt_since: None,
            skipping_since: None,
            clean_since: now,
            last_increase: now,
            clean_samples: 0,
        };
        controller.set_bounds(now, bitrate_bps, max_bps);
        controller
    }

    pub fn bitrate_bps(&self) -> u64 {
        self.bitrate_bps
    }

    pub fn max_bps(&self) -> u64 {
        self.max_bps
    }

    /// The effective floor: the frame-size floor, but never above the cap
    /// or a capacity target.
    pub fn floor_bps(&self) -> u64 {
        self.floor_for_frame_bps
            .min(self.max_bps)
            .min(self.capacity_target_bps.unwrap_or(u64::MAX))
    }

    /// The effective ceiling: the profile maximum, or a lower capacity target.
    pub fn ceiling_bps(&self) -> u64 {
        self.max_bps
            .min(self.capacity_target_bps.unwrap_or(u64::MAX))
    }

    pub fn capacity_target_bps(&self) -> Option<u64> {
        self.capacity_target_bps
    }

    /// A new capacity target from the link estimate (`None`: unconstrained).
    /// The bitrate moves to the target; moves within the deadband are
    /// ignored.
    pub fn set_capacity_target(
        &mut self,
        now: Instant,
        target_bps: Option<u64>,
    ) -> Option<RateDecision> {
        let target_bps = target_bps.map(|bps| bps.max(RATE_FLOOR_MIN_BPS));
        if let (Some(old), Some(new)) = (self.capacity_target_bps, target_bps) {
            if old.abs_diff(new) * 100 < old * CAPACITY_TARGET_DEADBAND_PCT {
                return None;
            }
        }
        if target_bps == self.capacity_target_bps {
            return None;
        }
        self.capacity_target_bps = target_bps;
        let target_bps = target_bps?;
        self.clean_since = now;
        self.last_increase = now;
        self.clean_samples = 0;
        self.high_rtt_since = None;
        self.skipping_since = None;
        let previous_bps = self.bitrate_bps;
        let next = target_bps.min(self.max_bps);
        (next != previous_bps).then(|| {
            self.bitrate_bps = next;
            RateDecision {
                bitrate_bps: next,
                previous_bps,
                reason: RateReason::Capacity,
            }
        })
    }

    pub fn min_rtt_ms(&self) -> Option<f64> {
        self.rtt_samples
            .iter()
            .map(|(_, rtt)| *rtt)
            .fold(None, |min, rtt| {
                Some(min.map_or(rtt, |min: f64| min.min(rtt)))
            })
    }

    /// Profile change: new cap, and the bitrate the encoder now runs at.
    pub fn set_bounds(&mut self, now: Instant, bitrate_bps: u64, max_bps: u64) {
        self.max_bps = max_bps.max(RATE_FLOOR_MIN_BPS);
        self.bitrate_bps = bitrate_bps.clamp(self.floor_bps(), self.ceiling_bps());
        self.clean_since = now;
        self.last_increase = now;
        self.clean_samples = 0;
        self.high_rtt_since = None;
        self.skipping_since = None;
    }

    /// The coded frame size sets the floor (2.5 Mbps at 1080p).
    pub fn set_frame_pixels(&mut self, width: u32, height: u32) {
        self.floor_for_frame_bps = rate_floor_for_pixels(width, height);
    }

    pub fn is_congested_rtt(min_rtt_ms: f64, rtt_ms: f64) -> bool {
        rtt_ms > RATE_RTT_FACTOR * min_rtt_ms + RATE_RTT_SLACK_MS
    }

    pub fn on_rtt(&mut self, now: Instant, rtt_ms: f64) -> Option<RateDecision> {
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
        if !Self::is_congested_rtt(min, rtt_ms) {
            // A clean ACK ends both congestion episodes.
            self.high_rtt_since = None;
            self.skipping_since = None;
            self.clean_samples = self.clean_samples.saturating_add(1);
            return None;
        }
        self.hold_clean(now);
        let since = *self.high_rtt_since.get_or_insert(now);
        if now.saturating_duration_since(since) < RATE_SUSTAIN {
            return None;
        }
        self.high_rtt_since = Some(now);
        self.decrease(RateReason::RttGrowth)
    }

    /// The send policy skipped a frame behind the in-flight budget. Skipping
    /// is the first response; the bitrate is cut only if it is sustained.
    pub fn on_skip(&mut self, now: Instant) -> Option<RateDecision> {
        self.hold_clean(now);
        let since = *self.skipping_since.get_or_insert(now);
        if now.saturating_duration_since(since) < RATE_SUSTAIN {
            return None;
        }
        self.skipping_since = Some(now);
        self.decrease(RateReason::SustainedSkips)
    }

    fn hold_clean(&mut self, now: Instant) {
        self.clean_since = now;
        self.clean_samples = 0;
    }

    fn decrease(&mut self, reason: RateReason) -> Option<RateDecision> {
        let previous_bps = self.bitrate_bps;
        let next = (previous_bps * RATE_DECREASE_PCT / 100).max(self.floor_bps());
        (next < previous_bps).then(|| {
            self.bitrate_bps = next;
            RateDecision {
                bitrate_bps: next,
                previous_bps,
                reason,
            }
        })
    }

    pub fn tick(&mut self, now: Instant) -> Option<RateDecision> {
        if self.bitrate_bps >= self.ceiling_bps()
            || self.clean_samples == 0
            || now.saturating_duration_since(self.clean_since) < RATE_CLEAN_INTERVAL
            || now.saturating_duration_since(self.last_increase) < RATE_CLEAN_INTERVAL
        {
            return None;
        }
        self.last_increase = now;
        self.clean_samples = 0;
        let previous_bps = self.bitrate_bps;
        let next = (previous_bps * RATE_INCREASE_PCT / 100)
            .max(self.floor_bps())
            .min(self.ceiling_bps());
        (next > previous_bps).then(|| {
            self.bitrate_bps = next;
            RateDecision {
                bitrate_bps: next,
                previous_bps,
                reason: RateReason::Clean,
            }
        })
    }
}

/// Share of a `width` x `height` frame covered by `rects`, in per mille.
/// Unknown damage (`None`) counts as the whole frame.
pub fn damage_per_mille(rects: Option<&[DamageRect]>, width: u32, height: u32) -> u64 {
    let total = u64::from(width) * u64::from(height);
    let Some(rects) = rects else {
        return 1_000;
    };
    if total == 0 {
        return 0;
    }
    let area: u64 = rects
        .iter()
        .map(|rect| {
            let w = rect.right.min(width).saturating_sub(rect.left);
            let h = rect.bottom.min(height).saturating_sub(rect.top);
            u64::from(w) * u64::from(h)
        })
        .sum();
    (area.min(total) * 1_000 / total).min(1_000)
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
    /// Why the delay controller chose `bitrate_bps`.
    pub bitrate_reason: Option<&'static str>,
    /// Highest (generation, seq) ACKed by the receiver since the last poll.
    pub latest_ack: Option<(u32, u32)>,
    /// The link stopped reconnecting on its own (reported once).
    pub link_end: Option<V4LinkEnd>,
}

/// Why a link stopped reconnecting without being shut down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V4LinkEnd {
    /// The relay closed a socket of an epoch that is no longer active. A newer
    /// media start replaces the link; it never reconnects.
    Retired { code: u16, reason: String },
    /// The relay refused `refusals` consecutive media hellos for the active
    /// epoch (for example an expired ticket). The session must re-register to
    /// get a fresh ticket and route.
    Refused {
        code: u16,
        reason: String,
        refusals: u32,
    },
}

/// A Close frame from the relay. A missing frame is reported as 1005.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RelayClose {
    code: u16,
    reason: String,
}

impl std::fmt::Display for RelayClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "v4 media socket closed by relay (code {}, reason {:?})",
            self.code, self.reason
        )
    }
}

impl std::error::Error for RelayClose {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseVerdict {
    Retry,
    Retired,
    Refused,
}

/// Closes the relay sends in normal operation: the viewer left (1001 "viewer departed"), or the
/// epoch was already replaced ("stale"/"retired", or no longer ours). They log at info.
fn close_is_expected(close: &RelayClose, epoch_active: bool) -> bool {
    let reason = close.reason.to_ascii_lowercase();
    reason.contains("viewer departed")
        || reason.contains("stale")
        || reason.contains("retired")
        || !epoch_active
}

/// What a socket's end means for the link. `refusals` counts consecutive
/// refusals including this one. Only relay Close frames can end the link.
fn close_verdict(
    close: Option<&RelayClose>,
    accepted: bool,
    epoch_active: bool,
    refusals: u32,
) -> CloseVerdict {
    let Some(close) = close else {
        return CloseVerdict::Retry;
    };
    let reason = close.reason.to_ascii_lowercase();
    if reason.contains("stale") || reason.contains("retired") || !epoch_active {
        return CloseVerdict::Retired;
    }
    if !accepted && refusals >= MEDIA_REFUSAL_LIMIT {
        return CloseVerdict::Refused;
    }
    CloseVerdict::Retry
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
    /// ACK stalls that dropped the in-flight window (no keyframe).
    pub ack_timeouts: u64,
    pub inflight_frames: usize,
    pub inflight_bytes: u64,
    pub bitrate_bps: u64,
    pub min_rtt_ms: f64,
    pub last_rtt_ms: f64,
    /// Measured capacity of a constrained link (0: unconstrained).
    pub capacity_bps: u64,
    /// The estimate capping the bitrate: the constrained capacity, or a
    /// released one being probed up (0: none).
    pub rate_cap_bps: u64,
    /// Total time the capacity bound the producer (budget, pacing, ceiling).
    pub constrained_ms: u64,
    /// BPA1 audio packets written to the media socket.
    pub audio_frames_sent: u64,
    pub audio_bytes_sent: u64,
    /// Audio frames dropped: queue overflow, stall, disconnect or encode error.
    pub audio_frames_dropped: u64,
    /// Audio sends are suspended by an unacked video stall.
    pub audio_suspended: bool,
}

struct Shared {
    producer_activity: Option<Activity>,
    peer_epoch: u64,
    connected: bool,
    /// The relay accepted the current socket's hello (hello-ok, presence or ACK).
    accepted: bool,
    /// Cleared by [`V4Link::retire_epoch`] once the host moved off this epoch.
    epoch_active: bool,
    peer_present: bool,
    policy: SendPolicy,
    rate: DelayRateController,
    capacity: CapacityEstimator,
    /// Last capacity update, for `constrained_ms`.
    capacity_tick: Option<Instant>,
    events: V4Events,
    timestamps: TimestampExtender,
    frame_size: (u16, u16),
    /// Stream frame interval, for the RTT-scaled in-flight window.
    frame_interval: Duration,
    stats: V4Stats,
    audio: AudioLane,
}

impl Shared {
    /// Latch the audio stall once the oldest unacked video frame is older than
    /// `AUDIO_STALL_LIMIT`. Must run before anything that can drop that frame
    /// from the in-flight window (ACK retirement, stall expiry).
    fn observe_audio(&mut self, now: Instant) {
        self.audio.observe(self.policy.oldest_unacked(now));
    }

    /// The next audio packet to write, or `None` while audio is not sendable
    /// (no opted-in current peer, stalled, or empty).
    fn next_audio(&mut self, now: Instant) -> Option<AudioFrame> {
        self.observe_audio(now);
        if !(self.connected && self.peer_present) {
            return None;
        }
        self.audio.take_next()
    }

    /// Accept one audio frame into the lane. Touches no video state.
    fn queue_audio(&mut self, now: Instant, frame: AudioFrame) -> V4AudioSubmit {
        if !self.audio.enabled {
            return V4AudioSubmit::Disabled;
        }
        self.observe_audio(now);
        if !(self.connected && self.peer_present) || self.audio.stalled {
            self.audio.mark_gap(1);
            return V4AudioSubmit::Unavailable;
        }
        self.audio.push(frame);
        V4AudioSubmit::Queued
    }
}

/// Retire ACKed video frames. Audio's stall is settled against the window
/// before and after the ACK: retiring frames is progress, and an ACK covering
/// the fence frame clears it even after SendPolicy expired that frame. A frame
/// that is still too old re-latches the stall. Video policy is unchanged.
fn retire_video(state: &mut Shared, now: Instant, generation: u32, seq: u32) -> Option<AckOutcome> {
    state.observe_audio(now);
    let before = state.policy.inflight_frames();
    let outcome = state.policy.on_ack(now, generation, seq);
    let progressed = state.policy.inflight_frames() < before;
    state.audio.on_ack(progressed, generation, seq);
    state.observe_audio(now);
    outcome
}

struct Outgoing {
    peer_epoch: u64,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V4AudioSubmit {
    Queued,
    /// Audio is not opted in on this link; nothing is kept.
    Disabled,
    /// No opted-in peer on a live socket, or video is stalled: dropped.
    Unavailable,
    Stopped,
}

/// Send-side audio lane. Audio shares the socket writer with video but none of
/// its admission: no in-flight budget, no keyframe requests, no video queue.
#[derive(Debug, Default)]
struct AudioLane {
    /// Set by the coordinator from the viewer's `audio:1` start; off by default.
    enabled: bool,
    queue: VecDeque<AudioFrame>,
    /// Something was dropped, stalled or reset: the next packet is marked
    /// discontinuous so the receiver resets its playout buffer.
    gap: bool,
    /// Latched when the oldest unacked video frame passed `AUDIO_STALL_LIMIT`.
    /// Cleared by video progress, by an ACK covering `fence`, or by a new
    /// socket / peer (`link_reset`). An expired video window does not clear it.
    stalled: bool,
    /// (generation, seq) of the oldest unacked video frame when the stall
    /// latched. Kept after SendPolicy expires that frame, so its late ACK can
    /// still end the stall. Set only while `stalled`.
    fence: Option<(u32, u32)>,
    /// Audio frames dropped for any reason, reported in `V4Stats`.
    dropped: u64,
}

impl AudioLane {
    fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.discard();
        }
    }

    fn mark_gap(&mut self, dropped: u64) {
        self.dropped += dropped;
        self.gap = true;
    }

    /// Drop the whole backlog; the next packet carries the discontinuity flag.
    fn discard(&mut self) {
        let count = self.queue.len() as u64;
        self.queue.clear();
        self.mark_gap(count);
    }

    /// Bounded queue: when full, the oldest pending frame goes.
    fn push(&mut self, frame: AudioFrame) {
        if self.queue.len() >= AUDIO_QUEUE_FRAMES {
            self.queue.pop_front();
            self.mark_gap(1);
        }
        self.queue.push_back(frame);
    }

    /// Latch the stall when the oldest unacked video frame is too old, fencing
    /// on that frame. A stall drops the backlog so audio resumes fresh rather
    /// than behind a delay.
    fn observe(&mut self, oldest: Option<(u32, u32, Duration)>) {
        if self.stalled {
            return;
        }
        if let Some((generation, seq, age)) = oldest {
            if age > AUDIO_STALL_LIMIT {
                self.stalled = true;
                self.fence = Some((generation, seq));
                self.discard();
            }
        }
    }

    /// Settle the stall against one inbound video ACK. Retiring video frames is
    /// progress. An ACK for the fence generation whose seq reaches the fence
    /// frame also clears it, even after that frame expired. Any other ACK is
    /// stale for audio and leaves the stall latched.
    fn on_ack(&mut self, progressed: bool, generation: u32, seq: u32) {
        let covers_fence = self.fence.is_some_and(|(fence_generation, fence_seq)| {
            fence_generation == generation && seq_reached(seq, fence_seq)
        });
        if progressed || covers_fence {
            self.stalled = false;
            self.fence = None;
        }
    }

    /// A new media socket or peer epoch: video in flight belongs to the old
    /// one, so the stall, its fence and the backlog go with it.
    fn link_reset(&mut self) {
        self.stalled = false;
        self.fence = None;
        self.discard();
    }

    fn take_next(&mut self) -> Option<AudioFrame> {
        if !self.enabled || self.stalled {
            return None;
        }
        let mut frame = self.queue.pop_front()?;
        if std::mem::take(&mut self.gap) {
            frame.discontinuity = true;
        }
        Some(frame)
    }
}

pub type Connector = Box<dyn Fn() -> Result<WebSocket<MaybeTlsStream<TcpStream>>> + Send + 'static>;

/// One live media worker in the process-wide budget; released when the worker ends.
struct WorkerSlot(&'static AtomicUsize);

impl Drop for WorkerSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Claims a worker slot. Abandoned workers keep their slot until their connect returns.
fn claim_worker_slot(live: &'static AtomicUsize, limit: usize) -> Result<WorkerSlot> {
    if live
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < limit).then_some(count + 1)
        })
        .is_err()
    {
        bail!("v4 media link worker budget exhausted ({limit} workers live)");
    }
    Ok(WorkerSlot(live))
}

/// Owns the single v4 media socket on a dedicated thread.
pub struct V4Link {
    shared: Arc<Mutex<Shared>>,
    tx: SyncSender<Outgoing>,
    stop: Arc<AtomicBool>,
    activity: Activity,
    /// The worker and a signal that disconnects when the worker ends.
    worker: Mutex<Option<(JoinHandle<()>, Receiver<()>)>>,
}

impl V4Link {
    pub fn spawn(connector: Connector, bitrate_bps: u64, max_bps: u64) -> Result<Self> {
        Self::spawn_for_epoch(connector, "", bitrate_bps, max_bps)
    }

    /// [`Self::spawn`] with the epoch the connector addresses, for logs.
    pub fn spawn_for_epoch(
        connector: Connector,
        epoch: &str,
        bitrate_bps: u64,
        max_bps: u64,
    ) -> Result<Self> {
        let epoch = epoch.to_owned();
        let now = Instant::now();
        let activity = Activity::new()?;
        let shared = Arc::new(Mutex::new(Shared {
            producer_activity: None,
            peer_epoch: 0,
            connected: false,
            accepted: false,
            epoch_active: true,
            peer_present: false,
            policy: SendPolicy::new(now),
            rate: DelayRateController::new(now, bitrate_bps, max_bps),
            capacity: CapacityEstimator::new(now),
            capacity_tick: None,
            events: V4Events::default(),
            timestamps: TimestampExtender::default(),
            frame_size: (0, 0),
            frame_interval: DEFAULT_FRAME_INTERVAL,
            stats: V4Stats::default(),
            audio: AudioLane::default(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(LINK_QUEUE_CAPACITY);
        let thread_shared = shared.clone();
        let thread_stop = stop.clone();
        let thread_activity = activity.clone();
        let slot =
            claim_worker_slot(&LINK_WORKERS, MAX_LINK_WORKERS).context("spawn v4 media link")?;
        let (exit_tx, exit_rx) = mpsc::channel::<()>();
        let join = thread::Builder::new()
            .name("bubbleplay-wss-v4".into())
            .spawn(move || {
                // Dropped after `link_loop` returns, even on a panic: the slot
                // is freed first, then the exit signal disconnects.
                let _exit = exit_tx;
                let _slot = slot;
                link_loop(
                    connector,
                    &epoch,
                    rx,
                    thread_shared,
                    thread_stop,
                    thread_activity,
                )
            })
            .context("spawn v4 media link")?;
        Ok(Self {
            shared,
            tx,
            stop,
            activity,
            worker: Mutex::new(Some((join, exit_rx))),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
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
        let window = inflight_window(shared.rate.min_rtt_ms(), shared.frame_interval);
        shared.policy.set_window(window);
        match shared.policy.admit(key, bitrate) {
            Admission::Send => V4Reserve::Send,
            Admission::RequireKeyframe => V4Reserve::RequireKeyframe,
            Admission::Backpressured => {
                shared.stats.skipped_budget += 1;
                // A keyframe in flight legitimately exceeds the airtime
                // budget; only count skips behind delta frames as congestion.
                if !shared.policy.key_in_flight() {
                    let decision = shared.rate.on_skip(now);
                    record_rate_decision(&mut shared, decision);
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
        if self.stop.load(Ordering::Acquire) {
            return V4Submit::Stopped;
        }
        let tx = &self.tx;
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
        let peer_epoch = shared.peer_epoch;
        match tx.try_send(Outgoing { peer_epoch, packet }) {
            Ok(()) => {
                self.activity.notify();
                shared.policy.on_sent(now, generation, seq, bytes, key);
                shared.capacity.on_sent(now, generation, seq, bytes);
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

    /// Opt the audio lane in or out. Off (the default) keeps no audio and sends
    /// no BPA1 packets. Turning it off drops the backlog.
    pub fn set_audio_enabled(&self, enabled: bool) {
        self.lock().audio.set_enabled(enabled);
        self.activity.notify();
    }

    /// Queue one audio frame for the socket writer, which sends it ahead of any
    /// video. Never touches the video queue, the in-flight budget or keyframe
    /// state, and never requests a keyframe.
    pub fn submit_audio(&self, frame: AudioFrame) -> V4AudioSubmit {
        if self.stop.load(Ordering::Acquire) {
            return V4AudioSubmit::Stopped;
        }
        let outcome = self.lock().queue_audio(Instant::now(), frame);
        if outcome == V4AudioSubmit::Queued {
            self.activity.notify();
        }
        outcome
    }

    /// Wake the producer whenever ACK, keyframe, peer or rate state changes.
    pub fn set_activity(&self, activity: Activity) {
        self.lock().producer_activity = Some(activity.clone());
        activity.notify();
    }

    pub fn take_events(&self) -> V4Events {
        std::mem::take(&mut self.lock().events)
    }

    /// The host moved off this link's epoch: the next relay close ends the
    /// link for good instead of reconnecting.
    pub fn retire_epoch(&self) {
        self.lock().epoch_active = false;
        self.activity.notify();
    }

    pub fn request_keyframe(&self) {
        self.lock().policy.request_keyframe();
    }

    pub fn bitrate_bps(&self) -> u64 {
        self.lock().rate.bitrate_bps()
    }

    /// Measured capacity of a constrained link; `None` when unconstrained,
    /// including a measured link whose 80 % target already covers the
    /// profile's maximum bitrate (nothing to budget).
    pub fn capacity(&self) -> Option<Capacity> {
        let state = self.lock();
        binding_capacity(state.capacity.capacity(), state.rate.max_bps())
    }

    pub fn set_bitrate_bounds(&self, bitrate_bps: u64, max_bps: u64) {
        self.lock()
            .rate
            .set_bounds(Instant::now(), bitrate_bps, max_bps);
        self.activity.notify();
    }

    /// Stream frame rate, which scales the in-flight frame cap with RTT.
    pub fn set_frame_rate(&self, fps: u32) {
        self.lock().frame_interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
    }

    pub fn set_frame_size(&self, width: u32, height: u32) {
        let mut shared = self.lock();
        shared.rate.set_frame_pixels(width, height);
        shared.frame_size = (
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
            audio_frames_dropped: shared.audio.dropped,
            audio_suspended: shared.audio.stalled,
            bitrate_bps: shared.rate.bitrate_bps(),
            min_rtt_ms: shared.rate.min_rtt_ms().unwrap_or(0.0),
            capacity_bps: shared
                .capacity
                .capacity()
                .map_or(0, |capacity| capacity.bps),
            rate_cap_bps: shared
                .capacity
                .rate_cap()
                .map_or(0, |capacity| capacity.bps),
            ..shared.stats
        }
    }

    pub fn shutdown(&self) {
        {
            // Set under the state lock: a connect that finishes after this
            // point sees `stop` before it publishes anything.
            let _state = self.lock();
            self.stop.store(true, Ordering::Release);
        }
        self.activity.notify();
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        let Some((_join, exited)) = worker else {
            return;
        };
        match exited.recv_timeout(LINK_SHUTDOWN_WAIT) {
            // The signal precedes thread-local cleanup. Even after it arrives,
            // joining could wait indefinitely for a destructor on that thread.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => {
                // Dropping the handle detaches the worker. It keeps its own
                // socket, queue and state, and its budget slot, until it ends.
                warn!(
                    "WSS v4 media worker did not exit within 400 ms; detaching with its resources"
                );
            }
        }
    }
}

impl Drop for V4Link {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A capacity only binds (budget, pacing, QP ceiling) when its 80 % target is
/// below the most the encoder would send anyway.
fn binding_capacity(capacity: Option<Capacity>, max_bps: u64) -> Option<Capacity> {
    capacity.filter(|capacity| capacity.target_bps() < max_bps)
}

/// Advance the capacity estimate and let the bitrate follow it: capped by a
/// constrained capacity, or by a released one while it is probed up (dropped
/// once its target reaches the profile maximum).
fn apply_capacity(shared: &mut Shared, now: Instant) {
    shared.capacity.advance(now);
    let max_bps = shared.rate.max_bps();
    if !shared.capacity.is_constrained()
        && shared
            .capacity
            .rate_cap()
            .is_some_and(|cap| cap.target_bps() >= max_bps)
    {
        shared.capacity.release();
    }
    if let Some(previous) = shared.capacity_tick {
        if binding_capacity(shared.capacity.capacity(), max_bps).is_some() {
            let elapsed = now
                .saturating_duration_since(previous)
                .min(Duration::from_secs(1));
            shared.stats.constrained_ms += elapsed.as_millis() as u64;
        }
    }
    shared.capacity_tick = Some(now);
    let target = shared.capacity.rate_cap().map(Capacity::target_bps);
    let decision = shared.rate.set_capacity_target(now, target);
    record_rate_decision(shared, decision);
}

/// Publish a controller decision to the producer and log it with its reason.
fn record_rate_decision(shared: &mut Shared, decision: Option<RateDecision>) {
    let Some(decision) = decision else {
        return;
    };
    // Probing up and small capacity tracking happen every second or so (27k info lines in two
    // days): only a real signal or a collapse stays at info.
    macro_rules! log_decision {
        ($level:ident) => {
            $level!(
                bitrate_bps = decision.bitrate_bps,
                previous_bps = decision.previous_bps,
                reason = decision.reason.as_str(),
                capacity_bps = shared
                    .capacity
                    .capacity()
                    .map_or(0, |capacity| capacity.bps),
                rate_cap_bps = shared
                    .capacity
                    .rate_cap()
                    .map_or(0, |capacity| capacity.bps),
                last_rtt_ms = shared.stats.last_rtt_ms,
                min_rtt_ms = shared.rate.min_rtt_ms().unwrap_or(0.0),
                floor_bps = shared.rate.floor_bps(),
                "WSS v4 rate decision"
            );
        };
    }
    if decision.is_notable() {
        log_decision!(info);
    } else {
        log_decision!(debug);
    }
    shared.events.bitrate_bps = Some(decision.bitrate_bps);
    shared.events.bitrate_reason = Some(decision.reason.as_str());
    if let Some(activity) = &shared.producer_activity {
        activity.notify();
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

fn sleep_unless_stopped(stop: &AtomicBool, activity: &Activity, delay: Duration) {
    let deadline = Instant::now() + delay;
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        if activity
            .wait(deadline.saturating_duration_since(Instant::now()), &[])
            .is_err()
        {
            break;
        }
    }
}

fn tcp_socket(ws: &WebSocket<MaybeTlsStream<TcpStream>>) -> Result<RawSocket> {
    Ok(match ws.get_ref() {
        MaybeTlsStream::Plain(stream) => stream.as_raw_socket(),
        MaybeTlsStream::NativeTls(stream) => stream.get_ref().as_raw_socket(),
        other => bail!("unsupported TLS stream variant: {other:?}"),
    })
}

fn link_loop(
    connector: Connector,
    epoch: &str,
    rx: Receiver<Outgoing>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    activity: Activity,
) {
    // Keep high resolution for controller and socket deadlines during media.
    let _timer = crate::rtc::RtcTimerPeriod::acquire();
    let lock = || shared.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut backoff = Backoff::default();
    let mut first = true;
    // Consecutive relay closes before the relay accepted the media hello.
    let mut refusals = 0u32;
    // Relay closes already logged at WARN for this link (and so this epoch).
    let mut logged_closes: HashSet<(u16, String)> = HashSet::new();
    while !stop.load(Ordering::Acquire) {
        let mut ws = match connector().and_then(|mut ws| {
            set_nonblocking(&mut ws)?;
            Ok(ws)
        }) {
            Ok(ws) => ws,
            Err(error) => {
                let delay = backoff.next_delay();
                warn!(error = %format!("{error:#}"), ?delay, "WSS v4 media connect failed");
                sleep_unless_stopped(&stop, &activity, delay);
                continue;
            }
        };
        // The backoff resets only once the relay accepts the hello: a socket
        // refused right after connect must not retry at the minimum delay.
        while rx.try_recv().is_ok() {}
        {
            let mut state = lock();
            // A connect that finished after shutdown is dropped unpublished.
            if stop.load(Ordering::Acquire) {
                break;
            }
            let now = Instant::now();
            state.peer_epoch = state.peer_epoch.wrapping_add(1);
            state.connected = true;
            state.accepted = false;
            // Presence is announced by the relay right after hello.
            state.peer_present = false;
            state.policy.reset(now);
            state.capacity.forget_inflight();
            state.audio.link_reset();
            if !first {
                state.stats.reconnects += 1;
            }
        }
        first = false;
        info!("WSS v4 media socket connected");
        let result = serve_socket(&mut ws, &rx, &shared, &stop, &activity);
        let (accepted, epoch_active) = {
            let mut state = lock();
            state.connected = false;
            state.peer_present = false;
            state.policy.reset(Instant::now());
            state.capacity.forget_inflight();
            state.audio.link_reset();
            if let Some(activity) = &state.producer_activity {
                activity.notify();
            }
            (state.accepted, state.epoch_active)
        };
        let _ = ws.close(None);
        let _ = ws.flush();
        if accepted {
            backoff.reset();
            refusals = 0;
        }
        let Err(error) = result else {
            continue;
        };
        let close = error.downcast_ref::<RelayClose>().cloned();
        if close.is_some() && !accepted {
            refusals += 1;
        }
        let verdict = close_verdict(close.as_ref(), accepted, epoch_active, refusals);
        if let Some(close) = &close {
            if logged_closes.insert((close.code, close.reason.clone())) {
                if close_is_expected(close, epoch_active) {
                    info!(
                        close_status = close.code,
                        reason = %close.reason,
                        epoch,
                        accepted,
                        epoch_active,
                        refusals,
                        "WSS v4 media socket closed by relay"
                    );
                } else {
                    warn!(
                        close_status = close.code,
                        reason = %close.reason,
                        epoch,
                        accepted,
                        epoch_active,
                        refusals,
                        "WSS v4 media socket closed by relay"
                    );
                }
            }
        }
        let end = match (verdict, close.clone()) {
            (CloseVerdict::Retry, _) | (_, None) => None,
            (CloseVerdict::Retired, Some(close)) => {
                if close_is_expected(&close, epoch_active) {
                    info!(
                        close_status = close.code,
                        epoch, "WSS v4 media epoch retired by relay; link stopped"
                    );
                } else {
                    warn!(
                        close_status = close.code,
                        epoch, "WSS v4 media epoch retired by relay; link stopped"
                    );
                }
                Some(V4LinkEnd::Retired {
                    code: close.code,
                    reason: close.reason,
                })
            }
            (CloseVerdict::Refused, Some(close)) => {
                warn!(
                    close_status = close.code,
                    epoch,
                    refusals,
                    "relay refused the WSS v4 media hello repeatedly; link stopped"
                );
                Some(V4LinkEnd::Refused {
                    code: close.code,
                    reason: close.reason,
                    refusals,
                })
            }
        };
        if let Some(end) = end {
            let mut state = lock();
            state.events.link_end = Some(end);
            if let Some(activity) = &state.producer_activity {
                activity.notify();
            }
            break;
        }
        let delay = backoff.next_delay();
        if close.is_some() {
            debug!(error = %format!("{error:#}"), ?delay, "WSS v4 media socket closed by relay; reconnecting");
        } else {
            warn!(error = %format!("{error:#}"), ?delay, "WSS v4 media socket lost; reconnecting");
        }
        sleep_unless_stopped(&stop, &activity, delay);
    }
}

fn flush_blocking(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
    activity: &Activity,
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
                activity.wait(
                    WRITE_STALL_LIMIT.saturating_sub(started.elapsed()),
                    &[(tcp_socket(ws)?, true)],
                )?;
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
    activity: &Activity,
) -> Result<()> {
    match ws.write(message) {
        Ok(()) => {}
        Err(error) if would_block(&error) => {}
        Err(error) => return Err(error).context("write v4 media socket"),
    }
    flush_blocking(ws, shared, stop, activity)
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
            Ok(Message::Close(frame)) => {
                let close = frame.map_or_else(
                    || RelayClose {
                        code: 1005,
                        reason: String::new(),
                    },
                    |frame| RelayClose {
                        code: u16::from(frame.code),
                        reason: frame.reason.as_str().to_owned(),
                    },
                );
                return Err(close.into());
            }
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
            generation,
            seq,
            recv_ts_ms,
            ..
        } => {
            state.accepted = true;
            state.stats.acks += 1;
            state.capacity.note_recv_ts(recv_ts_ms);
            state.capacity.on_ack(now, generation, seq);
            apply_capacity(&mut state, now);
            if let Some(outcome) = retire_video(&mut state, now, generation, seq) {
                let rtt_ms = outcome.rtt.as_secs_f64() * 1_000.0;
                state.stats.last_rtt_ms = rtt_ms;
                // Keyframes carry their own serialization delay; only deltas
                // are representative delay samples.
                if !outcome.key {
                    let decision = state.rate.on_rtt(now, rtt_ms);
                    record_rate_decision(&mut state, decision);
                }
            }
            let newer = state
                .events
                .latest_ack
                .is_none_or(|(ack_generation, ack_seq)| {
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
            state.accepted = true;
            state.peer_epoch = state.peer_epoch.wrapping_add(1);
            state.events = V4Events::default();
            state.peer_present = present;
            state.policy.reset(now);
            state.capacity.forget_inflight();
            state.audio.link_reset();
            if present {
                state.events.keyframe = Some("peer-joined".into());
            }
        }
        V4Inbound::HelloOk => state.accepted = true,
        V4Inbound::Ping | V4Inbound::Pong => {}
    }
    if let Some(activity) = &state.producer_activity {
        activity.notify();
    }
}

/// What one audio pass wrote, and whether it stopped at its bound.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct AudioPass {
    sent: bool,
    /// The pass used its whole bound, so more audio may be ready. The serve
    /// loop comes back for it without waiting, after control and video.
    more: bool,
}

/// Write up to one queue's worth of audio packets, ahead of video. The bound
/// keeps a continuously refilled audio producer from starving queued video
/// and control/housekeeping on this writer. Audio waits only on the stall
/// rule and the opt-in; it takes no video admission.
fn send_audio(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
    activity: &Activity,
) -> Result<AudioPass> {
    let mut pass = AudioPass::default();
    for _ in 0..AUDIO_QUEUE_FRAMES {
        if stop.load(Ordering::Acquire) {
            return Ok(pass);
        }
        let next = shared
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .next_audio(Instant::now());
        let Some(frame) = next else {
            return Ok(pass);
        };
        let packet = match crate::audio::encode_packet(&frame) {
            Ok(packet) => packet,
            Err(error) => {
                debug!(error = %format!("{error:#}"), "dropped unencodable audio frame");
                shared
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .audio
                    .mark_gap(1);
                continue;
            }
        };
        let bytes = packet.len() as u64;
        send_message(ws, Message::binary(packet), shared, stop, activity)?;
        let mut state = shared.lock().unwrap_or_else(|poison| poison.into_inner());
        state.stats.audio_frames_sent += 1;
        state.stats.audio_bytes_sent += bytes;
        pass.sent = true;
    }
    pass.more = true;
    Ok(pass)
}

fn serve_socket(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    rx: &Receiver<Outgoing>,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
    activity: &Activity,
) -> Result<()> {
    // Pings run on their own clock: video and audio sends must not reset it, or a busy sender never
    // pings and a still screen (no inbound) trips the silence limit.
    let mut last_ping = Instant::now();
    let mut last_inbound = Instant::now();
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        // Drain control first so queued AUs from a departed peer cannot
        // escape into the next peer's decoder chain.
        if read_available(ws, shared)? {
            last_inbound = Instant::now();
        }
        // Audio goes ahead of video on this writer, bounded per pass; it has
        // its own queue.
        let audio = send_audio(ws, shared, stop, activity)?;
        if ping_due(Instant::now(), last_ping) {
            send_ping(ws, shared, stop, activity)?;
            last_ping = Instant::now();
        }
        match rx.try_recv() {
            Ok(outgoing) => {
                // Re-check after the loop-top check: a stop that lands in
                // between must not reach the wire.
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                let state = shared.lock().unwrap_or_else(|poison| poison.into_inner());
                let current = outgoing_is_current(&state, &outgoing);
                drop(state);
                if current {
                    send_message(ws, Message::binary(outgoing.packet), shared, stop, activity)?;
                }
                continue;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
        }
        let now = Instant::now();
        if now.saturating_duration_since(last_inbound) >= INBOUND_SILENCE_LIMIT {
            let heartbeats = shared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .stats
                .heartbeats;
            bail!(
                "v4 media socket silent (no inbound for {} s, {heartbeats} heartbeats sent, last ping {} s ago)",
                now.saturating_duration_since(last_inbound).as_secs(),
                now.saturating_duration_since(last_ping).as_secs()
            );
        }
        let mut state = shared.lock().unwrap_or_else(|poison| poison.into_inner());
        // Latch the audio stall before expiry can forget the stalled frames.
        state.observe_audio(now);
        if state.policy.expire(now) {
            state.stats.ack_timeouts += 1;
            state.capacity.forget_inflight();
        }
        if state.peer_present {
            apply_capacity(&mut state, now);
            let decision = state.rate.tick(now);
            record_rate_decision(&mut state, decision);
        }
        let deadline = link_deadline(&state, now, last_ping, last_inbound);
        drop(state);
        // A full audio pass may have left packets ready: don't wait on them.
        if audio.more {
            continue;
        }
        activity.wait(
            deadline.saturating_duration_since(Instant::now()),
            &[(tcp_socket(ws)?, false)],
        )?;
    }
}

fn ping_due(now: Instant, last_ping: Instant) -> bool {
    now.saturating_duration_since(last_ping) >= HEARTBEAT_INTERVAL
}

fn send_ping(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
    activity: &Activity,
) -> Result<()> {
    send_message(
        ws,
        Message::text(r#"{"type":"ping"}"#),
        shared,
        stop,
        activity,
    )?;
    shared
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .stats
        .heartbeats += 1;
    Ok(())
}

fn link_deadline(
    state: &Shared,
    now: Instant,
    last_ping: Instant,
    last_inbound: Instant,
) -> Instant {
    let mut deadline = (last_ping + HEARTBEAT_INTERVAL).min(last_inbound + INBOUND_SILENCE_LIMIT);
    if state.policy.inflight_frames() != 0 {
        deadline = deadline.min(state.policy.last_progress + ACK_STALL_LIMIT);
    }
    if state.peer_present
        && state.rate.clean_samples != 0
        && state.rate.bitrate_bps() < state.rate.ceiling_bps()
    {
        deadline = deadline.min(
            (state.rate.clean_since + RATE_CLEAN_INTERVAL)
                .max(state.rate.last_increase + RATE_CLEAN_INTERVAL),
        );
    }
    // Capacity release/probing has its own aging timers. Only a measured cap
    // needs this bounded housekeeping; an idle unconstrained link sleeps.
    if state.peer_present && state.capacity.rate_cap().is_some() {
        deadline = deadline.min(now + Duration::from_millis(100));
    }
    deadline
}

fn outgoing_is_current(state: &Shared, outgoing: &Outgoing) -> bool {
    state.peer_present && state.peer_epoch == outgoing.peer_epoch
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    #[test]
    fn only_signals_and_collapses_are_notable_rate_decisions() {
        let decision = |previous_bps, bitrate_bps, reason| RateDecision {
            bitrate_bps,
            previous_bps,
            reason,
        };
        // Probing up and gentle capacity tracking are routine.
        assert!(!decision(4_800_000, 6_000_000, RateReason::Clean).is_notable());
        assert!(!decision(2_000_000, 2_500_000, RateReason::Capacity).is_notable());
        assert!(!decision(6_000_000, 5_000_000, RateReason::Capacity).is_notable());
        // A collapse onto a constrained link, delay growth and skipped frames are not.
        assert!(decision(6_000_000, 500_000, RateReason::Capacity).is_notable());
        assert!(decision(6_000_000, 4_800_000, RateReason::SustainedSkips).is_notable());
        assert!(decision(5_000_000, 4_900_000, RateReason::RttGrowth).is_notable());
    }

    fn idle_shared(now: Instant) -> Shared {
        Shared {
            producer_activity: None,
            peer_epoch: 0,
            connected: true,
            accepted: true,
            epoch_active: true,
            peer_present: false,
            policy: SendPolicy::new(now),
            rate: DelayRateController::new(now, 6_000_000, 6_000_000),
            capacity: CapacityEstimator::new(now),
            capacity_tick: None,
            events: V4Events::default(),
            timestamps: TimestampExtender::default(),
            frame_size: (0, 0),
            frame_interval: DEFAULT_FRAME_INTERVAL,
            stats: V4Stats::default(),
            audio: AudioLane::default(),
        }
    }

    fn audio_frame(seq: u32) -> AudioFrame {
        AudioFrame {
            seq,
            capture_ts_us: u64::from(seq) * 10_000,
            discontinuity: false,
            payload: vec![0xfc, 0xff, 0xfe],
        }
    }

    #[test]
    fn socket_writer_sends_opted_in_audio_ahead_of_queued_video() {
        use tungstenite::protocol::Role;

        for enabled in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let now = Instant::now();
            let mut state = idle_shared(now);
            state.peer_present = true;
            state.audio.set_enabled(enabled);
            state.queue_audio(now, audio_frame(7));
            let shared = Arc::new(Mutex::new(state));
            let (tx, rx) = mpsc::sync_channel(1);
            let video = encode_packet(
                &V4FrameHeader {
                    key: true,
                    refine: false,
                    codec: 2,
                    generation: 1,
                    seq: 1,
                    capture_ts_us: 0,
                    width: 0,
                    height: 0,
                },
                &[1],
            )
            .unwrap();
            tx.send(Outgoing {
                peer_epoch: 0,
                packet: video,
            })
            .unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let activity = Activity::new().unwrap();
            let worker = {
                let shared = shared.clone();
                let stop = stop.clone();
                let activity = activity.clone();
                thread::spawn(move || {
                    let (stream, _) = listener.accept().unwrap();
                    let mut socket = WebSocket::from_raw_socket(
                        MaybeTlsStream::Plain(stream),
                        Role::Server,
                        None,
                    );
                    set_nonblocking(&mut socket).unwrap();
                    serve_socket(&mut socket, &rx, &shared, &stop, &activity)
                })
            };
            let stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut viewer = WebSocket::from_raw_socket(stream, Role::Client, None);
            let mut packets = Vec::new();
            for _ in 0..if enabled { 2 } else { 1 } {
                packets.push(viewer.read());
                if packets.last().unwrap().is_err() {
                    break;
                }
            }
            stop.store(true, Ordering::Release);
            activity.notify();
            worker.join().unwrap().unwrap();
            let packets: Vec<_> = packets.into_iter().map(Result::unwrap).collect();
            let magic: Vec<_> = packets
                .iter()
                .map(|message| {
                    let Message::Binary(bytes) = message else {
                        panic!("expected binary media")
                    };
                    bytes[..4].to_vec()
                })
                .collect();
            let expected = if enabled {
                vec![b"BPA1".to_vec(), b"BPV4".to_vec()]
            } else {
                vec![b"BPV4".to_vec()]
            };
            assert_eq!(magic, expected);
        }
    }

    #[test]
    fn audio_is_absent_without_opt_in() {
        let now = Instant::now();
        let mut shared = idle_shared(now);
        shared.peer_present = true;
        assert_eq!(
            shared.queue_audio(now, audio_frame(1)),
            V4AudioSubmit::Disabled
        );
        assert!(shared.audio.queue.is_empty());
        assert!(shared.next_audio(now).is_none());
        assert_eq!(shared.stats.audio_frames_sent, 0);

        shared.audio.set_enabled(true);
        assert_eq!(
            shared.queue_audio(now, audio_frame(2)),
            V4AudioSubmit::Queued
        );
        shared.audio.set_enabled(false);
        assert!(shared.audio.queue.is_empty());
        assert!(shared.next_audio(now).is_none());
    }

    #[test]
    fn audio_needs_a_live_opted_in_peer() {
        let now = Instant::now();
        let mut shared = idle_shared(now);
        shared.audio.set_enabled(true);
        // Peer not present yet: dropped, and the next sent packet is a gap.
        assert_eq!(
            shared.queue_audio(now, audio_frame(1)),
            V4AudioSubmit::Unavailable
        );
        assert!(shared.audio.queue.is_empty());
        shared.peer_present = true;
        shared.queue_audio(now, audio_frame(2));
        assert!(shared.next_audio(now).unwrap().discontinuity);
    }

    #[test]
    fn audio_never_enters_video_admission_or_requests_keyframes() {
        let now = Instant::now();
        let mut shared = idle_shared(now);
        shared.peer_present = true;
        shared.audio.set_enabled(true);
        // A keyframe, then a full window of deltas: over budget and frame cap.
        shared.policy.on_sent(now, 1, 1, 1_000, true);
        for seq in 2..=(1 + INFLIGHT_MAX_FRAMES as u32) {
            shared.policy.on_sent(now, 1, seq, 1_000, false);
        }
        let bitrate = shared.rate.bitrate_bps();
        assert_eq!(
            shared.policy.admit(false, bitrate),
            Admission::Backpressured
        );
        assert!(!shared.policy.needs_keyframe());

        for seq in 0..3 {
            assert_eq!(
                shared.queue_audio(now, audio_frame(seq)),
                V4AudioSubmit::Queued
            );
        }

        assert_eq!(shared.audio.queue.len(), 3);
        assert_eq!(shared.policy.inflight_frames(), INFLIGHT_MAX_FRAMES + 1);
        assert_eq!(
            shared.policy.admit(false, bitrate),
            Admission::Backpressured
        );
        assert!(!shared.policy.needs_keyframe());
        assert_eq!(shared.stats.skipped_budget, 0);
        assert_eq!(shared.stats.frames_sent, 0);
    }

    #[test]
    fn audio_queue_drops_oldest_and_marks_the_next_packet() {
        let now = Instant::now();
        let mut shared = idle_shared(now);
        shared.peer_present = true;
        shared.audio.set_enabled(true);
        for seq in 0..12 {
            assert_eq!(
                shared.queue_audio(now, audio_frame(seq)),
                V4AudioSubmit::Queued
            );
        }
        assert_eq!(shared.audio.queue.len(), AUDIO_QUEUE_FRAMES);
        assert_eq!(shared.audio.dropped, 2);
        let first = shared.next_audio(now).unwrap();
        assert_eq!(first.seq, 2);
        assert!(first.discontinuity);
        let second = shared.next_audio(now).unwrap();
        assert_eq!(second.seq, 3);
        assert!(!second.discontinuity);
    }

    #[test]
    fn audio_suspends_past_one_second_and_resumes_only_on_ack() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut shared = idle_shared(t0);
        shared.peer_present = true;
        shared.audio.set_enabled(true);
        shared.policy.on_sent(t0, 1, 5, 1_000, true);

        assert_eq!(
            shared.queue_audio(at(900), audio_frame(1)),
            V4AudioSubmit::Queued
        );
        // Oldest unacked video (gen 1, seq 5) is 1.1 s old: audio stalls, the
        // backlog goes, and the stalled frame becomes the fence.
        assert_eq!(
            shared.queue_audio(at(1_100), audio_frame(2)),
            V4AudioSubmit::Unavailable
        );
        assert!(shared.audio.stalled);
        assert_eq!(shared.audio.fence, Some((1, 5)));
        assert!(shared.audio.queue.is_empty());
        assert!(shared.next_audio(at(1_100)).is_none());

        // The 3 s window expiry forgets frame 5 but must not resume audio.
        shared.observe_audio(at(3_100));
        assert!(shared.policy.expire(at(3_100)));
        shared.observe_audio(at(3_100));
        assert!(shared.audio.stalled);
        assert_eq!(
            shared.queue_audio(at(3_100), audio_frame(3)),
            V4AudioSubmit::Unavailable
        );

        // The late ACK for the expired frame covers the fence: audio resumes.
        retire_video(&mut shared, at(3_150), 1, 5);
        assert!(!shared.audio.stalled);
        assert_eq!(shared.audio.fence, None);
        assert_eq!(
            shared.queue_audio(at(3_150), audio_frame(4)),
            V4AudioSubmit::Queued
        );
        let resumed = shared.next_audio(at(3_150)).unwrap();
        assert_eq!(resumed.seq, 4);
        assert!(resumed.discontinuity);
    }

    #[test]
    fn stale_acks_do_not_clear_the_audio_stall() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut shared = idle_shared(t0);
        shared.peer_present = true;
        shared.audio.set_enabled(true);
        shared.policy.on_sent(t0, 1, 5, 1_000, true);
        shared.observe_audio(at(1_100));
        assert!(shared.audio.stalled);
        shared.policy.expire(at(3_100));
        shared.observe_audio(at(3_100));

        // Another generation, older or newer, never covers the fence.
        retire_video(&mut shared, at(3_110), 0, 5);
        assert!(shared.audio.stalled);
        retire_video(&mut shared, at(3_120), 2, 5);
        assert!(shared.audio.stalled);
        // Same generation but behind the fence: frame 5 was never ACKed.
        retire_video(&mut shared, at(3_130), 1, 4);
        assert!(shared.audio.stalled);
        retire_video(&mut shared, at(3_140), 1, 0);
        assert!(shared.audio.stalled);
        // Wrapped-behind seq (u32::MAX is 6 behind seq 5) is not a cover either.
        retire_video(&mut shared, at(3_145), 1, u32::MAX);
        assert!(shared.audio.stalled);
        assert_eq!(shared.audio.fence, Some((1, 5)));
        assert_eq!(
            shared.queue_audio(at(3_150), audio_frame(6)),
            V4AudioSubmit::Unavailable
        );
        // Stale ACKs are not video progress either: video policy is untouched.
        assert_eq!(shared.policy.inflight_frames(), 0);
        assert!(!shared.policy.needs_keyframe());

        // The matching ACK for the fence frame clears the stall.
        retire_video(&mut shared, at(3_160), 1, 5);
        assert!(!shared.audio.stalled);
        assert_eq!(shared.audio.fence, None);
    }

    #[test]
    fn video_progress_after_expiry_resumes_audio() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut shared = idle_shared(t0);
        shared.peer_present = true;
        shared.audio.set_enabled(true);
        shared.policy.on_sent(t0, 1, 5, 1_000, true);
        shared.observe_audio(at(1_100));
        assert!(shared.audio.stalled);
        shared.policy.expire(at(3_100));
        shared.observe_audio(at(3_100));

        // A fresh generation's frame is ACKed: retiring it is video progress.
        shared.policy.on_sent(at(3_200), 2, 1, 1_000, true);
        retire_video(&mut shared, at(3_300), 2, 1);
        assert!(!shared.audio.stalled);
        assert_eq!(shared.audio.fence, None);
        assert_eq!(
            shared.queue_audio(at(3_300), audio_frame(7)),
            V4AudioSubmit::Queued
        );
        assert_eq!(shared.next_audio(at(3_300)).unwrap().seq, 7);
    }

    #[test]
    fn audio_stall_is_not_latched_by_young_video() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut shared = idle_shared(t0);
        shared.peer_present = true;
        shared.audio.set_enabled(true);
        shared.policy.on_sent(t0, 1, 1, 1_000, true);
        // Exactly one second is not over the limit.
        shared.observe_audio(at(1_000));
        assert!(!shared.audio.stalled);
        assert_eq!(
            shared.queue_audio(at(1_000), audio_frame(1)),
            V4AudioSubmit::Queued
        );
        assert!(shared.next_audio(at(1_000)).is_some());
    }

    #[test]
    fn new_socket_or_peer_clears_the_stall_and_backlog() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut shared = idle_shared(t0);
        shared.peer_present = true;
        shared.audio.set_enabled(true);
        shared.policy.on_sent(t0, 1, 1, 1_000, true);
        shared.observe_audio(at(1_500));
        assert!(shared.audio.stalled);

        // The stalled frame belonged to the old socket.
        shared.policy.reset(at(1_500));
        assert_eq!(shared.audio.fence, Some((1, 1)));
        shared.audio.link_reset();
        assert!(!shared.audio.stalled);
        assert_eq!(shared.audio.fence, None);
        assert_eq!(
            shared.queue_audio(at(1_500), audio_frame(2)),
            V4AudioSubmit::Queued
        );
        assert!(shared.next_audio(at(1_500)).unwrap().discontinuity);
    }

    #[test]
    fn ping_is_due_on_its_own_clock_despite_steady_outbound_traffic() {
        // The writer no longer refreshes the ping clock on media sends: with a send every second and
        // no inbound, a ping still falls due every HEARTBEAT_INTERVAL and a pong keeps the link alive.
        let t0 = Instant::now();
        let mut last_ping = t0;
        let mut last_inbound = t0;
        let mut pings = 0;
        for sec in 1..=120u64 {
            let now = t0 + Duration::from_secs(sec);
            if ping_due(now, last_ping) {
                pings += 1;
                last_ping = now;
                last_inbound = now; // pong
            }
            assert!(now.saturating_duration_since(last_inbound) < INBOUND_SILENCE_LIMIT);
        }
        assert_eq!(pings, 8);
    }

    #[test]
    fn idle_link_waits_for_heartbeat_and_inflight_waits_for_ack_deadline() {
        let now = Instant::now();
        let mut shared = idle_shared(now);
        assert_eq!(
            link_deadline(&shared, now, now, now),
            now + HEARTBEAT_INTERVAL
        );
        shared.policy.on_sent(now, 1, 1, 1_000, true);
        assert_eq!(link_deadline(&shared, now, now, now), now + ACK_STALL_LIMIT);
    }

    #[test]
    fn peer_transition_invalidates_queued_media_and_stale_events() {
        let now = Instant::now();
        let shared = Mutex::new(idle_shared(now));
        handle_inbound(r#"{"type":"media-peer","present":true}"#, &shared);
        let queued = Outgoing {
            peer_epoch: shared.lock().unwrap().peer_epoch,
            packet: vec![1],
        };
        assert!(outgoing_is_current(&shared.lock().unwrap(), &queued));
        shared.lock().unwrap().events.latest_ack = Some((1, 1));
        handle_inbound(r#"{"type":"media-peer","present":false}"#, &shared);
        assert!(!outgoing_is_current(&shared.lock().unwrap(), &queued));
        handle_inbound(r#"{"type":"media-peer","present":true}"#, &shared);
        let state = shared.lock().unwrap();
        assert!(!outgoing_is_current(&state, &queued));
        assert!(state.policy.needs_keyframe());
        assert_eq!(state.events.latest_ack, None);
        assert_eq!(state.events.keyframe.as_deref(), Some("peer-joined"));
    }

    fn rect(left: u32, top: u32, right: u32, bottom: u32) -> DamageRect {
        DamageRect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn damage_area_is_measured_in_per_mille_of_the_frame() {
        assert_eq!(damage_per_mille(None, 1920, 1080), 1_000);
        assert_eq!(damage_per_mille(Some(&[]), 1920, 1080), 0);
        // A 20x20 cursor blink is far below the 2 % large-change share.
        assert_eq!(
            damage_per_mille(Some(&[rect(100, 100, 120, 120)]), 1920, 1080),
            0
        );
        // A quarter of the frame.
        assert_eq!(
            damage_per_mille(Some(&[rect(0, 0, 960, 540)]), 1920, 1080),
            250
        );
        // Rects are clipped to the frame and the total is capped.
        assert_eq!(
            damage_per_mille(
                Some(&[rect(0, 0, 4000, 4000), rect(0, 0, 1920, 1080)]),
                1920,
                1080
            ),
            1_000
        );
    }

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
        assert_eq!(
            contract["headerBytes"].as_u64().unwrap() as usize,
            V4_HEADER_BYTES
        );
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
            assert_eq!(
                payload,
                hex(case["payloadHex"].as_str().unwrap()).as_slice()
            );
            assert_eq!(encode_packet(&expected, payload).unwrap(), bytes);
        }
    }

    #[test]
    fn fixture_client_messages_match_relay_contract() {
        let contract = fixture();
        for case in contract["clientMessages"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            let parsed = parse_client_message(text);
            assert_eq!(
                parsed.is_some(),
                case["accepted"].as_bool().unwrap(),
                "{text}"
            );
            if let Some(kind) = case.get("kind").and_then(Value::as_str) {
                let matches = matches!(
                    (kind, parsed.unwrap()),
                    ("ack", V4Inbound::Ack { .. })
                        | ("keyframe", V4Inbound::Keyframe { .. })
                        | ("ping", V4Inbound::Ping)
                );
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
        assert_eq!(policy.admit(false, 6_000_000), Admission::RequireKeyframe);
        assert_eq!(policy.admit(true, 6_000_000), Admission::Send);
        policy.on_sent(now, 1, 1, 10_000, true);
        assert_eq!(policy.admit(false, 6_000_000), Admission::Send);
    }

    fn fps_interval(fps: f64) -> Duration {
        Duration::from_secs_f64(1.0 / fps)
    }

    #[test]
    fn inflight_window_without_rtt_sample_is_the_floor() {
        assert_eq!(
            inflight_window(None, fps_interval(60.0)),
            InflightWindow::FLOOR
        );
        assert_eq!(InflightWindow::FLOOR.max_frames, 4);
        assert_eq!(InflightWindow::FLOOR.budget, Duration::from_millis(150));
        assert_eq!(
            inflight_window(Some(f64::NAN), fps_interval(60.0)),
            InflightWindow::FLOOR
        );
    }

    #[test]
    fn inflight_window_low_rtt_keeps_the_floors() {
        // 30 fps: ceil(50 / 33.3) + 2 = 4, the floor; budget 130 ms -> 150 ms.
        let low = inflight_window(Some(10.0), fps_interval(30.0));
        assert_eq!(low.max_frames, INFLIGHT_MAX_FRAMES);
        assert_eq!(low.budget, INFLIGHT_BUDGET);
        // 60 fps: the formula itself gives 5 (ceil(50 / 16.7) + 2); budget floor holds.
        let low60 = inflight_window(Some(10.0), fps_interval(60.0));
        assert_eq!(low60.max_frames, 5);
        assert_eq!(low60.budget, INFLIGHT_BUDGET);
    }

    #[test]
    fn inflight_window_hundred_ms_rtt_at_sixty_fps() {
        // ceil(140 / 16.67) + 2 = 11 frames; 100 + 120 = 220 ms.
        let window = inflight_window(Some(100.0), fps_interval(60.0));
        assert_eq!(window.max_frames, 11);
        assert_eq!(window.budget, Duration::from_millis(220));
    }

    #[test]
    fn inflight_window_very_high_rtt_hits_the_caps() {
        let window = inflight_window(Some(500.0), fps_interval(60.0));
        assert_eq!(window.max_frames, INFLIGHT_MAX_FRAMES_CAP);
        assert_eq!(window.budget, INFLIGHT_BUDGET_CAP);
        assert_eq!(window.max_frames, 12);
        assert_eq!(window.budget, Duration::from_millis(400));
        assert!(LINK_QUEUE_CAPACITY >= window.max_frames + 2);
    }

    #[test]
    fn inflight_window_at_thirty_fps() {
        // ceil(140 / 33.3) + 2 = 7 frames; 220 ms.
        let window = inflight_window(Some(100.0), fps_interval(30.0));
        assert_eq!(window.max_frames, 7);
        assert_eq!(window.budget, Duration::from_millis(220));
    }

    #[test]
    fn policy_admits_up_to_the_scaled_frame_cap() {
        let now = Instant::now();
        let mut policy = SendPolicy::new(now);
        policy.set_window(inflight_window(Some(100.0), fps_interval(60.0)));
        for seq in 1..=11 {
            assert_eq!(policy.admit(seq == 1, 20_000_000), Admission::Send);
            policy.on_sent(now, 1, seq, 100, seq == 1);
        }
        assert_eq!(policy.inflight_frames(), 11);
        assert_eq!(policy.admit(false, 20_000_000), Admission::Backpressured);
    }

    #[test]
    fn policy_budget_bytes_and_frames() {
        let now = Instant::now();
        // 6 Mbps x 150 ms = 112_500 bytes.
        assert_eq!(SendPolicy::budget_bytes(6_000_000), 112_500);
        let mut policy = SendPolicy::new(now);
        policy.on_sent(now, 1, 1, 112_500, true);
        assert_eq!(policy.admit(false, 6_000_000), Admission::Send);
        policy.on_sent(now, 1, 2, 1, false);
        assert_eq!(policy.admit(false, 6_000_000), Admission::Backpressured);
        policy.on_ack(now, 1, 2);
        assert_eq!(policy.inflight_frames(), 0);
        assert_eq!(policy.inflight_bytes(), 0);
        for seq in 3..7 {
            assert_eq!(policy.admit(false, 6_000_000), Admission::Send);
            policy.on_sent(now, 1, seq, 100, false);
        }
        assert_eq!(policy.inflight_frames(), INFLIGHT_MAX_FRAMES);
        assert_eq!(policy.admit(false, 6_000_000), Admission::Backpressured);
        let outcome = policy
            .on_ack(now + Duration::from_millis(30), 1, 4)
            .unwrap();
        assert_eq!(outcome.rtt, Duration::from_millis(30));
        assert!(!outcome.key);
        assert_eq!(policy.inflight_frames(), 2);
        assert_eq!(policy.admit(false, 6_000_000), Admission::Send);
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
    fn policy_has_no_periodic_keyframe() {
        let start = Instant::now();
        let mut policy = SendPolicy::new(start);
        assert_eq!(policy.admit(false, 6_000_000), Admission::RequireKeyframe);
        policy.on_sent(start, 1, 1, 100, true);
        policy.on_ack(start, 1, 1);
        // Deltas keep flowing for an hour without another keyframe.
        for seq in 2..3_600u32 {
            assert_eq!(policy.admit(false, 6_000_000), Admission::Send);
            let at = start + Duration::from_secs(u64::from(seq));
            policy.on_sent(at, 1, seq, 100, false);
            policy.on_ack(at, 1, seq);
        }
        policy.request_keyframe();
        assert_eq!(policy.admit(false, 6_000_000), Admission::RequireKeyframe);
    }

    #[test]
    fn policy_stall_forgets_inflight_without_requiring_a_key() {
        let start = Instant::now();
        let mut policy = SendPolicy::new(start);
        policy.on_sent(start, 1, 1, 100, true);
        assert!(!policy.expire(start + Duration::from_secs(2)));
        assert!(policy.expire(start + ACK_STALL_LIMIT));
        assert!(!policy.needs_keyframe());
        assert_eq!(policy.inflight_frames(), 0);
        assert_eq!(policy.admit(false, 6_000_000), Admission::Send);
    }

    fn ms(start: Instant, value: u64) -> Instant {
        start + Duration::from_millis(value)
    }

    #[test]
    fn rate_reacts_only_to_sustained_rtt_growth() {
        let start = Instant::now();
        let mut rate = DelayRateController::new(start, 6_000_000, 6_000_000);
        assert_eq!(rate.on_rtt(start, 20.0), None);
        // Threshold: 2 x 20 + 60 = 100 ms. A single spike is ignored.
        assert_eq!(rate.on_rtt(ms(start, 10), 100.0), None);
        assert_eq!(rate.on_rtt(ms(start, 20), 150.0), None);
        assert_eq!(rate.on_rtt(ms(start, 40), 25.0), None);
        // Growth sustained for 500 ms cuts once, with its reason.
        assert_eq!(rate.on_rtt(ms(start, 100), 150.0), None);
        assert_eq!(rate.on_rtt(ms(start, 400), 150.0), None);
        let cut = rate.on_rtt(ms(start, 600), 150.0).unwrap();
        assert_eq!(cut.bitrate_bps, 4_800_000);
        assert_eq!(cut.previous_bps, 6_000_000);
        assert_eq!(cut.reason, RateReason::RttGrowth);
        // The next cut needs another full sustain window.
        assert_eq!(rate.on_rtt(ms(start, 900), 150.0), None);
        assert_eq!(
            rate.on_rtt(ms(start, 1_100), 150.0).map(|d| d.bitrate_bps),
            Some(3_840_000)
        );
    }

    #[test]
    fn rtt_baseline_is_the_minimum_over_ten_seconds() {
        let start = Instant::now();
        let mut rate = DelayRateController::new(start, 6_000_000, 6_000_000);
        rate.on_rtt(start, 20.0);
        rate.on_rtt(ms(start, 5_000), 80.0);
        assert_eq!(rate.min_rtt_ms(), Some(20.0));
        // Past the window the old minimum ages out and the route's new RTT
        // becomes the baseline (2 x 80 + 60 = 220 ms threshold).
        rate.on_rtt(ms(start, 10_500), 80.0);
        assert_eq!(rate.min_rtt_ms(), Some(80.0));
        assert_eq!(rate.on_rtt(ms(start, 10_600), 200.0), None);
        assert_eq!(rate.on_rtt(ms(start, 11_200), 200.0), None);
    }

    #[test]
    fn skips_come_before_bitrate_cuts() {
        let start = Instant::now();
        let mut rate = DelayRateController::new(start, 6_000_000, 6_000_000);
        rate.on_rtt(start, 20.0);
        // Budget skips around a burst do not cut the bitrate while ACKs stay
        // clean.
        assert_eq!(rate.on_skip(ms(start, 100)), None);
        assert_eq!(rate.on_rtt(ms(start, 300), 22.0), None);
        assert_eq!(rate.on_skip(ms(start, 700)), None);
        assert_eq!(rate.bitrate_bps(), 6_000_000);
        // Skipping for 500 ms with no clean ACK does.
        let cut = rate.on_skip(ms(start, 1_200)).unwrap();
        assert_eq!(cut.reason, RateReason::SustainedSkips);
        assert_eq!(cut.bitrate_bps, 4_800_000);
    }

    #[test]
    fn rate_floor_is_two_and_a_half_megabits_at_1080p_scaled_by_pixels() {
        assert_eq!(rate_floor_for_pixels(1920, 1080), 2_500_000);
        assert_eq!(rate_floor_for_pixels(3840, 2160), 10_000_000);
        assert_eq!(rate_floor_for_pixels(1280, 720), 1_111_111);
        assert_eq!(rate_floor_for_pixels(320, 180), RATE_FLOOR_MIN_BPS);
        let start = Instant::now();
        let mut rate = DelayRateController::new(start, 3_000_000, 6_000_000);
        rate.set_frame_pixels(1920, 1080);
        let mut at = start;
        rate.on_rtt(at, 20.0);
        for _ in 0..20 {
            at += Duration::from_millis(600);
            rate.on_rtt(at, 500.0);
        }
        assert_eq!(rate.bitrate_bps(), 2_500_000);
        // A profile capped below the floor keeps its cap.
        rate.set_bounds(at, 1_500_000, 1_500_000);
        assert_eq!(rate.floor_bps(), 1_500_000);
        assert_eq!(rate.bitrate_bps(), 1_500_000);
    }

    #[test]
    fn rate_steps_up_after_one_clean_second_capped() {
        let start = Instant::now();
        let mut rate = DelayRateController::new(start, 4_000_000, 6_000_000);
        // No ACK traffic: no probing upward.
        assert_eq!(rate.tick(ms(start, 3_000)), None);
        rate.on_rtt(ms(start, 3_000), 20.0);
        let up = rate.tick(ms(start, 3_100)).unwrap();
        assert_eq!((up.bitrate_bps, up.reason), (4_600_000, RateReason::Clean));
        rate.on_rtt(ms(start, 3_500), 20.0);
        assert_eq!(rate.tick(ms(start, 4_000)), None);
        assert_eq!(
            rate.tick(ms(start, 4_100)).map(|d| d.bitrate_bps),
            Some(5_290_000)
        );
        rate.on_rtt(ms(start, 4_500), 20.0);
        assert_eq!(
            rate.tick(ms(start, 5_100)).map(|d| d.bitrate_bps),
            Some(6_000_000)
        );
        rate.on_rtt(ms(start, 5_500), 20.0);
        assert_eq!(rate.tick(ms(start, 9_000)), None);
        // A skip restarts the clean window.
        rate.set_bounds(ms(start, 9_000), 5_000_000, 6_000_000);
        rate.on_skip(ms(start, 10_000));
        rate.on_rtt(ms(start, 10_500), 20.0);
        assert_eq!(rate.tick(ms(start, 10_900)), None);
        assert_eq!(
            rate.tick(ms(start, 11_000)).map(|d| d.bitrate_bps),
            Some(5_750_000)
        );
    }

    #[test]
    fn rate_follows_a_capacity_target_below_the_floor() {
        let start = Instant::now();
        let mut rate = DelayRateController::new(start, 6_000_000, 6_000_000);
        // A measured 2 Mbps link: 80 % = 1.6 Mbps, under the 2.5 Mbps floor.
        let follow = rate.set_capacity_target(start, Some(1_600_000)).unwrap();
        assert_eq!(
            (follow.bitrate_bps, follow.reason),
            (1_600_000, RateReason::Capacity)
        );
        assert_eq!(rate.floor_bps(), 1_600_000);
        // Clean seconds do not step above the target.
        rate.on_rtt(ms(start, 1_500), 20.0);
        assert_eq!(rate.tick(ms(start, 2_000)), None);
        // A move inside the deadband is ignored; a larger one is followed.
        assert_eq!(
            rate.set_capacity_target(ms(start, 2_000), Some(1_650_000)),
            None
        );
        let up = rate
            .set_capacity_target(ms(start, 2_000), Some(2_000_000))
            .unwrap();
        assert_eq!(up.bitrate_bps, 2_000_000);
        // Never above the profile maximum.
        let capped = rate
            .set_capacity_target(ms(start, 3_000), Some(20_000_000))
            .unwrap();
        assert_eq!(capped.bitrate_bps, 6_000_000);
        assert_eq!(rate.ceiling_bps(), 6_000_000);
    }

    #[test]
    fn rate_without_a_capacity_target_is_unchanged() {
        let start = Instant::now();
        let mut rate = DelayRateController::new(start, 3_000_000, 6_000_000);
        assert_eq!(rate.set_capacity_target(start, None), None);
        assert_eq!(rate.bitrate_bps(), 3_000_000);
        assert_eq!(rate.floor_bps(), RATE_FLOOR_1080P_BPS);
        assert_eq!(rate.ceiling_bps(), 6_000_000);
    }

    #[test]
    fn capacity_above_the_profile_maximum_does_not_bind() {
        let fast = Some(Capacity { bps: 7_500_000 });
        let slow = Some(Capacity { bps: 2_000_000 });
        // 80 % of 7.5 Mbps = 6 Mbps: the encoder never sends more anyway.
        assert_eq!(binding_capacity(fast, 6_000_000), None);
        assert_eq!(binding_capacity(slow, 6_000_000), slow);
        assert_eq!(binding_capacity(None, 6_000_000), None);
    }

    #[test]
    fn rate_bounds_clamp() {
        let start = Instant::now();
        let mut rate = DelayRateController::new(start, 500_000, 800_000);
        assert_eq!(rate.bitrate_bps(), 800_000);
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

    fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(value) = probe() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn next_peer_uses_the_existing_socket_and_wakes_from_idle() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (resume_tx, resume_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            for round in 0..2 {
                if round == 1 {
                    resume_rx.recv().unwrap();
                }
                ws.send(Message::text(r#"{"type":"media-peer","present":true}"#))
                    .unwrap();
                let packet = loop {
                    if let Message::Binary(bytes) = ws.read().unwrap() {
                        break bytes;
                    }
                };
                let (header, _) = decode_packet(&packet).unwrap();
                assert_eq!(header.generation, round + 1);
                ws.send(Message::text(format!(
                    r#"{{"type":"ack","generation":{},"seq":{},"recvTsMs":1}}"#,
                    header.generation, header.seq
                )))
                .unwrap();
                if round == 0 {
                    ws.send(Message::text(r#"{"type":"media-peer","present":false}"#))
                        .unwrap();
                }
            }
            // Leave the connection alive until the host shuts down.
            while !matches!(ws.read(), Ok(Message::Close(_)) | Err(_)) {}
        });
        let connector: Connector = Box::new(move || Ok(tungstenite::connect(url.as_str())?.0));
        let link = V4Link::spawn(connector, 6_000_000, 6_000_000).unwrap();
        let activity = Activity::new().unwrap();
        link.set_activity(activity.clone());
        for round in 0..2 {
            if round == 1 {
                wait_for(|| (!link.stats().peer_present).then_some(()));
                assert_eq!(link.reserve(true), V4Reserve::Backpressured);
                resume_tx.send(()).unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if link.take_events().keyframe.as_deref() == Some("peer-joined") {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "peer notice did not wake idle link"
                );
                activity
                    .wait(deadline.saturating_duration_since(Instant::now()), &[])
                    .unwrap();
            }
            assert_eq!(
                link.submit(1, round + 1, 1, 90_000, true, false, &[0, 0, 1, 0x40]),
                V4Submit::Queued
            );
            if round == 1 {
                wait_for(|| (link.stats().acks >= 2).then_some(()));
            }
        }
        assert_eq!(link.stats().reconnects, 0);
        link.shutdown();
        server.join().unwrap();
    }

    /// End-to-end link over a loopback relay stand-in: peer notice ->
    /// keyframe gate -> binary frame -> ACK on the same socket -> socket
    /// drop -> automatic reconnect.
    #[test]
    fn link_sends_frames_reads_acks_and_reconnects() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut sessions = Vec::new();
            for round in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let mut ws = tungstenite::accept(stream).unwrap();
                ws.send(Message::text(r#"{"type":"media-peer","present":true}"#))
                    .unwrap();
                let packet = loop {
                    match ws.read().unwrap() {
                        Message::Binary(bytes) => break bytes.to_vec(),
                        _ => continue,
                    }
                };
                let (header, payload) = decode_packet(&packet).unwrap();
                sessions.push((header, payload.to_vec()));
                ws.send(Message::text(format!(
                    r#"{{"type":"ack","generation":{},"seq":{},"recvTsMs":1}}"#,
                    header.generation, header.seq
                )))
                .unwrap();
                if round == 0 {
                    // Wait for the host to observe the ACK, then drop.
                    thread::sleep(Duration::from_millis(100));
                }
            }
            sessions
        });
        let connector: Connector = Box::new(move || {
            let (ws, _) = tungstenite::connect(url.as_str())?;
            Ok(ws)
        });
        let link = V4Link::spawn(connector, 6_000_000, 6_000_000).unwrap();
        link.set_frame_size(1920, 1080);
        for round in 0..2u32 {
            let events = wait_for(|| {
                let events = link.take_events();
                events.keyframe.is_some().then_some(events)
            });
            assert_eq!(events.keyframe.as_deref(), Some("peer-joined"));
            assert_eq!(link.reserve(false), V4Reserve::RequireKeyframe);
            assert_eq!(link.reserve(true), V4Reserve::Send);
            assert_eq!(
                link.submit(1, round + 1, 10, 90_000, true, false, &[0, 0, 1, 0x40]),
                V4Submit::Queued
            );
            let ack = wait_for(|| link.take_events().latest_ack);
            assert_eq!(ack, (round + 1, 10));
            assert_eq!(link.stats().inflight_frames, 0);
        }
        let sessions = server.join().unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions[0].0.key);
        assert_eq!(sessions[0].0.width, 1920);
        assert_eq!(sessions[0].0.capture_ts_us, 1_000_000);
        assert_eq!(sessions[1].1, vec![0, 0, 1, 0x40]);
        assert_eq!(
            wait_for(|| {
                let stats = link.stats();
                (stats.reconnects >= 1).then_some(stats.reconnects)
            }),
            1
        );
        link.shutdown();
    }

    #[test]
    fn viewer_departed_and_stale_closes_are_expected() {
        let close = |code, reason: &str| RelayClose {
            code,
            reason: reason.to_owned(),
        };
        assert!(close_is_expected(&close(1001, "viewer departed"), true));
        assert!(close_is_expected(&close(1001, "stale epoch"), true));
        assert!(close_is_expected(&close(1008, ""), false));
        assert!(!close_is_expected(&close(1008, "bad hello"), true));
        assert!(!close_is_expected(&close(1011, ""), true));
    }

    #[test]
    fn close_verdict_ends_only_retired_epochs_and_repeated_refusals() {
        let close = |code, reason: &str| RelayClose {
            code,
            reason: reason.to_owned(),
        };
        // Network errors and closes after acceptance keep reconnecting.
        assert_eq!(close_verdict(None, false, true, 9), CloseVerdict::Retry);
        assert_eq!(
            close_verdict(Some(&close(1001, "")), true, true, 0),
            CloseVerdict::Retry
        );
        // Refusals of the active epoch retry until the limit.
        assert_eq!(
            close_verdict(Some(&close(1008, "")), false, true, 1),
            CloseVerdict::Retry
        );
        assert_eq!(
            close_verdict(Some(&close(1008, "")), false, true, MEDIA_REFUSAL_LIMIT - 1),
            CloseVerdict::Retry
        );
        assert_eq!(
            close_verdict(Some(&close(1008, "")), false, true, MEDIA_REFUSAL_LIMIT),
            CloseVerdict::Refused
        );
        // A stale or retired epoch never reconnects.
        assert_eq!(
            close_verdict(Some(&close(1001, "stale epoch")), true, true, 0),
            CloseVerdict::Retired
        );
        assert_eq!(
            close_verdict(Some(&close(1008, "")), false, false, 1),
            CloseVerdict::Retired
        );
        assert_eq!(
            close_verdict(Some(&close(1001, "")), true, false, 0),
            CloseVerdict::Retired
        );
    }

    /// A relay stand-in that answers every connect with `Close(code, reason)`
    /// before any hello-ok, holding the first one until `gate` fires. Returns
    /// the accept times once `done` is set.
    fn refusing_relay(
        code: u16,
        reason: &'static str,
        gate: Option<Receiver<()>>,
    ) -> (String, Arc<AtomicBool>, JoinHandle<Vec<Instant>>) {
        use tungstenite::protocol::frame::coding::CloseCode;
        use tungstenite::protocol::frame::CloseFrame;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let done = Arc::new(AtomicBool::new(false));
        let server_done = done.clone();
        let server = thread::spawn(move || {
            let mut accepts = Vec::new();
            while !server_done.load(Ordering::Acquire) {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("accept failed: {error}"),
                };
                accepts.push(Instant::now());
                stream.set_nonblocking(false).unwrap();
                let mut ws = tungstenite::accept(stream).unwrap();
                if accepts.len() == 1 {
                    if let Some(gate) = &gate {
                        gate.recv().unwrap();
                    }
                }
                ws.close(Some(CloseFrame {
                    code: CloseCode::from(code),
                    reason: reason.into(),
                }))
                .unwrap();
                // Finish the close handshake or see the host drop the socket.
                while ws.read().is_ok() {}
            }
            accepts
        });
        (url, done, server)
    }

    fn flagged_connector(url: String, ended: Arc<AtomicBool>) -> Connector {
        let exit = ExitFlag(ended);
        Box::new(move || {
            let _ = &exit;
            Ok(tungstenite::connect(url.as_str())?.0)
        })
    }

    /// The 2026-10-25 loop: a relay refusing a retired epoch's socket must not
    /// be reconnected every 250 ms.
    #[test]
    fn a_relay_refusal_of_a_retired_epoch_ends_the_link_after_one_connect() {
        for (code, reason, retire) in [(1008u16, "", true), (1001, "stale epoch", false)] {
            let (gate_tx, gate_rx) = mpsc::channel();
            let (url, done, server) = refusing_relay(code, reason, Some(gate_rx));
            let ended = Arc::new(AtomicBool::new(false));
            let started = Instant::now();
            let link = V4Link::spawn_for_epoch(
                flagged_connector(url, ended.clone()),
                "0123456789abcdef0123456789abcdef",
                6_000_000,
                6_000_000,
            )
            .unwrap();
            if retire {
                link.retire_epoch();
            }
            gate_tx.send(()).unwrap();
            assert!(wait_for_flag(&ended), "link kept reconnecting");
            assert!(started.elapsed() < Duration::from_secs(2));
            // Linger past the minimum backoff so a reconnect would be seen.
            thread::sleep(RECONNECT_BACKOFF_MIN * 2);
            done.store(true, Ordering::Release);
            assert_eq!(server.join().unwrap().len(), 1, "close {code} {reason:?}");
            assert_eq!(
                link.take_events().link_end,
                Some(V4LinkEnd::Retired {
                    code,
                    reason: reason.to_owned()
                })
            );
            assert!(!link.stats().connected);
            // A self-ended link backpressures; it never reports Stopped.
            assert_eq!(link.reserve(true), V4Reserve::Backpressured);
            link.shutdown();
        }
    }

    #[test]
    fn repeated_refusals_of_the_active_epoch_back_off_and_end_the_link() {
        let (url, done, server) = refusing_relay(1008, "", None);
        let ended = Arc::new(AtomicBool::new(false));
        let link = V4Link::spawn_for_epoch(
            flagged_connector(url, ended.clone()),
            "0123456789abcdef0123456789abcdef",
            6_000_000,
            6_000_000,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ended.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "link never gave up");
            thread::sleep(Duration::from_millis(10));
        }
        done.store(true, Ordering::Release);
        let accepts = server.join().unwrap();
        assert_eq!(accepts.len(), MEDIA_REFUSAL_LIMIT as usize);
        let gaps: Vec<Duration> = accepts.windows(2).map(|pair| pair[1] - pair[0]).collect();
        // A connect the relay refuses never resets the backoff.
        for (gap, expected_ms) in gaps.iter().zip([250u64, 500, 1_000, 2_000]) {
            assert!(
                *gap >= Duration::from_millis(expected_ms - 20),
                "gaps {gaps:?}"
            );
        }
        assert_eq!(
            link.take_events().link_end,
            Some(V4LinkEnd::Refused {
                code: 1008,
                reason: String::new(),
                refusals: MEDIA_REFUSAL_LIMIT
            })
        );
        link.shutdown();
    }

    /// Caps the test connector's wait, so the old unbounded join still ends.
    const TEST_CONNECTOR_CAP: Duration = Duration::from_secs(2);

    /// Drops with the connector, which the worker drops when it ends.
    struct ExitFlag(Arc<AtomicBool>);

    impl Drop for ExitFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    fn wait_for_flag(flag: &AtomicBool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !flag.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        true
    }

    /// Blocks like a stuck connect until `release` or the cap, then returns
    /// `socket` as a connection, or fails when there is none.
    fn blocked_connector(
        entered: SyncSender<()>,
        release: Receiver<()>,
        ended: Arc<AtomicBool>,
        socket: Mutex<Option<TcpStream>>,
    ) -> Connector {
        let exit = ExitFlag(ended);
        Box::new(move || {
            let _ = &exit;
            let _ = entered.try_send(());
            let _ = release.recv_timeout(TEST_CONNECTOR_CAP);
            match socket.lock().unwrap().take() {
                Some(stream) => Ok(WebSocket::from_raw_socket(
                    MaybeTlsStream::Plain(stream),
                    tungstenite::protocol::Role::Client,
                    None,
                )),
                None => bail!("test connector released without a socket"),
            }
        })
    }

    #[test]
    fn shutdown_returns_while_a_connect_is_stuck() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let ended = Arc::new(AtomicBool::new(false));
        let connector = blocked_connector(entered_tx, release_rx, ended.clone(), Mutex::new(None));
        let link = V4Link::spawn(connector, 2_000_000, 6_000_000).expect("spawn link");
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("connector entered");

        let started = Instant::now();
        link.shutdown();
        let waited = started.elapsed();
        let ended_before_release = ended.load(Ordering::Acquire);
        // Let the abandoned worker's connect fail now, so the test ends cleanly.
        let _ = release_tx.send(());
        assert!(
            !ended_before_release,
            "shutdown must return before the blocked connector finishes"
        );
        assert!(
            // Allow scheduling slack around the 400 ms shutdown wait while
            // still rejecting the old join's two-second connector wait.
            waited < Duration::from_millis(1200),
            "shutdown waited {waited:?} for a stuck connect"
        );
        assert!(wait_for_flag(&ended), "abandoned worker never ended");
    }

    #[test]
    fn connect_finishing_after_stop_publishes_and_sends_nothing() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let client = TcpStream::connect(listener.local_addr().expect("local addr"))
            .expect("connect loopback");
        let (mut peer, _) = listener.accept().expect("accept loopback");
        peer.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("peer timeout");

        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let ended = Arc::new(AtomicBool::new(false));
        let connector = blocked_connector(
            entered_tx,
            release_rx,
            ended.clone(),
            Mutex::new(Some(client)),
        );
        let link = V4Link::spawn(connector, 2_000_000, 6_000_000).expect("spawn link");
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("connector entered");
        // Media a published connection would send.
        link.tx
            .try_send(Outgoing {
                peer_epoch: 0,
                packet: vec![0xAA; 16],
            })
            .expect("queue packet");

        link.shutdown();
        let _ = release_tx.send(());
        assert!(wait_for_flag(&ended), "worker never ended");

        {
            let state = link.lock();
            assert_eq!(state.peer_epoch, 0, "a stopped link published a connection");
            assert!(!state.connected, "a stopped link is marked connected");
        }
        // Dropping the unpublished socket closes it and writes nothing.
        let mut byte = [0u8; 1];
        assert_eq!(
            peer.read(&mut byte).expect("read from peer"),
            0,
            "no media or close frame may reach the relay after stop"
        );
    }

    #[test]
    fn worker_budget_refuses_when_full_and_frees_on_release() {
        static LIVE: AtomicUsize = AtomicUsize::new(0);
        let first = claim_worker_slot(&LIVE, 2).expect("first slot");
        let second = claim_worker_slot(&LIVE, 2).expect("second slot");
        assert!(
            claim_worker_slot(&LIVE, 2).is_err(),
            "budget must refuse a third worker"
        );
        drop(first);
        let third = claim_worker_slot(&LIVE, 2).expect("freed slot is reusable");
        drop((second, third));
        assert_eq!(LIVE.load(Ordering::Acquire), 0);
    }
}
