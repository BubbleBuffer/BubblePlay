//! Bounded WebSocket media egress for the Worker media relay.
//!
//! This module deliberately owns no capture or encoder state.  The producer
//! first reserves a lane, then captures/encodes only when that reservation is
//! available.  A full or stale lane is therefore a drop signal, never a reason
//! to wait in the capture path.

use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::quality::StreamMode;
use anyhow::{anyhow, bail, Context, Result};
use tracing::{debug, warn};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::{header::AUTHORIZATION, HeaderName, HeaderValue, Request};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

pub const STRIPE_COUNT: usize = 4;
pub const FRAME_VERSION: u8 = 2;
pub const FRAME_HEADER_BYTES: usize = 24;
pub const DATA_SHARDS: usize = 3;
pub const V3_FRAME_VERSION: u8 = 3;
pub const V3_FRAME_HEADER_BYTES: usize = 28;
pub const MAX_V3_LANES: usize = 16;
pub const MAX_ACCESS_UNIT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_MEDIA_PACKET_BYTES: usize =
    FRAME_HEADER_BYTES + MAX_ACCESS_UNIT_BYTES.div_ceil(DATA_SHARDS);
pub const LANE_QUEUE_CAPACITY: usize = 1;
pub const WORK_ACK_STALL_LIMIT: Duration = Duration::from_millis(1_200);
pub const GAME_ACK_STALL_LIMIT: Duration = Duration::from_millis(500);
pub const WORK_RECOVERY_PROBE_INTERVAL: Duration = Duration::from_secs(2);
pub const GAME_RECOVERY_PROBE_INTERVAL: Duration = Duration::from_secs(1);
pub const WORK_ACK_WINDOW_FRAMES: u32 = 8;
pub const GAME_ACK_WINDOW_FRAMES: u32 = 16;
const V3_FEC_XOR: u8 = 0;
const V3_FEC_PQ: u8 = 1;
const LAB_HOST_TRACE_BYTES: usize = 20;
const LAB_HOST_TRACE_MAGIC: u32 = 0x4250_4854;
const LAB_HOST_TRACE_VERSION: u8 = 1;
const STREAM_LAB_COMPATIBILITY_ID: &str = "wss-v3-host20-edge12-bench2-hevc-tiles-v14";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WssTopology {
    lane_count: usize,
    data_shards: usize,
    parity_shards: usize,
}

impl WssTopology {
    pub const V2: Self = Self {
        lane_count: STRIPE_COUNT,
        data_shards: DATA_SHARDS,
        parity_shards: 1,
    };

    pub fn new(lane_count: usize, data_shards: usize, parity_shards: usize) -> Result<Self> {
        if !(1..=MAX_V3_LANES).contains(&lane_count)
            || data_shards == 0
            || parity_shards > 2
            || data_shards + parity_shards != lane_count
        {
            bail!("invalid WSS media topology");
        }
        Ok(Self {
            lane_count,
            data_shards,
            parity_shards,
        })
    }

    pub const fn lane_count(self) -> usize {
        self.lane_count
    }

    pub const fn data_shards(self) -> usize {
        self.data_shards
    }

