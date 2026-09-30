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

/// In-flight budget: skip (before encode) once un-ACKed bytes exceed this
/// much airtime at the current bitrate, or once this many frames are
/// un-ACKed.
pub const INFLIGHT_BUDGET: Duration = Duration::from_millis(150);
pub const INFLIGHT_MAX_FRAMES: usize = 4;
/// No ACK progress for this long while frames are in flight: forget the
/// in-flight frames so sending can resume. This never forces a keyframe; a
/// receiver whose decoder chain broke asks for one (seq gap / decode error).
pub const ACK_STALL_LIMIT: Duration = Duration::from_secs(3);
/// Idle heartbeat on the media socket.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// Declare the socket dead after this long without any inbound message
/// (the relay answers every heartbeat with a pong).
pub const INBOUND_SILENCE_LIMIT: Duration = Duration::from_secs(40);
pub const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(250);
pub const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);
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
}

impl SendPolicy {
    pub fn new(now: Instant) -> Self {
        Self {
            inflight: VecDeque::new(),
            inflight_bytes: 0,
            need_key: true,
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
        let Some(target_bps) = target_bps else {
            return None;
        };
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
}

struct Shared {
    connected: bool,
    peer_present: bool,
    policy: SendPolicy,
    rate: DelayRateController,
    capacity: CapacityEstimator,
    /// Last capacity update, for `constrained_ms`.
    capacity_tick: Option<Instant>,
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

pub type Connector = Box<dyn Fn() -> Result<WebSocket<MaybeTlsStream<TcpStream>>> + Send + 'static>;

/// Owns the single v4 media socket on a dedicated thread.
pub struct V4Link {
    shared: Arc<Mutex<Shared>>,
    tx: SyncSender<Outgoing>,
    stop: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl V4Link {
    pub fn spawn(connector: Connector, bitrate_bps: u64, max_bps: u64) -> Result<Self> {
        let now = Instant::now();
        let shared = Arc::new(Mutex::new(Shared {
            connected: false,
            peer_present: false,
            policy: SendPolicy::new(now),
            rate: DelayRateController::new(now, bitrate_bps, max_bps),
            capacity: CapacityEstimator::new(now),
            capacity_tick: None,
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
            tx,
            stop,
            join: Mutex::new(Some(join)),
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
        match tx.try_send(Outgoing { packet }) {
            Ok(()) => {
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

    pub fn take_events(&self) -> V4Events {
        std::mem::take(&mut self.lock().events)
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
        self.stop.store(true, Ordering::Release);
        let join = self
            .join
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if let Some(join) = join {
            let _ = join.join();
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
    info!(
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
    shared.events.bitrate_bps = Some(decision.bitrate_bps);
    shared.events.bitrate_reason = Some(decision.reason.as_str());
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
            state.capacity.forget_inflight();
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
            state.capacity.forget_inflight();
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
            state.capacity.on_ack(now, generation, seq);
            apply_capacity(&mut state, now);
            if let Some(outcome) = state.policy.on_ack(now, generation, seq) {
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
            state.peer_present = present;
            state.policy.reset(now);
            state.capacity.forget_inflight();
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
            state.stats.ack_timeouts += 1;
            state.capacity.forget_inflight();
        }
        apply_capacity(&mut state, now);
        let decision = state.rate.tick(now);
        record_rate_decision(&mut state, decision);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(policy.admit(false, 6_000_000), Admission::RequireKeyframe);
        assert_eq!(policy.admit(true, 6_000_000), Admission::Send);
        policy.on_sent(now, 1, 1, 10_000, true);
        assert_eq!(policy.admit(false, 6_000_000), Admission::Send);
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
}