    pub const fn parity_shards(self) -> usize {
        self.parity_shards
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCodec {
    Hevc,
    H264,
}

impl MediaCodec {
    pub const fn wire(self) -> u8 {
        match self {
            Self::Hevc => 1,
            Self::H264 => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LabRoiReason {
    None = 0,
    Applied = 1,
    MissingDamage = 2,
    EmptyDamage = 3,
    Idr = 4,
    CoverageAtLeastHalf = 5,
    UnsupportedEncoderState = 6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabRoiTrace {
    pub reason: LabRoiReason,
    pub changed_ctbs: u16,
    pub total_ctbs: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessUnitMeta {
    pub codec: MediaCodec,
    pub generation: u32,
    pub gop: u32,
    pub sequence: u32,
    pub timestamp90k: u32,
    pub key: bool,
    pub lab_roi: Option<LabRoiTrace>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    Queued { stripe: u8 },
    DroppedRequireIdr,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneAck {
    pub stripe: u8,
    pub generation: u32,
    pub gop: u32,
    pub sequence: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneHealth {
    pub stripe: u8,
    pub queued: bool,
    pub healthy: bool,
    pub awaiting_ack: bool,
    pub require_idr: bool,
    pub recovery_pending: bool,
    pub recovery_sequence: Option<u32>,
    pub ack_lag: u32,
    pub state: LaneRecoveryState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoverySnapshotStatus {
    pub needs_idr: bool,
    pub awaiting_ack: bool,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneRecoveryState {
    Ready = 0,
    NeedsRecoveryIdr = 1,
    AwaitingRecoveryAck = 2,
    Unavailable = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveOutcome {
    /// At least three healthy lanes exist, but one or more require a fresh
    /// random-access frame before they can rejoin the shard set.
    RequireKeyframe,
    /// Fewer than three lanes can accept a frame right now.  The producer
    /// should skip capture/encode and retry later without forcing an IDR.
    Backpressured,
    Stopped,
}

#[derive(Debug)]
struct QueuedAccessUnit {
    meta: AccessUnitMeta,
    packet: Vec<u8>,
    enqueued_at: Instant,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WssMediaWindowStats {
    pub reserve_count: u64,
    pub reserve_us_total: u64,
    pub reserve_us_max: u64,
    pub reserve_backpressured: u64,
    pub reserve_require_keyframe: u64,
    pub blocked_queued_lanes: u64,
    pub blocked_ack_window_lanes: u64,
    pub blocked_recovery_lanes: u64,
    pub unavailable_lanes: u64,
    pub submit_count: u64,
    pub access_unit_bytes: u64,
    pub packet_bytes: u64,
    pub shard_us_total: u64,
    pub shard_us_max: u64,
    pub dispatch_us_total: u64,
    pub dispatch_us_max: u64,
    pub queue_full: u64,
    pub queue_age_us_total: u64,
    pub queue_age_us_max: u64,
    pub send_count: u64,
    pub send_us_total: u64,
    pub send_us_max: u64,
    pub send_over_10ms: u64,
    pub send_over_frame_budget: u64,
    pub send_failures: u64,
}

#[derive(Debug, Default)]
struct MediaWindowStats {
    reserve_count: AtomicU64,
    reserve_us_total: AtomicU64,
    reserve_us_max: AtomicU64,
    reserve_backpressured: AtomicU64,
    reserve_require_keyframe: AtomicU64,
    blocked_queued_lanes: AtomicU64,
    blocked_ack_window_lanes: AtomicU64,
    blocked_recovery_lanes: AtomicU64,
    unavailable_lanes: AtomicU64,
    submit_count: AtomicU64,
    access_unit_bytes: AtomicU64,
    packet_bytes: AtomicU64,
    shard_us_total: AtomicU64,
    shard_us_max: AtomicU64,
    dispatch_us_total: AtomicU64,
    dispatch_us_max: AtomicU64,
    queue_full: AtomicU64,
    queue_age_us_total: AtomicU64,
    queue_age_us_max: AtomicU64,
    send_count: AtomicU64,
    send_us_total: AtomicU64,
    send_us_max: AtomicU64,
    send_over_10ms: AtomicU64,
    send_over_frame_budget: AtomicU64,
    send_failures: AtomicU64,
}

impl MediaWindowStats {
    fn record_duration(total: &AtomicU64, max: &AtomicU64, elapsed: Duration) {
        let micros = elapsed.as_micros().min(u64::MAX as u128) as u64;
        total.fetch_add(micros, Ordering::Relaxed);
        max.fetch_max(micros, Ordering::Relaxed);
    }

    fn take(&self) -> WssMediaWindowStats {
        macro_rules! take {
            ($field:ident) => {
                self.$field.swap(0, Ordering::Relaxed)
            };
        }
        WssMediaWindowStats {
            reserve_count: take!(reserve_count),
            reserve_us_total: take!(reserve_us_total),
            reserve_us_max: take!(reserve_us_max),
            reserve_backpressured: take!(reserve_backpressured),
            reserve_require_keyframe: take!(reserve_require_keyframe),
            blocked_queued_lanes: take!(blocked_queued_lanes),
            blocked_ack_window_lanes: take!(blocked_ack_window_lanes),
            blocked_recovery_lanes: take!(blocked_recovery_lanes),
            unavailable_lanes: take!(unavailable_lanes),
            submit_count: take!(submit_count),
            access_unit_bytes: take!(access_unit_bytes),
            packet_bytes: take!(packet_bytes),
            shard_us_total: take!(shard_us_total),
            shard_us_max: take!(shard_us_max),
            dispatch_us_total: take!(dispatch_us_total),
            dispatch_us_max: take!(dispatch_us_max),
            queue_full: take!(queue_full),
            queue_age_us_total: take!(queue_age_us_total),
            queue_age_us_max: take!(queue_age_us_max),
            send_count: take!(send_count),
            send_us_total: take!(send_us_total),
            send_us_max: take!(send_us_max),
            send_over_10ms: take!(send_over_10ms),
            send_over_frame_budget: take!(send_over_frame_budget),
            send_failures: take!(send_failures),
        }
    }
}

#[derive(Debug)]
struct LaneState {
    queued: AtomicBool,
    reconnect_requested: AtomicBool,
    recovery_state: AtomicU8,
    recovery_generation: AtomicU64,
    recovery_sequence: AtomicU64,
    recovery_sent_ms: AtomicU64,
    last_sent_ms: AtomicU64,
    last_sent_generation: AtomicU64,
    last_ack_ms: AtomicU64,
    last_ack_generation: AtomicU64,
    last_ack_sequence: AtomicU64,
    last_sent_sequence: AtomicU64,
}

impl LaneState {
    fn new() -> Self {
        Self {
            queued: AtomicBool::new(false),
            reconnect_requested: AtomicBool::new(false),
            recovery_state: AtomicU8::new(LaneRecoveryState::Ready as u8),
            recovery_generation: AtomicU64::new(0),
            recovery_sequence: AtomicU64::new(0),
            recovery_sent_ms: AtomicU64::new(0),
            last_sent_ms: AtomicU64::new(0),
            last_sent_generation: AtomicU64::new(0),
            last_ack_ms: AtomicU64::new(0),
            last_ack_generation: AtomicU64::new(0),
            last_ack_sequence: AtomicU64::new(0),
            last_sent_sequence: AtomicU64::new(0),
        }
    }

    fn state(&self) -> LaneRecoveryState {
        match self.recovery_state.load(Ordering::Acquire) {
            0 => LaneRecoveryState::Ready,
            1 => LaneRecoveryState::NeedsRecoveryIdr,
            2 => LaneRecoveryState::AwaitingRecoveryAck,
            _ => LaneRecoveryState::Unavailable,
        }
    }

    fn set_state(&self, state: LaneRecoveryState) {
        self.recovery_state.store(state as u8, Ordering::Release);
    }

    fn rearm_if_awaiting_recovery_ack(&self) -> bool {
        self.recovery_state
            .compare_exchange(
                LaneRecoveryState::AwaitingRecoveryAck as u8,
                LaneRecoveryState::NeedsRecoveryIdr as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn request_transport_reconnect(&self) {
        self.reconnect_requested.store(true, Ordering::Release);
        self.set_state(LaneRecoveryState::Unavailable);
    }

    fn take_transport_reconnect_request(&self) -> bool {
        self.reconnect_requested.swap(false, Ordering::AcqRel)
    }
}

fn dispatch_packets(
    stripes: &[usize],
    meta: AccessUnitMeta,
    protocol: u8,
    data_shards: usize,
    lanes: &[Arc<LaneState>],
    senders: &[SyncSender<QueuedAccessUnit>],
    stats: &MediaWindowStats,
    packets: Vec<Vec<u8>>,
) -> usize {
    let mut packets = packets.into_iter().map(Some).collect::<Vec<_>>();
    let mut queued = 0;
    for stripe in dispatch_order(stripes, protocol, meta.sequence, data_shards) {
        let Some(packet) = packets.get_mut(stripe).and_then(Option::take) else {
            continue;
        };
        let item = QueuedAccessUnit {
            meta,
            packet,
            enqueued_at: Instant::now(),
        };
        stats
            .packet_bytes
            .fetch_add(item.packet.len() as u64, Ordering::Relaxed);
        match senders[stripe].try_send(item) {
            Ok(()) => queued += 1,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                stats.queue_full.fetch_add(1, Ordering::Relaxed);
                lanes[stripe].set_state(LaneRecoveryState::Unavailable);
                lanes[stripe].queued.store(false, Ordering::Release);
            }
        }
    }
    queued
}

fn dispatch_order(
    stripes: &[usize],
    _protocol: u8,
    _sequence: u32,
    _data_shards: usize,
) -> impl Iterator<Item = usize> + '_ {
    stripes.iter().copied()
}

/// A reservation is intentionally consumed by `submit`; dropping it releases
/// its pre-encode slot without putting anything on a writer queue.
pub struct MediaReservation {
    stripes: Vec<usize>,
    meta: AccessUnitMeta,
    lanes: Arc<Vec<Arc<LaneState>>>,
    senders: Vec<SyncSender<QueuedAccessUnit>>,
    protocol: u8,
    topology: WssTopology,
    stats: Arc<MediaWindowStats>,
    active: bool,
}

impl MediaReservation {
    pub fn stripe(&self) -> u8 {
        self.stripes.first().copied().unwrap_or(0) as u8
    }
    pub fn set_meta(&mut self, meta: AccessUnitMeta) {
        self.meta = meta;
    }
    pub fn submit(mut self, bytes: Vec<u8>) -> SubmitOutcome {
        self.stats.submit_count.fetch_add(1, Ordering::Relaxed);
        self.stats
            .access_unit_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let shard_started = Instant::now();
        let packets = match match self.protocol {
            FRAME_VERSION => shard_access_unit(self.meta, &bytes),
            V3_FRAME_VERSION => shard_access_unit_v3(self.meta, &bytes, self.topology),
            _ => Err(anyhow!("unsupported WSS media protocol")),
        } {
            Ok(packets) => packets,
            Err(_) => {
                self.release();
                return SubmitOutcome::DroppedRequireIdr;
            }
        };
        MediaWindowStats::record_duration(
            &self.stats.shard_us_total,
            &self.stats.shard_us_max,
            shard_started.elapsed(),
        );
        let dispatch_started = Instant::now();
        let queued = dispatch_packets(
            &self.stripes,
            self.meta,
            self.protocol,
            self.topology.data_shards(),
            &self.lanes,
            &self.senders,
            &self.stats,
            packets,
        );
        MediaWindowStats::record_duration(
            &self.stats.dispatch_us_total,
            &self.stats.dispatch_us_max,
            dispatch_started.elapsed(),
        );
        self.active = false;
        if queued >= self.topology.data_shards() {
            SubmitOutcome::Queued { stripe: 0xff }
        } else {
            for lane in self.lanes.iter() {
                if lane.state() != LaneRecoveryState::Unavailable {
                    lane.set_state(LaneRecoveryState::NeedsRecoveryIdr);
                }
            }
            SubmitOutcome::DroppedRequireIdr
        }
    }
    fn release(&mut self) {
        for &stripe in &self.stripes {
            self.lanes[stripe].queued.store(false, Ordering::Release);
        }
        self.active = false;
    }
}

impl Drop for MediaReservation {
    fn drop(&mut self) {
        if self.active {
            self.release();
        }
    }
}

struct Coordinator {
    lanes: Arc<Vec<Arc<LaneState>>>,
    stopped: Arc<AtomicBool>,
    started: Instant,
    game_mode: AtomicBool,
    topology: WssTopology,
    required_shards: usize,
    stats: Arc<MediaWindowStats>,
}

impl Coordinator {
    fn now_ms(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.started).as_millis() as u64
    }
    fn mode(&self) -> StreamMode {
        if self.game_mode.load(Ordering::Acquire) {
            StreamMode::Game
        } else {
            StreamMode::Work
        }
    }
    fn stall_limit(&self) -> Duration {
        match self.mode() {
            StreamMode::Work => WORK_ACK_STALL_LIMIT,
            StreamMode::Game => GAME_ACK_STALL_LIMIT,
        }
    }
    fn recovery_probe_interval(&self) -> Duration {
        match self.mode() {
            StreamMode::Work => WORK_RECOVERY_PROBE_INTERVAL,
            StreamMode::Game => GAME_RECOVERY_PROBE_INTERVAL,
        }
    }
    fn ack_window_frames(&self) -> u32 {
        match self.mode() {
            StreamMode::Work => WORK_ACK_WINDOW_FRAMES,
            StreamMode::Game => GAME_ACK_WINDOW_FRAMES,
        }
    }
    fn expire_stalled(&self, now: Instant) {
        let now_ms = self.now_ms(now);
        for lane in self.lanes.iter() {
            let sent = lane.last_sent_ms.load(Ordering::Acquire);
            let progress = lane.last_ack_ms.load(Ordering::Acquire);
            let awaiting_since = sent.max(progress);
            let state = lane.state();
            let has_unacked = state == LaneRecoveryState::AwaitingRecoveryAck
                || (state == LaneRecoveryState::Ready
                    && !sequence_reached(
                        lane.last_ack_sequence.load(Ordering::Acquire) as u32,
                        lane.last_sent_sequence.load(Ordering::Acquire) as u32,
                    ));
            if has_unacked
                && now_ms.saturating_sub(awaiting_since) >= self.stall_limit().as_millis() as u64
            {
                if state == LaneRecoveryState::AwaitingRecoveryAck {
                    let recovery_sent = lane.recovery_sent_ms.load(Ordering::Acquire);
                    if now_ms.saturating_sub(recovery_sent)
                        < self.recovery_probe_interval().as_millis() as u64
                    {
                        continue;
                    }
                }
                if state == LaneRecoveryState::AwaitingRecoveryAck {
                    // Repeatedly writing a recovery IDR to a half-open WSS
                    // socket can succeed locally while the Worker no longer
                    // receives that lane. Rotate the transport before the next
                    // probe so a fresh Worker association carries the retry.
                    lane.request_transport_reconnect();
                } else {
                    // A first missing ACK does not prove the TCP socket is
                    // broken. Admit one recovery IDR, then gate this lane until
                    // that exact IDR is acknowledged.
                    lane.set_state(LaneRecoveryState::NeedsRecoveryIdr);
                }
            }
        }
    }

    fn recovery_snapshot_status(&self, now: Instant) -> RecoverySnapshotStatus {
        self.expire_stalled(now);
        let mut status = RecoverySnapshotStatus {
            needs_idr: false,
            awaiting_ack: false,
        };
        for lane in self.lanes.iter() {
            match lane.state() {
                LaneRecoveryState::NeedsRecoveryIdr => status.needs_idr = true,
                LaneRecoveryState::AwaitingRecoveryAck => status.awaiting_ack = true,
                LaneRecoveryState::Ready | LaneRecoveryState::Unavailable => {}
            }
        }
        status
    }

    fn rearm_awaiting_recovery_lanes(&self) -> usize {
        self.lanes
            .iter()
            .filter(|lane| lane.rearm_if_awaiting_recovery_ack())
            .count()
    }

    fn reserve_lanes(
        &self,
        meta: AccessUnitMeta,
        now: Instant,
    ) -> Result<Vec<usize>, ReserveOutcome> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(ReserveOutcome::Stopped);
        }
        self.expire_stalled(now);
        let mut reserved = Vec::with_capacity(self.lanes.len());
        let mut healthy = 0usize;
        let mut eligible = 0usize;
        let mut blocked_by_keyframe = false;
        let mut awaiting_recovery = false;
        let mut blocked_queued_lanes = 0u64;
        let mut blocked_ack_window_lanes = 0u64;
        let mut blocked_recovery_lanes = 0u64;
        let mut unavailable_lanes = 0u64;
        for (index, state) in self.lanes.iter().enumerate() {
            let lane_state = state.state();
            if lane_state == LaneRecoveryState::Unavailable {
                unavailable_lanes += 1;
                continue;
            }
            healthy += 1;
            let requires_idr = lane_state == LaneRecoveryState::NeedsRecoveryIdr;
            if lane_state == LaneRecoveryState::AwaitingRecoveryAck {
                awaiting_recovery = true;
                blocked_recovery_lanes += 1;
                continue;
            }
            let sent_generation = state.last_sent_generation.load(Ordering::Acquire) as u32;
            let sent_sequence = state.last_sent_sequence.load(Ordering::Acquire) as u32;
            let ack_generation = state.last_ack_generation.load(Ordering::Acquire) as u32;
            let ack_sequence = state.last_ack_sequence.load(Ordering::Acquire) as u32;
            let ack_lag = if sent_generation == ack_generation {
                sent_sequence.wrapping_sub(ack_sequence)
            } else {
                sent_sequence
            };
            if requires_idr {
                if !meta.key {
                    blocked_by_keyframe = true;
                    continue;
                }
            }
            eligible += 1;
            if !requires_idr && sent_sequence != 0 && ack_lag > self.ack_window_frames() {
                blocked_ack_window_lanes += 1;
                continue;
            }
            if state
                .queued
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                reserved.push(index);
            } else {
                blocked_queued_lanes += 1;
            }
        }
        // Parity is the network's loss budget, not spare host admission
        // capacity. When a lane is otherwise eligible for this frame, wait
        // for its bounded queue/ACK pressure instead of sending a quorum that
        // has already consumed every recovery shard. Explicitly unavailable
        // or recovering lanes remain excluded so parity can preserve
        // presentation through a real lane outage.
        let required_reservations = self.required_shards.max(eligible);
        if reserved.len() < required_reservations {
            for index in reserved {
                self.lanes[index].queued.store(false, Ordering::Release);
            }
            if !meta.key
                && healthy >= self.required_shards
                && blocked_by_keyframe
                && !awaiting_recovery
            {
                self.stats
                    .reserve_require_keyframe
                    .fetch_add(1, Ordering::Relaxed);
                return Err(ReserveOutcome::RequireKeyframe);
            }
            self.stats
                .reserve_backpressured
                .fetch_add(1, Ordering::Relaxed);
            self.stats
                .blocked_queued_lanes
                .fetch_add(blocked_queued_lanes, Ordering::Relaxed);
            self.stats
                .blocked_ack_window_lanes
                .fetch_add(blocked_ack_window_lanes, Ordering::Relaxed);
            self.stats
                .blocked_recovery_lanes
                .fetch_add(blocked_recovery_lanes, Ordering::Relaxed);
            self.stats
                .unavailable_lanes
                .fetch_add(unavailable_lanes, Ordering::Relaxed);
            return Err(ReserveOutcome::Backpressured);
        }
        Ok(reserved)
    }

    fn on_ack(&self, ack: LaneAck, now: Instant) {
        if let Some(lane) = self.lanes.get(ack.stripe as usize) {
            let sent_generation = lane.last_sent_generation.load(Ordering::Acquire) as u32;
            if sent_generation != 0 && ack.generation != sent_generation {
                return;
            }
            let last_sent_sequence = lane.last_sent_sequence.load(Ordering::Acquire) as u32;
            if last_sent_sequence == 0 || !sequence_reached(last_sent_sequence, ack.sequence) {
                return;
            }
            let previous_generation = lane.last_ack_generation.load(Ordering::Acquire) as u32;
            let previous_sequence = lane.last_ack_sequence.load(Ordering::Acquire) as u32;
            let progressed = previous_generation != ack.generation
                || sequence_reached(ack.sequence, previous_sequence.wrapping_add(1));
            if progressed {
                lane.last_ack_generation
                    .store(ack.generation as u64, Ordering::Release);
                lane.last_ack_sequence
                    .store(ack.sequence as u64, Ordering::Release);
                lane.last_ack_ms.store(self.now_ms(now), Ordering::Release);
            }
            if lane.state() == LaneRecoveryState::AwaitingRecoveryAck {
                let recovery_generation = lane.recovery_generation.load(Ordering::Acquire) as u32;
                let recovery_sequence = lane.recovery_sequence.load(Ordering::Acquire) as u32;
                if ack.generation == recovery_generation
                    && sequence_reached(ack.sequence, recovery_sequence)
                {
                    lane.set_state(LaneRecoveryState::Ready);
                }
            }
        }
    }
}

fn sequence_reached(candidate: u32, target: u32) -> bool {
    candidate.wrapping_sub(target) < (1 << 31)
}

/// Isolated WSS writers. A writer owns its socket so one stalled connection
/// cannot block the other negotiated lanes or the capture thread.
pub struct WssMediaEgress {
    coordinator: Arc<Coordinator>,
    senders: Vec<SyncSender<QueuedAccessUnit>>,
    joins: Vec<JoinHandle<()>>,
    protocol: u8,
    topology: WssTopology,
}

impl WssMediaEgress {
    /// Connect `worker_url` on `/host-media-ws?stripe=0..3`, authenticating the
    /// HTTP upgrade with `Authorization: Bearer <bearer>`.
    pub fn connect(worker_url: &str, bearer: &str, epoch: &str) -> Result<Self> {
        Self::connect_with_topology(worker_url, bearer, epoch, FRAME_VERSION, WssTopology::V2)
    }

    pub fn connect_v3(
        worker_url: &str,
        bearer: &str,
        epoch: &str,
        topology: WssTopology,
    ) -> Result<Self> {
        Self::connect_with_topology(worker_url, bearer, epoch, V3_FRAME_VERSION, topology)
    }

    fn connect_with_topology(
        worker_url: &str,
        bearer: &str,
        epoch: &str,
        protocol: u8,
        topology: WssTopology,
    ) -> Result<Self> {
        if protocol != FRAME_VERSION && protocol != V3_FRAME_VERSION {
            bail!("unsupported WSS media protocol");
        }
        if protocol == FRAME_VERSION && topology != WssTopology::V2 {
            bail!("protocol v2 requires fixed 3+1 topology");
        }
        if epoch.len() != 32
            || !epoch
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!("invalid WSS media epoch");
        }
        let started = Instant::now();
        let stats = Arc::new(MediaWindowStats::default());
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..topology.lane_count())
                .map(|_| Arc::new(LaneState::new()))
                .collect::<Vec<_>>(),
        );
        let stopped = Arc::new(AtomicBool::new(false));
        let lab_host_trace = lab_host_trace_enabled(worker_url, protocol);
        let coordinator = Arc::new(Coordinator {
            lanes: lanes.clone(),
            stopped: stopped.clone(),
            started,
            game_mode: AtomicBool::new(false),
            topology,
            required_shards: topology.data_shards(),
            stats: stats.clone(),
        });
        let mut senders = Vec::with_capacity(topology.lane_count());
        let mut joins = Vec::with_capacity(topology.lane_count());
        for stripe in 0..topology.lane_count() {
            // Every lane establishes independently inside its writer thread.
            // Serial TLS handshakes would make high-lane-count experiments
            // scale startup time linearly and let one blocked lane delay all.
            let ws = None;
            let (tx, rx) = mpsc::sync_channel(LANE_QUEUE_CAPACITY);
            let lane_state = lanes[stripe].clone();
            lane_state.set_state(if ws.is_some() {
                LaneRecoveryState::Ready
            } else {
                LaneRecoveryState::Unavailable
            });
            let writer_stop = stopped.clone();
            let writer_started = started;
            let writer_stats = stats.clone();
            let writer_url = worker_url.to_string();
            let writer_bearer = bearer.to_string();
            let writer_epoch = epoch.to_string();
            joins.push(
                thread::Builder::new()
                    .name(format!("bubbleplay-wss-media-{stripe}"))
                    .spawn(move || {
                        writer_loop(
                            ws,
                            rx,
                            lane_state,
                            writer_stop,
                            writer_started,
                            writer_stats,
                            writer_url,
                            writer_bearer,
                            writer_epoch,
                            stripe,
                            protocol,
                            lab_host_trace,
                        )
                    })
                    .context("spawn media writer")?,
            );
            senders.push(tx);
        }
        Ok(Self {
            coordinator,
            senders,
            joins,
            protocol,
            topology,
        })
    }

    /// Returns a pre-encode reservation.  The metadata is updated after the
    /// encoder has classified the access unit, immediately before submit.
    pub fn reserve(&self, meta: AccessUnitMeta) -> Result<MediaReservation, ReserveOutcome> {
        let started = Instant::now();
        let result = self
            .coordinator
            .reserve_lanes(meta, Instant::now())
            .map(|stripes| MediaReservation {
                stripes,
                meta,
                lanes: self.coordinator.lanes.clone(),
                senders: self.senders.clone(),
                protocol: self.protocol,
                topology: self.topology,
                stats: self.coordinator.stats.clone(),
                active: true,
            });
        self.coordinator
            .stats
            .reserve_count
            .fetch_add(1, Ordering::Relaxed);
        MediaWindowStats::record_duration(
            &self.coordinator.stats.reserve_us_total,
            &self.coordinator.stats.reserve_us_max,
            started.elapsed(),
        );
        result
    }
    pub fn on_ack(&self, ack: LaneAck) {
        self.coordinator.on_ack(ack, Instant::now());
    }
    pub fn rearm_awaiting_recovery_lanes(&self) -> usize {
        self.coordinator.rearm_awaiting_recovery_lanes()
    }
    /// Resolve the browser's global frame ACK to the stripe that owns its GOP.
    /// Stripe identity is host-authoritative and never trusted from the page.
    pub fn on_frame_ack(&self, generation: u32, gop: u32, sequence: u32) {
        for stripe in 0..self.coordinator.lanes.len() {
            self.on_ack(LaneAck {
                stripe: stripe as u8,
                generation,
                gop,
                sequence,
            });
        }
    }
    pub fn on_v2_ack(&self, generation: u32, lanes: [u32; STRIPE_COUNT]) {
        for (stripe, sequence) in lanes.into_iter().enumerate() {
            if sequence != 0 {
                self.on_ack(LaneAck {
                    stripe: stripe as u8,
                    generation,
                    gop: 0,
                    sequence,
                });
            }
        }
    }
    pub fn on_v3_ack(
        &self,
        generation: u32,
        lanes: &[u32],
        completion_missing_by_lane: &[u32],
    ) -> Result<()> {
        if self.protocol != V3_FRAME_VERSION || lanes.len() != self.topology.lane_count() {
            bail!("WSS media ACK topology mismatch");
        }
        if !completion_missing_by_lane.is_empty()
            && completion_missing_by_lane.len() != self.topology.lane_count()
        {
            bail!("WSS media completion telemetry topology mismatch");
        }
        for (stripe, &sequence) in lanes.iter().enumerate() {
            if sequence != 0 {
                self.on_ack(LaneAck {
                    stripe: stripe as u8,
                    generation,
                    gop: 0,
                    sequence,
                });
            }
        }
        Ok(())
    }
    pub fn set_stream_mode(&self, mode: StreamMode) {
        self.coordinator
            .game_mode
            .store(mode == StreamMode::Game, Ordering::Release);
    }
    pub const fn protocol(&self) -> u8 {
        self.protocol
    }
    pub const fn topology(&self) -> WssTopology {
        self.topology
    }
    pub fn wire_payload_bytes(&self, access_unit_bytes: usize) -> u64 {
        let header = if self.protocol == V3_FRAME_VERSION {
            V3_FRAME_HEADER_BYTES
        } else {
            FRAME_HEADER_BYTES
        };
        let shard = access_unit_bytes.div_ceil(self.topology.data_shards());
        self.topology.lane_count() as u64 * (shard + header) as u64
    }
    pub fn health(&self) -> Vec<LaneHealth> {
        self.coordinator.expire_stalled(Instant::now());
        self.coordinator
            .lanes
            .iter()
            .enumerate()
            .map(|(stripe, lane)| {
                let state = lane.state();
                LaneHealth {
                    stripe: stripe as u8,
                    queued: lane.queued.load(Ordering::Acquire),
                    healthy: state != LaneRecoveryState::Unavailable,
                    awaiting_ack: state == LaneRecoveryState::AwaitingRecoveryAck,
                    require_idr: state == LaneRecoveryState::NeedsRecoveryIdr,
                    recovery_pending: state == LaneRecoveryState::AwaitingRecoveryAck,
                    recovery_sequence: (state == LaneRecoveryState::AwaitingRecoveryAck)
                        .then(|| lane.recovery_sequence.load(Ordering::Acquire) as u32),
                    ack_lag: (lane.last_sent_sequence.load(Ordering::Acquire) as u32)
                        .wrapping_sub(lane.last_ack_sequence.load(Ordering::Acquire) as u32),
                    state,
                }
            })
            .collect()
    }
    pub fn recovery_snapshot_status(&self) -> RecoverySnapshotStatus {
        self.coordinator.recovery_snapshot_status(Instant::now())
    }
    pub fn take_window_stats(&self) -> WssMediaWindowStats {
        self.coordinator.stats.take()
    }
    pub fn shutdown(mut self) {
        self.coordinator.stopped.store(true, Ordering::Release);
        self.senders.clear();
        for join in self.joins.drain(..) {
            let _ = join.join();
        }
    }
}

fn set_socket_timeouts(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Duration,
) -> Result<()> {
    let tcp = match ws.get_mut() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::NativeTls(stream) => stream.get_ref(),
        other => bail!("unsupported TLS stream variant: {other:?}"),
    };
    tcp.set_write_timeout(Some(timeout))
        .context("set media socket write timeout")?;
    tcp.set_nodelay(true)
        .context("enable TCP_NODELAY for media socket")?;
    Ok(())
}

fn receive_queued_access_unit(
    rx: &Receiver<QueuedAccessUnit>,
    lane: &LaneState,
    stats: &MediaWindowStats,
    timeout: Duration,
) -> Result<QueuedAccessUnit, mpsc::RecvTimeoutError> {
    let au = rx.recv_timeout(timeout)?;
    MediaWindowStats::record_duration(
        &stats.queue_age_us_total,
        &stats.queue_age_us_max,
        au.enqueued_at.elapsed(),
    );
    // The writer now owns this packet. Release the channel admission flag so
    // one later frame may wait behind the in-flight socket send. The bounded
    // sync channel still backpressures once that single pipeline slot is full.
    lane.queued.store(false, Ordering::Release);
    Ok(au)
}

fn close_requested_transport(
    ws: &mut Option<WebSocket<MaybeTlsStream<TcpStream>>>,
    lane: &LaneState,
    stripe: usize,
) -> bool {
    if !lane.take_transport_reconnect_request() {
        return false;
    }
    if let Some(mut socket) = ws.take() {
        let _ = socket.close(None);
    }
    warn!(
        stripe,
        "WSS media stripe rotating after an unacknowledged recovery IDR"
    );
    true
}

fn writer_loop(
    mut ws: Option<WebSocket<MaybeTlsStream<TcpStream>>>,
    rx: Receiver<QueuedAccessUnit>,
    lane: Arc<LaneState>,
    stopped: Arc<AtomicBool>,
    started: Instant,
    stats: Arc<MediaWindowStats>,
    worker_url: String,
    bearer: String,
    epoch: String,
    stripe: usize,
    protocol: u8,
    lab_host_trace: bool,
) {
    while !stopped.load(Ordering::Acquire) {
        close_requested_transport(&mut ws, &lane, stripe);
        if ws.is_none() {
            match connect_media_socket(&worker_url, &bearer, &epoch, stripe, protocol) {
                Ok(reconnected) => {
                    ws = Some(reconnected);
                    lane.set_state(LaneRecoveryState::NeedsRecoveryIdr);
                }
                Err(error) => {
                    debug!(stripe, ?error, "WSS media stripe connect pending");
                    thread::sleep(Duration::from_millis(250));
                    continue;
                }
            }
        }
        let au = match receive_queued_access_unit(&rx, &lane, &stats, Duration::from_millis(20)) {
            Ok(au) => au,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if close_requested_transport(&mut ws, &lane, stripe) {
            while !stopped.load(Ordering::Acquire) {
                match connect_media_socket(&worker_url, &bearer, &epoch, stripe, protocol) {
                    Ok(reconnected) => {
                        ws = Some(reconnected);
                        lane.set_state(LaneRecoveryState::NeedsRecoveryIdr);
                        break;
                    }
                    Err(error) => {
                        debug!(stripe, ?error, "WSS media stripe reconnect pending");
                        thread::sleep(Duration::from_millis(250));
                    }
                }
            }
            if ws.is_none() {
                break;
            }
        }
        let send_started = Instant::now();
        let sent = ws.as_mut().is_some_and(|socket| {
            // tungstenite::WebSocket::send writes and immediately flushes.
            // A second flush adds one redundant call per lane and frame.
            if lab_host_trace {
                let mut packet = au.packet;
                append_lab_host_trace(&mut packet, unix_epoch_ms(), au.meta.lab_roi);
                socket.send(Message::Binary(packet.into())).is_ok()
            } else {
                socket.send(Message::Binary(au.packet.into())).is_ok()
            }
        });
        let send_elapsed = send_started.elapsed();
        stats.send_count.fetch_add(1, Ordering::Relaxed);
        MediaWindowStats::record_duration(&stats.send_us_total, &stats.send_us_max, send_elapsed);
        if send_elapsed >= Duration::from_millis(10) {
            stats.send_over_10ms.fetch_add(1, Ordering::Relaxed);
        }
        if send_elapsed >= Duration::from_millis(33) {
            stats.send_over_frame_budget.fetch_add(1, Ordering::Relaxed);
        }
        if sent {
            let last_sent = lane.last_sent_sequence.load(Ordering::Acquire) as u32;
            let last_ack = lane.last_ack_sequence.load(Ordering::Acquire) as u32;
            let had_unacked = last_sent != 0 && !sequence_reached(last_ack, last_sent);
            lane.last_sent_generation
                .store(au.meta.generation as u64, Ordering::Release);
            lane.last_sent_sequence
                .store(au.meta.sequence as u64, Ordering::Release);
            if !had_unacked {
                lane.last_sent_ms
                    .store(started.elapsed().as_millis() as u64, Ordering::Release);
            }
            if au.meta.key && lane.state() == LaneRecoveryState::NeedsRecoveryIdr {
                lane.recovery_generation
                    .store(au.meta.generation as u64, Ordering::Release);
                lane.recovery_sequence
                    .store(au.meta.sequence as u64, Ordering::Release);
                lane.recovery_sent_ms
                    .store(started.elapsed().as_millis() as u64, Ordering::Release);
                lane.set_state(LaneRecoveryState::AwaitingRecoveryAck);
            }
        } else {
            stats.send_failures.fetch_add(1, Ordering::Relaxed);
            warn!(
                stripe,
                sequence = au.meta.sequence,
                "WSS media stripe send failed; reconnecting"
            );
            lane.set_state(LaneRecoveryState::Unavailable);
            while !stopped.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(250));
                if let Ok(reconnected) =
                    connect_media_socket(&worker_url, &bearer, &epoch, stripe, protocol)
                {
                    ws = Some(reconnected);
                    lane.set_state(LaneRecoveryState::NeedsRecoveryIdr);
                    break;
                }
            }
        }
    }
    if let Some(mut socket) = ws {
        let _ = socket.close(None);
    }
}

fn connect_media_socket(
    worker_url: &str,
    bearer: &str,
    epoch: &str,
    stripe: usize,
    protocol: u8,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let request = media_request(worker_url, bearer, epoch, stripe, protocol)?;
    let (mut ws, _) = connect(request).with_context(|| format!("connect media stripe {stripe}"))?;
    set_socket_timeouts(&mut ws, Duration::from_millis(500))?;
    ws.send(Message::text(r#"{"type":"hello","role":"media-host"}"#))
        .context("send media hello")?;
    ws.flush().context("flush media hello")?;
    Ok(ws)
}

fn media_request(
    worker_url: &str,
    bearer: &str,
    epoch: &str,
    stripe: usize,
    protocol: u8,
) -> Result<Request<()>> {
    let worker_name = std::env::var("BUBBLEPLAY_LAB_WORKER_NAME")
        .ok()
        .filter(|value| !value.is_empty());
    let version_id = std::env::var("BUBBLEPLAY_LAB_WORKER_VERSION_ID")
        .ok()
        .filter(|value| !value.is_empty());
    let worker_version = match (worker_name.as_deref(), version_id.as_deref()) {
        (None, None) => None,
        (Some(worker_name), Some(version_id)) => Some((worker_name, version_id)),
        _ => bail!(
            "Stream Lab Worker version override requires both worker name and version identifier"
        ),
    };
    media_request_with_worker_version(worker_url, bearer, epoch, stripe, protocol, worker_version)
}

fn media_request_with_worker_version(
    worker_url: &str,
    bearer: &str,
    epoch: &str,
    stripe: usize,
    protocol: u8,
    worker_version: Option<(&str, &str)>,
) -> Result<Request<()>> {
    if protocol != FRAME_VERSION && protocol != V3_FRAME_VERSION {
        bail!("unsupported WSS media protocol");
    }
    let lane_limit = if protocol == FRAME_VERSION {
        STRIPE_COUNT
    } else {
        MAX_V3_LANES
    };
    if stripe >= lane_limit {
        bail!("invalid WSS media stripe");
    }
    let mut url = tungstenite::http::Uri::try_from(worker_url)
        .context("invalid Worker URL")?
        .to_string();
    if let Some(rest) = url.strip_prefix("https://") {
        url = format!("wss://{rest}");
    } else if let Some(rest) = url.strip_prefix("http://") {
        url = format!("ws://{rest}");
    }
    let mut parsed: tungstenite::http::Uri = url.parse().context("invalid WebSocket Worker URL")?;
    let authority = parsed
        .authority()
        .context("Worker URL has no authority")?
        .as_str()
        .to_owned();
    let scheme = parsed.scheme_str().unwrap_or("wss").to_owned();
    let lab_route = if parsed.path() == "/lab-host-ws" {
        let query = parsed.query().unwrap_or_default();
        let run = query
            .split('&')
            .find_map(|field| field.strip_prefix("run="))
            .filter(|run| canonical_uuid(run))
            .map(str::to_owned)
            .context("lab Worker URL requires a canonical run id")?;
        let compatibility = query
            .split('&')
            .find_map(|field| field.strip_prefix("compat="))
            .filter(|value| *value == STREAM_LAB_COMPATIBILITY_ID)
            .map(str::to_owned)
            .context("lab Worker URL requires the current compatibility identifier")?;
        Some((run, compatibility))
    } else {
        None
    };
    let media_path = if lab_route.is_some() {
        "/lab-host-media-ws"
    } else {
        "/host-media-ws"
    };
    let lab_query = lab_route
        .map(|(run, compatibility)| format!("&run={run}&compat={compatibility}"))
        .unwrap_or_default();
    parsed = format!(
        "{scheme}://{authority}{media_path}?v={protocol}&epoch={epoch}&stripe={stripe}{lab_query}"
    )
    .parse()
    .expect("constructed media URL is valid");
    let mut request = parsed
        .to_string()
        .into_client_request()
        .context("build media websocket request")?;
    let value = HeaderValue::from_str(&format!("Bearer {bearer}"))
        .context("host bearer is not a valid HTTP header value")?;
    request.headers_mut().insert(AUTHORIZATION, value);
    if parsed.path() == "/lab-host-media-ws" {
        if let Some((worker_name, version_id)) = worker_version {
            if !canonical_worker_name(worker_name) || !canonical_worker_version_id(version_id) {
                bail!("Stream Lab Worker version override is not canonical");
            }
            let value = HeaderValue::from_str(&format!("{worker_name}=\"{version_id}\""))
                .context("Stream Lab Worker version override is not a valid HTTP header value")?;
            request.headers_mut().insert(
                HeaderName::from_static("cloudflare-workers-version-overrides"),
                value,
            );
        }
    }
    Ok(request)
}

fn canonical_worker_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn canonical_worker_version_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
        && matches!(bytes[14], b'1'..=b'5')
        && matches!(bytes[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b')
}

fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && value
            .as_bytes()
            .iter()
            .enumerate()
            .all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => *byte == b'-',
                14 => matches!(*byte, b'1'..=b'5'),
                19 => matches!(byte.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b'),
                _ => byte.is_ascii_hexdigit(),
            })
}

fn lab_host_trace_enabled(worker_url: &str, protocol: u8) -> bool {
    if protocol != V3_FRAME_VERSION {
        return false;
    }
    let Ok(parsed) = worker_url.parse::<tungstenite::http::Uri>() else {
        return false;
    };
    parsed.path() == "/lab-host-ws"
        && parsed.query().is_some_and(|query| {
            let run = query
                .split('&')
                .find_map(|field| field.strip_prefix("run="))
                .is_some_and(canonical_uuid);
            let compatibility = query
                .split('&')
                .find_map(|field| field.strip_prefix("compat="))
                == Some(STREAM_LAB_COMPATIBILITY_ID);
            run && compatibility
        })
}

fn unix_epoch_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1_000.0
}

fn append_lab_host_trace(packet: &mut Vec<u8>, epoch_ms: f64, roi: Option<LabRoiTrace>) {
    packet.reserve(LAB_HOST_TRACE_BYTES);
    packet.extend_from_slice(&LAB_HOST_TRACE_MAGIC.to_be_bytes());
    packet.extend_from_slice(&epoch_ms.to_be_bytes());
    packet.push(LAB_HOST_TRACE_VERSION);
    packet.push(roi.map_or(LabRoiReason::None as u8, |trace| trace.reason as u8));
    packet.extend_from_slice(&roi.map_or(0, |trace| trace.changed_ctbs).to_be_bytes());
    packet.extend_from_slice(&roi.map_or(0, |trace| trace.total_ctbs).to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes());
}

/// Split one encoded access unit into three data shards plus XOR parity.
pub fn shard_access_unit(meta: AccessUnitMeta, data: &[u8]) -> Result<Vec<Vec<u8>>> {
    if data.is_empty() || data.len() > MAX_ACCESS_UNIT_BYTES {
        bail!("invalid access unit size");
    }
    let shard_len = data.len().div_ceil(DATA_SHARDS);
    let mut shards = vec![vec![0u8; shard_len]; STRIPE_COUNT];
    for (index, shard) in shards.iter_mut().take(DATA_SHARDS).enumerate() {
        let start = index * shard_len;
        let end = data.len().min(start + shard_len);
        if start < end {
            shard[..end - start].copy_from_slice(&data[start..end]);
        }
    }
    for byte in 0..shard_len {
        shards[3][byte] = shards[0][byte] ^ shards[1][byte] ^ shards[2][byte];
    }
    Ok(shards
        .into_iter()
        .enumerate()
        .map(|(stripe, shard)| {
            let mut packet = Vec::with_capacity(FRAME_HEADER_BYTES + shard.len());
            packet.extend_from_slice(&[
                FRAME_VERSION,
                meta.codec.wire(),
                u8::from(meta.key),
                stripe as u8,
            ]);
            packet.extend_from_slice(&meta.generation.to_be_bytes());
            packet.extend_from_slice(&meta.gop.to_be_bytes());
            packet.extend_from_slice(&meta.sequence.to_be_bytes());
            packet.extend_from_slice(&meta.timestamp90k.to_be_bytes());
            packet.extend_from_slice(&(data.len() as u32).to_be_bytes());
            packet.extend_from_slice(&shard);
            packet
        })
        .collect())
}

/// Split one encoded access unit according to the negotiated protocol-v3
/// topology. One parity lane is XOR P; two parity lanes are independent P/Q
/// shards over GF(256), allowing any two missing lanes to be reconstructed.
pub fn shard_access_unit_v3(
    meta: AccessUnitMeta,
    data: &[u8],
    topology: WssTopology,
) -> Result<Vec<Vec<u8>>> {
    if data.is_empty() || data.len() > MAX_ACCESS_UNIT_BYTES {
        bail!("invalid access unit size");
    }
    let shard_len = data.len().div_ceil(topology.data_shards());
    let mut shards = vec![vec![0u8; shard_len]; topology.lane_count()];
    for (index, shard) in shards.iter_mut().take(topology.data_shards()).enumerate() {
        let start = index * shard_len;
        let end = data.len().min(start + shard_len);
        if start < end {
            shard[..end - start].copy_from_slice(&data[start..end]);
        }
    }
    if topology.parity_shards() > 0 {
        let (data_shards, parity) = shards.split_at_mut(topology.data_shards());
        for byte in 0..shard_len {
            for (index, shard) in data_shards.iter().enumerate() {
                let value = shard[byte];
                parity[0][byte] ^= value;
                if parity.len() == 2 {
                    parity[1][byte] ^= gf_multiply(pq_coefficient(index), value);
                }
            }
        }
    }
    Ok(shards
        .into_iter()
        .enumerate()
        .map(|(stripe, shard)| {
            let mut packet = Vec::with_capacity(V3_FRAME_HEADER_BYTES + shard.len());
            packet.extend_from_slice(&[
                V3_FRAME_VERSION,
                meta.codec.wire(),
                u8::from(meta.key),
                stripe as u8,
                topology.lane_count() as u8,
                topology.data_shards() as u8,
                topology.parity_shards() as u8,
                if topology.parity_shards() == 2 {
                    V3_FEC_PQ
                } else {
                    V3_FEC_XOR
                },
            ]);
            packet.extend_from_slice(&meta.generation.to_be_bytes());
            packet.extend_from_slice(&meta.gop.to_be_bytes());
            packet.extend_from_slice(&meta.sequence.to_be_bytes());
            packet.extend_from_slice(&meta.timestamp90k.to_be_bytes());
            packet.extend_from_slice(&(data.len() as u32).to_be_bytes());
            packet.extend_from_slice(&shard);
            packet
        })
        .collect())
}

fn gf_multiply(mut left: u8, mut right: u8) -> u8 {
    let mut product = 0u8;
    for _ in 0..8 {
        if right & 1 != 0 {
            product ^= left;
        }
        let high = left & 0x80;
        left <<= 1;
        if high != 0 {
            left ^= 0x1d;
        }
        right >>= 1;
    }
    product
}

fn pq_coefficient(data_shard: usize) -> u8 {
    (0..data_shard).fold(1u8, |value, _| gf_multiply(value, 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(key: bool, generation: u32, gop: u32, sequence: u32) -> AccessUnitMeta {
        AccessUnitMeta {
            codec: MediaCodec::H264,
            generation,
            gop,
            sequence,
            timestamp90k: 0x0102_0304,
            key,
            lab_roi: None,
        }
    }

    #[test]
    fn v2_wire_bytes_match_worker_big_endian_layout() {
        let packets = shard_access_unit(
            meta(true, 0x1122_3344, 0x5566_7788, 0x99aa_bbcc),
            &[0xde, 0xad, 0xbe, 0xef],
        )
        .unwrap();
        let packet = &packets[2];
        assert_eq!(
            &packet[..24],
            &[
                2, 2, 1, 2, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
                1, 2, 3, 4, 0, 0, 0, 4
            ]
        );
        assert_eq!(&packet[24..], &[0, 0]);
    }

    #[test]
    fn any_three_v2_shards_reconstruct_uneven_access_unit() {
        let data: Vec<u8> = (0..100_003).map(|i| (i * 17) as u8).collect();
        let packets = shard_access_unit(meta(false, 1, 2, 3), &data).unwrap();
        assert_eq!(packets.len(), 4);
        let shard_len = data.len().div_ceil(3);
        let payloads: Vec<&[u8]> = packets.iter().map(|p| &p[FRAME_HEADER_BYTES..]).collect();
        for missing in 0..3 {
            let mut recovered = payloads[3].to_vec();
            for data_shard in 0..3 {
                if data_shard != missing {
                    for i in 0..shard_len {
                        recovered[i] ^= payloads[data_shard][i];
                    }
                }
            }
            let mut joined = Vec::new();
            for data_shard in 0..3 {
                joined.extend_from_slice(if data_shard == missing {
                    &recovered
                } else {
                    payloads[data_shard]
                });
            }
            joined.truncate(data.len());
            assert_eq!(joined, data);
        }
    }

    #[test]
    fn v3_topology_accepts_only_bounded_fec_shapes() {
        assert_eq!(WssTopology::new(2, 2, 0).unwrap().lane_count(), 2);
        assert_eq!(WssTopology::new(16, 15, 1).unwrap().data_shards(), 15);
        assert_eq!(WssTopology::new(16, 14, 2).unwrap().parity_shards(), 2);
        for invalid in [(0, 0, 0), (17, 16, 1), (4, 1, 3), (4, 3, 2), (3, 3, 1)] {
            assert!(WssTopology::new(invalid.0, invalid.1, invalid.2).is_err());
        }
    }

    #[test]
    fn v3_wire_header_is_self_describing_and_xor_recovers_each_data_lane() {
        let topology = WssTopology::new(9, 8, 1).unwrap();
        let data: Vec<u8> = (0..100_003).map(|i| (i * 17) as u8).collect();
        let packets = shard_access_unit_v3(meta(true, 7, 8, 9), &data, topology).unwrap();
        assert_eq!(packets.len(), 9);
        assert_eq!(
            &packets[8][..28],
            &[
                3, 2, 1, 8, 9, 8, 1, 0, 0, 0, 0, 7, 0, 0, 0, 8, 0, 0, 0, 9, 1, 2, 3, 4, 0, 1, 134,
                163,
            ]
        );
        let shard_len = data.len().div_ceil(topology.data_shards());
        let payloads: Vec<&[u8]> = packets
            .iter()
            .map(|p| &p[V3_FRAME_HEADER_BYTES..])
            .collect();
        for missing in 0..topology.data_shards() {
            let mut recovered = payloads[topology.data_shards()].to_vec();
            for (index, payload) in payloads.iter().take(topology.data_shards()).enumerate() {
                if index != missing {
                    for offset in 0..shard_len {
                        recovered[offset] ^= payload[offset];
                    }
                }
            }
            let mut joined = Vec::new();
            for (index, payload) in payloads.iter().take(topology.data_shards()).enumerate() {
                joined.extend_from_slice(if index == missing {
                    &recovered
                } else {
                    payload
                });
            }
            joined.truncate(data.len());
            assert_eq!(joined, data);
        }
    }

    #[test]
    fn v3_no_parity_emits_exactly_the_requested_data_lanes() {
        let topology = WssTopology::new(5, 5, 0).unwrap();
        let packets =
            shard_access_unit_v3(meta(false, 1, 2, 3), &[1, 2, 3, 4, 5, 6], topology).unwrap();
        assert_eq!(packets.len(), 5);
        assert!(packets.iter().all(|packet| packet[4..8] == [5, 5, 0, 0]));
    }

    #[test]
    fn v3_two_parity_lanes_encode_independent_p_and_q_recovery_shards() {
        let topology = WssTopology::new(6, 4, 2).unwrap();
        let data = [1, 2, 3, 4, 5, 6, 7, 8];
        let packets = shard_access_unit_v3(meta(true, 7, 8, 9), &data, topology).unwrap();
        assert_eq!(packets.len(), 6);
        assert_eq!(packets[4][7], 1);
        assert_eq!(&packets[4][V3_FRAME_HEADER_BYTES..], &[0, 8]);
        assert_eq!(&packets[5][V3_FRAME_HEADER_BYTES..], &[43, 82]);
    }

    #[test]
    fn derives_all_stripe_endpoints_and_sends_bearer_only_in_upgrade_header() {
        for stripe in 0..STRIPE_COUNT {
            let request = media_request(
                "https://worker.example/host-ws",
                "host-bearer",
                "0123456789abcdef0123456789abcdef",
                stripe,
                FRAME_VERSION,
            )
            .unwrap();
            assert_eq!(
                request.uri().to_string(),
                format!("wss://worker.example/host-media-ws?v=2&epoch=0123456789abcdef0123456789abcdef&stripe={stripe}")
            );
            assert_eq!(
                request.headers().get(AUTHORIZATION).unwrap(),
                "Bearer host-bearer"
            );
            assert!(!request.uri().to_string().contains("host-bearer"));
        }
    }

    #[test]
    fn derives_protocol_v3_endpoint_for_the_sixteenth_lane() {
        let request = media_request(
            "https://worker.example/host-ws",
            "host-bearer",
            "0123456789abcdef0123456789abcdef",
            15,
            3,
        )
        .unwrap();
        assert_eq!(
            request.uri().to_string(),
            "wss://worker.example/host-media-ws?v=3&epoch=0123456789abcdef0123456789abcdef&stripe=15"
        );
    }

    #[test]
    fn derives_run_scoped_lab_media_endpoints_from_lab_signaling_url() {
        let request = media_request(
            "wss://worker.example/lab-host-ws?run=123e4567-e89b-42d3-a456-426614174000&compat=wss-v3-host20-edge12-bench2-hevc-tiles-v14",
            "host-bearer",
            "0123456789abcdef0123456789abcdef",
            3,
            FRAME_VERSION,
        )
        .unwrap();
        assert_eq!(
            request.uri().to_string(),
            "wss://worker.example/lab-host-media-ws?v=2&epoch=0123456789abcdef0123456789abcdef&stripe=3&run=123e4567-e89b-42d3-a456-426614174000&compat=wss-v3-host20-edge12-bench2-hevc-tiles-v14"
        );
    }

    #[test]
    fn lab_media_worker_version_override_is_header_only_and_never_reaches_production() {
        let request = media_request_with_worker_version(
            "wss://worker.example/lab-host-ws?run=123e4567-e89b-42d3-a456-426614174000&compat=wss-v3-host20-edge12-bench2-hevc-tiles-v14",
            "host-bearer",
            "0123456789abcdef0123456789abcdef",
            15,
            V3_FRAME_VERSION,
            Some(("playfull-cat", "1a10a2ec-6b6a-4d6a-a7e6-5fd5696360d1")),
        )
        .unwrap();
        assert_eq!(
            request
                .headers()
                .get("Cloudflare-Workers-Version-Overrides")
                .unwrap(),
            r#"playfull-cat="1a10a2ec-6b6a-4d6a-a7e6-5fd5696360d1""#
        );

        let production = media_request_with_worker_version(
            "wss://worker.example/host-ws",
            "host-bearer",
            "0123456789abcdef0123456789abcdef",
            15,
            V3_FRAME_VERSION,
            Some(("playfull-cat", "1a10a2ec-6b6a-4d6a-a7e6-5fd5696360d1")),
        )
        .unwrap();
        assert!(production
            .headers()
            .get("Cloudflare-Workers-Version-Overrides")
            .is_none());
    }

    #[test]
    fn lab_v3_host_trace_marks_egress_without_changing_production_packets() {
        let lab_url = "wss://worker.example/lab-host-ws?run=123e4567-e89b-42d3-a456-426614174000&compat=wss-v3-host20-edge12-bench2-hevc-tiles-v14";
        assert!(lab_host_trace_enabled(lab_url, V3_FRAME_VERSION));
        assert!(!lab_host_trace_enabled(lab_url, FRAME_VERSION));
        assert!(!lab_host_trace_enabled(
            "wss://worker.example/host-ws",
            V3_FRAME_VERSION
        ));

        let mut packet = vec![3, 1, 0, 2, 9, 8, 7];
        append_lab_host_trace(
            &mut packet,
            1_787_940_000_123.0,
            Some(LabRoiTrace {
                reason: LabRoiReason::Applied,
                changed_ctbs: 9,
                total_ctbs: 2_040,
            }),
        );
        assert_eq!(&packet[..7], &[3, 1, 0, 2, 9, 8, 7]);
        assert_eq!(packet.len(), 27);
        assert_eq!(
            u32::from_be_bytes(packet[7..11].try_into().unwrap()),
            LAB_HOST_TRACE_MAGIC
        );
        assert_eq!(
            f64::from_be_bytes(packet[11..19].try_into().unwrap()),
            1_787_940_000_123.0
        );
        assert_eq!(packet[19], LAB_HOST_TRACE_VERSION);
        assert_eq!(packet[20], LabRoiReason::Applied as u8);
        assert_eq!(u16::from_be_bytes(packet[21..23].try_into().unwrap()), 9);
        assert_eq!(
            u16::from_be_bytes(packet[23..25].try_into().unwrap()),
            2_040
        );
    }

    #[test]
    fn reserves_at_least_three_lanes_or_skips_before_encode() {
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..STRIPE_COUNT)
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        let c = Coordinator {
            lanes,
            stopped: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
            game_mode: AtomicBool::new(false),
            topology: WssTopology::V2,
            required_shards: DATA_SHARDS,
            stats: Arc::new(MediaWindowStats::default()),
        };
        let now = Instant::now();
        let first = c.reserve_lanes(meta(true, 7, 10, 1), now).unwrap();
        assert_eq!(first.len(), 4);
        for lane in first {
            c.lanes[lane].queued.store(false, Ordering::Release);
        }
        c.lanes[0].set_state(LaneRecoveryState::Unavailable);
        assert_eq!(
            c.reserve_lanes(meta(false, 7, 10, 2), now).unwrap().len(),
            3
        );
    }

    #[test]
    fn protocol_v3_coordinator_uses_the_negotiated_data_quorum() {
        let topology = WssTopology::new(9, 8, 1).unwrap();
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..topology.lane_count())
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        let coordinator = Coordinator {
            lanes: lanes.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
            game_mode: AtomicBool::new(false),
            topology,
            required_shards: topology.data_shards(),
            stats: Arc::new(MediaWindowStats::default()),
        };
        lanes[0].set_state(LaneRecoveryState::Unavailable);
        let reserved = coordinator
            .reserve_lanes(meta(false, 1, 1, 1), Instant::now())
            .unwrap();
        assert_eq!(reserved.len(), 8);
        for lane in reserved {
            lanes[lane].queued.store(false, Ordering::Release);
        }
        lanes[1].set_state(LaneRecoveryState::Unavailable);
        assert_eq!(
            coordinator.reserve_lanes(meta(false, 1, 1, 2), Instant::now()),
            Err(ReserveOutcome::Backpressured)
        );
    }

    #[test]
    fn transiently_busy_lane_preserves_parity_budget_before_encode() {
        let topology = WssTopology::new(16, 15, 1).unwrap();
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..topology.lane_count())
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        let coordinator = Coordinator {
            lanes: lanes.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
            game_mode: AtomicBool::new(false),
            topology,
            required_shards: topology.data_shards(),
            stats: Arc::new(MediaWindowStats::default()),
        };
        let (tx, rx) = mpsc::sync_channel(LANE_QUEUE_CAPACITY);
        lanes[0].queued.store(true, Ordering::Release);
        tx.send(QueuedAccessUnit {
            meta: meta(false, 1, 1, 1),
            packet: vec![1],
            enqueued_at: Instant::now(),
        })
        .unwrap();
        lanes[1].queued.store(true, Ordering::Release);

        let in_flight = receive_queued_access_unit(
            &rx,
            &lanes[0],
            &coordinator.stats,
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(in_flight.meta.sequence, 1);
        assert!(!lanes[0].queued.load(Ordering::Acquire));

        assert_eq!(
            coordinator.reserve_lanes(meta(false, 1, 1, 2), Instant::now()),
            Err(ReserveOutcome::Backpressured)
        );
        lanes[1].set_state(LaneRecoveryState::Unavailable);
        let degraded = coordinator
            .reserve_lanes(meta(false, 1, 1, 2), Instant::now())
            .unwrap();
        assert_eq!(degraded.len(), topology.data_shards());
        assert_eq!(
            coordinator.reserve_lanes(meta(false, 1, 1, 3), Instant::now()),
            Err(ReserveOutcome::Backpressured)
        );
    }

    #[test]
    fn window_stats_classify_submit_work_and_queued_lane_backpressure() {
        let topology = WssTopology::new(4, 3, 1).unwrap();
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..topology.lane_count())
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        let stats = Arc::new(MediaWindowStats::default());
        let coordinator = Arc::new(Coordinator {
            lanes: lanes.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
            game_mode: AtomicBool::new(false),
            topology,
            required_shards: topology.data_shards(),
            stats: stats.clone(),
        });
        let mut senders = Vec::new();
        let mut receivers = Vec::new();
        for _ in 0..topology.lane_count() {
            let (tx, rx) = mpsc::sync_channel(LANE_QUEUE_CAPACITY);
            senders.push(tx);
            receivers.push(rx);
        }
        let egress = WssMediaEgress {
            coordinator,
            senders,
            joins: Vec::new(),
            protocol: V3_FRAME_VERSION,
            topology,
        };

        let payload = vec![7u8; 1_024];
        assert_eq!(
            egress
                .reserve(meta(false, 1, 1, 1))
                .unwrap()
                .submit(payload.clone()),
            SubmitOutcome::Queued { stripe: 0xff }
        );
        let submitted = egress.take_window_stats();
        assert_eq!(submitted.submit_count, 1);
        assert_eq!(submitted.access_unit_bytes, payload.len() as u64);
        assert!(submitted.packet_bytes >= payload.len() as u64);
        assert_eq!(submitted.reserve_backpressured, 0);

        for (index, receiver) in receivers.iter().enumerate() {
            receive_queued_access_unit(receiver, &lanes[index], &stats, Duration::from_millis(1))
                .unwrap();
        }
        let _ = egress.take_window_stats();
        for lane in lanes.iter().take(2) {
            lane.queued.store(true, Ordering::Release);
        }
        assert!(matches!(
            egress.reserve(meta(false, 1, 1, 2)),
            Err(ReserveOutcome::Backpressured)
        ));
        let blocked = egress.take_window_stats();
        assert_eq!(blocked.submit_count, 0);
        assert_eq!(blocked.reserve_backpressured, 1);
        assert_eq!(blocked.blocked_queued_lanes, 2);
        assert_eq!(egress.take_window_stats(), WssMediaWindowStats::default());

        drop(receivers);
    }

    #[test]
    fn dispatch_moves_selected_packet_allocations_into_writer_queues() {
        let lanes: Arc<Vec<Arc<LaneState>>> =
            Arc::new((0..4).map(|_| Arc::new(LaneState::new())).collect());
        let stats = MediaWindowStats::default();
        let mut senders = Vec::new();
        let mut receivers = Vec::new();
        for _ in 0..4 {
            let (tx, rx) = mpsc::sync_channel(LANE_QUEUE_CAPACITY);
            senders.push(tx);
            receivers.push(rx);
        }
        let packets = vec![vec![1u8; 7], vec![2u8; 11], vec![3u8; 13], vec![4u8; 17]];
        let pointers = packets
            .iter()
            .map(|packet| packet.as_ptr())
            .collect::<Vec<_>>();

        assert_eq!(
            dispatch_packets(
                &[0, 2, 3],
                meta(false, 1, 1, 9),
                V3_FRAME_VERSION,
                3,
                &lanes,
                &senders,
                &stats,
                packets,
            ),
            3
        );
        for stripe in [0usize, 2, 3] {
            let queued = receivers[stripe].recv().unwrap();
            assert_eq!(queued.packet.as_ptr(), pointers[stripe]);
        }
        assert!(matches!(
            receivers[1].try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn dispatch_order_stays_fixed_for_both_protocol_versions() {
        assert_eq!(
            dispatch_order(&[0, 1, 2, 3], FRAME_VERSION, 4, 3).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            dispatch_order(&[0, 1, 2, 3], V3_FRAME_VERSION, 1, 3).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            dispatch_order(&[0, 2, 3], V3_FRAME_VERSION, 1, 3).collect::<Vec<_>>(),
            vec![0, 2, 3]
        );
        assert_eq!(
            dispatch_order(&[3], V3_FRAME_VERSION, 1, 3).collect::<Vec<_>>(),
            vec![3]
        );
        assert!(dispatch_order(&[], V3_FRAME_VERSION, 1, 3)
            .collect::<Vec<_>>()
            .is_empty());
    }

    #[test]
    fn saturated_or_stalled_lane_requires_a_fresh_idr() {
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..STRIPE_COUNT)
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        for lane in lanes.iter() {
            lane.queued.store(true, Ordering::Release);
        }
        let c = Coordinator {
            lanes: lanes.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
            game_mode: AtomicBool::new(false),
            topology: WssTopology::V2,
            required_shards: DATA_SHARDS,
            stats: Arc::new(MediaWindowStats::default()),
        };
        assert_eq!(
            c.reserve_lanes(meta(true, 1, 1, 1), Instant::now()),
            Err(ReserveOutcome::Backpressured)
        );
        for lane in lanes.iter().take(3) {
            lane.queued.store(false, Ordering::Release);
        }
        lanes[0].last_sent_generation.store(1, Ordering::Release);
        lanes[0].last_sent_sequence.store(1, Ordering::Release);
        c.expire_stalled(Instant::now() + WORK_ACK_STALL_LIMIT);
        assert_eq!(lanes[0].state(), LaneRecoveryState::NeedsRecoveryIdr);
        assert!(
            c.recovery_snapshot_status(Instant::now() + WORK_ACK_STALL_LIMIT)
                .needs_idr
        );
        assert_eq!(
            c.reserve_lanes(meta(false, 1, 1, 2), Instant::now()),
            Err(ReserveOutcome::RequireKeyframe)
        );
        lanes[3].set_state(LaneRecoveryState::Unavailable);
        assert_eq!(
            c.reserve_lanes(meta(true, 1, 2, 3), Instant::now())
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn recovery_idr_bypasses_the_normal_ack_window_for_the_returning_lane() {
        let started = Instant::now();
        let topology = WssTopology::new(6, 5, 1).unwrap();
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..topology.lane_count())
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        let returning = &lanes[0];
        returning.last_sent_generation.store(1, Ordering::Release);
        returning.last_sent_sequence.store(20, Ordering::Release);
        returning.last_ack_generation.store(1, Ordering::Release);
        returning.last_ack_sequence.store(11, Ordering::Release);
        returning.set_state(LaneRecoveryState::NeedsRecoveryIdr);
        let coordinator = Coordinator {
            lanes,
            stopped: Arc::new(AtomicBool::new(false)),
            started,
            game_mode: AtomicBool::new(false),
            topology,
            required_shards: topology.data_shards(),
            stats: Arc::new(MediaWindowStats::default()),
        };

        let reserved = coordinator
            .reserve_lanes(meta(true, 1, 2, 21), started + Duration::from_millis(10))
            .unwrap();

        assert_eq!(reserved.len(), topology.lane_count());
        assert!(reserved.contains(&0));
    }

    #[test]
    fn explicit_stripe_reconnect_rearms_waiting_lane_before_periodic_probe() {
        let started = Instant::now();
        let topology = WssTopology::new(16, 15, 1).unwrap();
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..topology.lane_count())
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        let returning = lanes[14].clone();
        returning.last_sent_generation.store(7, Ordering::Release);
        returning.last_sent_sequence.store(602, Ordering::Release);
        returning.recovery_generation.store(7, Ordering::Release);
        returning.recovery_sequence.store(602, Ordering::Release);
        returning
            .recovery_sent_ms
            .store(started.elapsed().as_millis() as u64, Ordering::Release);
        returning.set_state(LaneRecoveryState::AwaitingRecoveryAck);
        let coordinator = Coordinator {
            lanes,
            stopped: Arc::new(AtomicBool::new(false)),
            started,
            game_mode: AtomicBool::new(false),
            topology,
            required_shards: topology.data_shards(),
            stats: Arc::new(MediaWindowStats::default()),
        };

        assert_eq!(coordinator.rearm_awaiting_recovery_lanes(), 1);
        assert_eq!(returning.state(), LaneRecoveryState::NeedsRecoveryIdr);
        let reserved = coordinator
            .reserve_lanes(meta(true, 7, 43, 603), started + Duration::from_millis(100))
            .unwrap();
        assert_eq!(reserved.len(), topology.lane_count());
        assert!(reserved.contains(&14));
    }

    #[test]
    fn unacknowledged_recovery_probe_requires_a_fresh_transport_before_retry() {
        let started = Instant::now();
        let topology = WssTopology::new(16, 15, 1).unwrap();
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..topology.lane_count())
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        for stripe in [0usize, 12] {
            let lane = &lanes[stripe];
            lane.last_sent_generation.store(3, Ordering::Release);
            lane.last_sent_sequence.store(1, Ordering::Release);
            lane.recovery_generation.store(3, Ordering::Release);
            lane.recovery_sequence.store(1, Ordering::Release);
            lane.set_state(LaneRecoveryState::AwaitingRecoveryAck);
        }
        let coordinator = Coordinator {
            lanes: lanes.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            started,
            game_mode: AtomicBool::new(false),
            topology,
            required_shards: topology.data_shards(),
            stats: Arc::new(MediaWindowStats::default()),
        };

        coordinator.expire_stalled(started + WORK_RECOVERY_PROBE_INTERVAL);

        assert_eq!(lanes[0].state(), LaneRecoveryState::Unavailable);
        assert_eq!(lanes[12].state(), LaneRecoveryState::Unavailable);
        assert_eq!(
            coordinator.reserve_lanes(meta(true, 3, 2, 2), started + WORK_RECOVERY_PROBE_INTERVAL,),
            Err(ReserveOutcome::Backpressured),
        );

        // A successful writer reconnect is the only event that makes these
        // lanes eligible for the next recovery IDR.
        assert!(lanes[0].take_transport_reconnect_request());
        assert!(lanes[12].take_transport_reconnect_request());
        lanes[0].set_state(LaneRecoveryState::NeedsRecoveryIdr);
        lanes[12].set_state(LaneRecoveryState::NeedsRecoveryIdr);
        let reserved = coordinator
            .reserve_lanes(meta(true, 3, 2, 2), started + WORK_RECOVERY_PROBE_INTERVAL)
            .unwrap();
        assert_eq!(reserved.len(), topology.lane_count());
    }

    #[test]
    fn continuous_ack_progress_does_not_false_positive_as_a_stalled_lane() {
        let started = Instant::now();
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..STRIPE_COUNT)
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        let c = Coordinator {
            lanes: lanes.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            started,
            game_mode: AtomicBool::new(false),
            topology: WssTopology::V2,
            required_shards: DATA_SHARDS,
            stats: Arc::new(MediaWindowStats::default()),
        };
        lanes[0].last_sent_sequence.store(10, Ordering::Release);
        c.on_ack(
            LaneAck {
                stripe: 0,
                generation: 1,
                gop: 1,
                sequence: 9,
            },
            started + Duration::from_millis(1_100),
        );
        c.expire_stalled(started + Duration::from_millis(1_500));
        assert_eq!(lanes[0].state(), LaneRecoveryState::Ready);
    }

    #[test]
    fn progressing_acks_cannot_allow_an_unbounded_socket_backlog() {
        let started = Instant::now();
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..STRIPE_COUNT)
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        let c = Coordinator {
            lanes: lanes.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            started,
            game_mode: AtomicBool::new(false),
            topology: WssTopology::V2,
            required_shards: DATA_SHARDS,
            stats: Arc::new(MediaWindowStats::default()),
        };
        for lane in lanes.iter() {
            lane.last_sent_generation.store(1, Ordering::Release);
            lane.last_sent_sequence.store(20, Ordering::Release);
            lane.last_ack_generation.store(1, Ordering::Release);
            lane.last_ack_sequence.store(11, Ordering::Release);
        }

        assert_eq!(
            c.reserve_lanes(meta(false, 1, 1, 21), started + Duration::from_millis(10)),
            Err(ReserveOutcome::Backpressured)
        );
    }

    #[test]
    fn stale_future_ack_from_a_previous_egress_cannot_poison_a_fresh_sequence() {
        let started = Instant::now();
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..STRIPE_COUNT)
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        let c = Coordinator {
            lanes: lanes.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            started,
            game_mode: AtomicBool::new(false),
            topology: WssTopology::V2,
            required_shards: DATA_SHARDS,
            stats: Arc::new(MediaWindowStats::default()),
        };
        let lane = &lanes[0];
        lane.last_sent_generation.store(1, Ordering::Release);
        lane.last_sent_sequence.store(3, Ordering::Release);
        lane.set_state(LaneRecoveryState::AwaitingRecoveryAck);
        lane.recovery_generation.store(1, Ordering::Release);
        lane.recovery_sequence.store(3, Ordering::Release);

        c.on_ack(
            LaneAck {
                stripe: 0,
                generation: 1,
                gop: 1,
                sequence: 77,
            },
            started + Duration::from_millis(10),
        );
        assert_eq!(lane.last_ack_sequence.load(Ordering::Acquire), 0);
        assert_eq!(lane.state(), LaneRecoveryState::AwaitingRecoveryAck);

        c.on_ack(
            LaneAck {
                stripe: 0,
                generation: 1,
                gop: 1,
                sequence: 3,
            },
            started + Duration::from_millis(20),
        );
        assert_eq!(lane.last_ack_sequence.load(Ordering::Acquire), 3);
        assert_eq!(lane.state(), LaneRecoveryState::Ready);
    }

    #[test]
    fn trailing_ack_for_recovery_frame_clears_gate_even_after_newer_sends() {
        let started = Instant::now();
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..STRIPE_COUNT)
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        let c = Coordinator {
            lanes: lanes.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            started,
            game_mode: AtomicBool::new(false),
            topology: WssTopology::V2,
            required_shards: DATA_SHARDS,
            stats: Arc::new(MediaWindowStats::default()),
        };
        let lane = &lanes[0];
        lane.set_state(LaneRecoveryState::AwaitingRecoveryAck);
        lane.recovery_generation.store(1, Ordering::Release);
        lane.recovery_sequence.store(11, Ordering::Release);
        // Sequence 11 is the recovery IDR. Frames produced after it make the
        // browser's batched ACK trail the newest send, which is normal.
        lane.last_sent_sequence.store(20, Ordering::Release);
        lane.last_sent_generation.store(1, Ordering::Release);
        c.on_ack(
            LaneAck {
                stripe: 0,
                generation: 1,
                gop: 2,
                sequence: 11,
            },
            started + Duration::from_millis(100),
        );
        assert_eq!(lane.state(), LaneRecoveryState::Ready);
    }

    #[test]
    fn lanes_waiting_for_recovery_ack_backpressure_instead_of_requesting_more_idrs() {
        let started = Instant::now();
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..STRIPE_COUNT)
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        for lane in lanes.iter().take(3) {
            lane.set_state(LaneRecoveryState::AwaitingRecoveryAck);
            lane.recovery_generation.store(4, Ordering::Release);
            lane.recovery_sequence.store(9, Ordering::Release);
        }
        let c = Coordinator {
            lanes,
            stopped: Arc::new(AtomicBool::new(false)),
            started,
            game_mode: AtomicBool::new(false),
            topology: WssTopology::V2,
            required_shards: DATA_SHARDS,
            stats: Arc::new(MediaWindowStats::default()),
        };
        assert_eq!(
            c.reserve_lanes(meta(false, 4, 2, 10), started + Duration::from_millis(10)),
            Err(ReserveOutcome::Backpressured)
        );
        assert_eq!(
            c.reserve_lanes(meta(true, 4, 3, 11), started + Duration::from_millis(10)),
            Err(ReserveOutcome::Backpressured)
        );
    }

    #[test]
    fn older_generation_ack_cannot_clear_current_recovery_and_sequence_wrap_is_safe() {
        let started = Instant::now();
        let lanes: Arc<Vec<Arc<LaneState>>> = Arc::new(
            (0..STRIPE_COUNT)
                .map(|_| Arc::new(LaneState::new()))
                .collect(),
        );
        let c = Coordinator {
            lanes: lanes.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            started,
            game_mode: AtomicBool::new(false),
            topology: WssTopology::V2,
            required_shards: DATA_SHARDS,
            stats: Arc::new(MediaWindowStats::default()),
        };
        let lane = &lanes[0];
        lane.last_sent_generation.store(8, Ordering::Release);
        lane.last_sent_sequence.store(1, Ordering::Release);
        lane.set_state(LaneRecoveryState::AwaitingRecoveryAck);
        lane.recovery_generation.store(8, Ordering::Release);
        lane.recovery_sequence
            .store(u32::MAX as u64, Ordering::Release);
        c.on_ack(
            LaneAck {
                stripe: 0,
                generation: 7,
                gop: 1,
                sequence: 1,
            },
            started,
        );
        assert_eq!(lane.state(), LaneRecoveryState::AwaitingRecoveryAck);
        c.on_ack(
            LaneAck {
                stripe: 0,
                generation: 8,
                gop: 1,
                sequence: 0,
            },
            started,
        );
        assert_eq!(lane.state(), LaneRecoveryState::Ready);
    }
}
