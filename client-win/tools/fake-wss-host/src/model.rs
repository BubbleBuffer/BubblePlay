//! The host side of one WSS v4 session as pure logic: when a frame may go out,
//! which access unit it is, how sequence numbers and generations advance, and
//! the deterministic injections. No sockets and no wall clock here (the caller
//! passes `now`), so every rule is unit-tested below.
//!
//! The flow rules mirror `host/src/wss_v4.rs` (`SendPolicy`): at most
//! `INFLIGHT_MAX_FRAMES` un-ACKed frames, a `INFLIGHT_BUDGET` of airtime in
//! bytes, cumulative ACK retirement, and a 3 s stall that forgets in-flight
//! frames without asking for a keyframe. A held frame is not encoded or sent:
//! the recording's access units wait, so no reference is ever skipped.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::protocol::{encode_frame, FrameHeader};
use crate::stream::{Codec, Stream};

pub const INFLIGHT_MAX_FRAMES: usize = 4;
pub const INFLIGHT_BUDGET: Duration = Duration::from_millis(150);
pub const ACK_STALL_LIMIT: Duration = Duration::from_secs(3);
/// After a seq-gap or generation injection, how long to wait for the
/// receiver's keyframe request before seeking to the next IDR anyway.
pub const KEYFRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// What the model needs from a recording. `Stream` implements it; the tests
/// use a synthetic source so they do not need the spike files.
pub trait Source: Send + Sync {
    fn codec(&self) -> Codec;
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn len(&self) -> usize;
    fn is_key(&self, index: usize) -> bool;
    fn has_param_sets(&self, index: usize) -> bool;
    fn next_key_at_or_after(&self, index: usize) -> usize;
    fn payload(&self, index: usize) -> Vec<u8>;
}

impl Source for Stream {
    fn codec(&self) -> Codec {
        self.codec
    }
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn len(&self) -> usize {
        Stream::len(self)
    }
    fn is_key(&self, index: usize) -> bool {
        Stream::is_key(self, index)
    }
    fn has_param_sets(&self, index: usize) -> bool {
        self.units[index].has_param_sets
    }
    fn next_key_at_or_after(&self, index: usize) -> usize {
        Stream::next_key_at_or_after(self, index)
    }
    fn payload(&self, index: usize) -> Vec<u8> {
        Stream::payload(self, index)
    }
}

fn seq_reached(candidate: u32, target: u32) -> bool {
    candidate.wrapping_sub(target) < 0x8000_0000
}

#[derive(Debug, Clone, Copy)]
struct InFlight {
    generation: u32,
    seq: u32,
    bytes: u64,
    sent_at: Instant,
}

/// ACK-based flow control for one media socket.
#[derive(Debug, Default)]
pub struct Flow {
    inflight: VecDeque<InFlight>,
    bytes: u64,
    last_progress: Option<Instant>,
}

/// Result of one ACK: frames retired (0 for a duplicate or a stale ACK) and
/// the RTT of the frame that the ACK named exactly, when it was in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckOutcome {
    pub retired: usize,
    pub exact_rtt: Option<Duration>,
}

impl Flow {
    pub fn budget_bytes(bitrate_bps: u64) -> u64 {
        (u128::from(bitrate_bps) * INFLIGHT_BUDGET.as_millis() / 8_000) as u64
    }

    pub fn over_budget(&self, bitrate_bps: u64) -> bool {
        self.inflight.len() >= INFLIGHT_MAX_FRAMES
            || (!self.inflight.is_empty() && self.bytes > Self::budget_bytes(bitrate_bps))
    }

    pub fn in_flight(&self) -> usize {
        self.inflight.len()
    }

    pub fn in_flight_bytes(&self) -> u64 {
        self.bytes
    }

    pub fn on_sent(&mut self, now: Instant, generation: u32, seq: u32, bytes: u64) {
        if self.inflight.is_empty() {
            self.last_progress = Some(now);
        }
        self.inflight.push_back(InFlight {
            generation,
            seq,
            bytes,
            sent_at: now,
        });
        self.bytes += bytes;
    }

    /// `None` when no in-flight frame carries this generation (unknown or
    /// long-retired). Otherwise retires every frame up to and including `seq`.
    pub fn on_ack(&mut self, now: Instant, generation: u32, seq: u32) -> Option<AckOutcome> {
        if !self
            .inflight
            .iter()
            .any(|frame| frame.generation == generation)
        {
            return None;
        }
        let mut retired = 0;
        let mut exact_rtt = None;
        while let Some(front) = self.inflight.front().copied() {
            if front.generation == generation && !seq_reached(seq, front.seq) {
                break;
            }
            self.inflight.pop_front();
            self.bytes -= front.bytes;
            self.last_progress = Some(now);
            retired += 1;
            if front.generation == generation && front.seq == seq {
                exact_rtt = Some(now.saturating_duration_since(front.sent_at));
            }
        }
        Some(AckOutcome { retired, exact_rtt })
    }

    /// Forget in-flight frames after `ACK_STALL_LIMIT` without ACK progress.
    /// Returns how many were forgotten. No keyframe is implied.
    pub fn expire(&mut self, now: Instant) -> usize {
        let stalled = match self.last_progress {
            Some(last) => now.saturating_duration_since(last) >= ACK_STALL_LIMIT,
            None => false,
        };
        if stalled && !self.inflight.is_empty() {
            let forgotten = self.clear();
            self.last_progress = Some(now);
            return forgotten;
        }
        0
    }

    pub fn clear(&mut self) -> usize {
        let count = self.inflight.len();
        self.inflight.clear();
        self.bytes = 0;
        count
    }
}

/// The deterministic injections. Each fires once per run, at the first
/// eligible frame at or after its offset from the run start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Injections {
    /// Skip one sequence number on a delta frame (the receiver sees a gap).
    pub seq_gap_at: Option<Duration>,
    /// Bump the generation on a delta frame without a keyframe.
    pub generation_at: Option<Duration>,
    /// Close the signalling and media sockets; the next session is a new epoch.
    pub socket_drop_at: Option<Duration>,
}

#[derive(Clone)]
pub struct ModelConfig {
    /// Every recording this run may serve. `start` picks one by codec.
    pub sources: Vec<Arc<dyn Source>>,
    pub fps: u32,
    pub bitrate_bps: u64,
    pub injections: Injections,
    pub run_start: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Injection {
    SeqGap,
    GenerationChange,
    SocketDrop,
}

impl Injection {
    pub fn name(self) -> &'static str {
        match self {
            Injection::SeqGap => "seq-gap",
            Injection::GenerationChange => "generation-change",
            Injection::SocketDrop => "socket-drop",
        }
    }
}

/// One injection and how the session recovered from it. `recovery_ms` runs
/// from the injection to the ACK of the first keyframe that follows it.
#[derive(Debug, Clone, PartialEq)]
pub struct AckTrace {
    pub generation: u32,
    pub seq: u32,
    pub received_ms: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Recovery {
    pub kind: Injection,
    pub fired_ms: f64,
    pub keyframe_request: Option<String>,
    pub keyframe_requested_ms: Option<f64>,
    pub idr: Option<(u32, u32)>,
    pub idr_sent_ms: Option<f64>,
    pub recovered_ms: Option<f64>,
    /// Bounded ACK evidence lets the runner match the frame actually drawn,
    /// even when the latest-frame mailbox skips the recovery IDR itself.
    pub acknowledgements: Vec<AckTrace>,
}

impl Recovery {
    pub fn recovery_ms(&self) -> Option<f64> {
        self.recovered_ms.map(|done| done - self.fired_ms)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Counters {
    pub frames_sent: u64,
    pub keyframes_sent: u64,
    pub delta_frames_sent: u64,
    pub backpressure_holds: u64,
    pub keyframe_seeks: u64,
    pub keyframe_timeouts: u64,
    pub loops: u64,
    pub parameter_sets_injected: u64,
    pub max_inflight: usize,
    pub acks_received: u64,
    pub frames_retired_by_ack: u64,
    pub acks_duplicate_or_stale: u64,
    pub acks_unknown_generation: u64,
    pub frames_forgotten_on_stall: u64,
    pub frames_lost_to_disconnect: u64,
    pub keyframe_requests: BTreeMap<String, u64>,
    pub starts: u64,
    /// Codec of the last accepted start, as the wire names it.
    pub negotiated_codec: Option<String>,
    pub stops: u64,
    pub codec_mismatches: u64,
    pub media_attaches: u64,
    pub media_detaches: u64,
    pub epochs_issued: u64,
    pub socket_drops: u64,
    pub seq_gaps_injected: u64,
    pub generation_changes_injected: u64,
    pub first_frame_requests: u64,
    pub first_frame_presented: u64,
    pub media_pings: u64,
    pub unhandled_control: BTreeMap<String, u64>,
    pub rejected_control: u64,
    pub stale_epoch_control: u64,
    pub outbound_overflow: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StartOutcome {
    Started,
    AlreadyStreaming,
    CodecMismatch,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AckResult {
    Retired(usize),
    Duplicate,
    UnknownGeneration,
}

#[derive(Debug)]
pub struct Emitted {
    pub packet: Vec<u8>,
    pub key: bool,
    pub generation: u32,
    pub seq: u32,
    pub index: usize,
}

#[derive(Debug)]
pub enum Action {
    Idle,
    Held,
    Emit(Emitted),
    /// The sockets must be closed now (the socket-drop injection).
    SocketDrop,
}

pub struct Model {
    library: Vec<Arc<dyn Source>>,
    active: usize,
    fps: u32,
    bitrate_bps: u64,
    injections: Injections,
    run_start: Instant,
    flow: Flow,
    streaming: bool,
    media_attached: bool,
    draining: bool,
    need_key: bool,
    index: usize,
    generation: u32,
    seq: u32,
    fired: BTreeMap<Injection, bool>,
    keyframe_due: Option<Instant>,
    pub recoveries: Vec<Recovery>,
    pub counters: Counters,
}

impl Model {
    pub fn new(config: ModelConfig) -> Self {
        let library = config.sources;
        assert!(
            !library.is_empty(),
            "the model needs at least one recording"
        );
        Self {
            library,
            active: 0,
            fps: config.fps,
            bitrate_bps: config.bitrate_bps,
            injections: config.injections,
            run_start: config.run_start,
            flow: Flow::default(),
            streaming: false,
            media_attached: false,
            draining: false,
            need_key: true,
            index: 0,
            generation: 0,
            seq: 1,
            fired: BTreeMap::new(),
            keyframe_due: None,
            recoveries: Vec::new(),
            counters: Counters::default(),
        }
    }

    /// The recording that is currently served.
    fn current(&self) -> &dyn Source {
        self.library[self.active].as_ref()
    }

    pub fn frame_period(&self) -> Duration {
        Duration::from_nanos(1_000_000_000 / u64::from(self.fps.max(1)))
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    pub fn in_flight(&self) -> usize {
        self.flow.in_flight()
    }

    fn ms(&self, now: Instant) -> f64 {
        now.saturating_duration_since(self.run_start).as_secs_f64() * 1000.0
    }

    fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.run_start)
    }

    /// A new signalling epoch: the previous session's frames are gone, the
    /// generation advances, and the next start begins at a keyframe.
    pub fn begin_epoch(&mut self, _now: Instant) {
        self.counters.epochs_issued += 1;
        self.counters.frames_lost_to_disconnect += self.flow.clear() as u64;
        self.generation = self.generation.wrapping_add(1);
        self.streaming = false;
        self.need_key = true;
    }

    /// Enable the recording the browser names. With no codec, the active one
    /// stays. A codec with no recording in the library is a mismatch.
    pub fn start(&mut self, codec_value: Option<&str>) -> StartOutcome {
        let chosen = match codec_value {
            None => self.active,
            Some(value) => match self
                .library
                .iter()
                .position(|s| s.codec().matches_start_value(value))
            {
                Some(index) => index,
                None => {
                    self.counters.codec_mismatches += 1;
                    return StartOutcome::CodecMismatch;
                }
            },
        };
        if self.streaming && chosen == self.active {
            return StartOutcome::AlreadyStreaming;
        }
        if chosen != self.active {
            // Frames of the other recording must not be ACKed against this one.
            self.counters.frames_lost_to_disconnect += self.flow.clear() as u64;
            self.active = chosen;
            self.index = 0;
        }
        self.counters.starts += 1;
        self.streaming = true;
        self.need_key = true;
        self.counters.negotiated_codec = Some(self.current().codec().wire_name().to_owned());
        StartOutcome::Started
    }

    pub fn stop(&mut self) {
        if self.streaming {
            self.counters.stops += 1;
        }
        self.streaming = false;
        // Stop production while preserving frames already sent on the separate
        // media socket. Their ACKs may arrive after this signaling control.
        // Actual detach/epoch replacement accounts for any unreceived tail.
    }

    /// The receiver's media socket is up. Its decoder chain is new, so the
    /// next frame has to be a keyframe.
    pub fn media_attach(&mut self) {
        self.counters.media_attaches += 1;
        self.media_attached = true;
        self.need_key = true;
        self.counters.frames_lost_to_disconnect += self.flow.clear() as u64;
    }

    pub fn media_detach(&mut self) {
        if self.media_attached {
            self.counters.media_detaches += 1;
        }
        self.media_attached = false;
        self.counters.frames_lost_to_disconnect += self.flow.clear() as u64;
    }

    pub fn keyframe_request(&mut self, reason: &str, now: Instant) {
        *self
            .counters
            .keyframe_requests
            .entry(reason.to_owned())
            .or_insert(0) += 1;
        self.keyframe_due = None;
        if self.streaming {
            self.need_key = true;
        }
        let at = self.ms(now);
        for recovery in self
            .recoveries
            .iter_mut()
            .filter(|r| r.recovered_ms.is_none())
        {
            if recovery.keyframe_request.is_none() {
                recovery.keyframe_request = Some(reason.to_owned());
                recovery.keyframe_requested_ms = Some(at);
            }
        }
    }

    pub fn ack(&mut self, now: Instant, generation: u32, seq: u32) -> AckResult {
        self.counters.acks_received += 1;
        let result = match self.flow.on_ack(now, generation, seq) {
            None if generation == self.generation => {
                // The first ACK may empty the queue before its replay arrives.
                // It is still a duplicate in the current generation.
                self.counters.acks_duplicate_or_stale += 1;
                AckResult::Duplicate
            }
            None => {
                self.counters.acks_unknown_generation += 1;
                AckResult::UnknownGeneration
            }
            Some(AckOutcome { retired: 0, .. }) => {
                self.counters.acks_duplicate_or_stale += 1;
                AckResult::Duplicate
            }
            Some(AckOutcome { retired, .. }) => {
                self.counters.frames_retired_by_ack += retired as u64;
                AckResult::Retired(retired)
            }
        };
        let at = self.ms(now);
        for recovery in &mut self.recoveries {
            if at - recovery.fired_ms <= 2500.0 && recovery.acknowledgements.len() < 160 {
                recovery.acknowledgements.push(AckTrace {
                    generation,
                    seq,
                    received_ms: at,
                });
            }
        }
        for recovery in self
            .recoveries
            .iter_mut()
            .filter(|r| r.recovered_ms.is_none())
        {
            if let Some((idr_generation, idr_seq)) = recovery.idr {
                // Cumulative: an ACK at or past the keyframe's seq covers it.
                if idr_generation == generation && seq_reached(seq, idr_seq) {
                    recovery.recovered_ms = Some(at);
                }
            }
        }
        result
    }

    /// Stop starting new frames; in-flight frames are still ACKed and expired.
    pub fn set_draining(&mut self) {
        self.draining = true;
    }

    pub fn tick(&mut self, now: Instant) -> Action {
        self.counters.frames_forgotten_on_stall += self.flow.expire(now) as u64;
        if self.draining || !self.streaming || !self.media_attached {
            return Action::Idle;
        }
        let elapsed = self.elapsed(now);
        if let Some(at) = self.injections.socket_drop_at {
            if elapsed >= at && !self.fired.contains_key(&Injection::SocketDrop) {
                self.fired.insert(Injection::SocketDrop, true);
                self.counters.socket_drops += 1;
                self.counters.frames_lost_to_disconnect += self.flow.clear() as u64;
                self.open_recovery(Injection::SocketDrop, now);
                self.streaming = false;
                self.media_attached = false;
                return Action::SocketDrop;
            }
        }
        if let Some(due) = self.keyframe_due {
            if now.saturating_duration_since(due) >= KEYFRAME_TIMEOUT {
                self.keyframe_due = None;
                self.need_key = true;
                self.counters.keyframe_timeouts += 1;
            }
        }
        if self.flow.over_budget(self.bitrate_bps) {
            self.counters.backpressure_holds += 1;
            return Action::Held;
        }
        if self.need_key && !self.current().is_key(self.index) {
            self.index = self.current().next_key_at_or_after(self.index);
            self.counters.keyframe_seeks += 1;
        }
        let key = self.current().is_key(self.index);

        // The injections need a delta frame: a keyframe would hide them.
        let mut seq_gap = false;
        let mut generation_change = false;
        if !key {
            if let Some(at) = self.injections.seq_gap_at {
                if elapsed >= at && !self.fired.contains_key(&Injection::SeqGap) {
                    self.fired.insert(Injection::SeqGap, true);
                    seq_gap = true;
                }
            }
            if let Some(at) = self.injections.generation_at {
                if elapsed >= at && !self.fired.contains_key(&Injection::GenerationChange) {
                    self.fired.insert(Injection::GenerationChange, true);
                    generation_change = true;
                }
            }
        }

        let mut seq = self.seq;
        if seq_gap {
            seq = seq.wrapping_add(1);
            self.counters.seq_gaps_injected += 1;
            self.open_recovery(Injection::SeqGap, now);
            self.keyframe_due.get_or_insert(now);
        }
        if generation_change {
            self.generation = self.generation.wrapping_add(1);
            self.counters.generation_changes_injected += 1;
            self.open_recovery(Injection::GenerationChange, now);
            self.keyframe_due.get_or_insert(now);
        }
        self.seq = seq.wrapping_add(1);
        let generation = self.generation;

        let payload = self.current().payload(self.index);
        if key && !self.current().has_param_sets(self.index) {
            self.counters.parameter_sets_injected += 1;
        }
        let header = FrameHeader {
            key,
            refine: false,
            codec: self.current().codec().bpv4(),
            generation,
            seq,
            capture_ts_us: elapsed.as_micros() as u64,
            width: self.current().width() as u16,
            height: self.current().height() as u16,
        };
        let packet = match encode_frame(&header, &payload) {
            Ok(packet) => packet,
            Err(_) => return Action::Idle,
        };
        self.flow
            .on_sent(now, generation, seq, payload.len() as u64);
        self.counters.max_inflight = self.counters.max_inflight.max(self.flow.in_flight());
        self.counters.frames_sent += 1;
        if key {
            self.counters.keyframes_sent += 1;
            self.need_key = false;
            let at = self.ms(now);
            for recovery in self.recoveries.iter_mut().filter(|r| r.idr.is_none()) {
                recovery.idr = Some((generation, seq));
                recovery.idr_sent_ms = Some(at);
            }
        } else {
            self.counters.delta_frames_sent += 1;
        }

        let index = self.index;
        self.index += 1;
        if self.index == self.current().len() {
            // Unit 0 is an IDR, so this wrap always lands on a keyframe.
            self.index = 0;
            self.counters.loops += 1;
        }
        Action::Emit(Emitted {
            packet,
            key,
            generation,
            seq,
            index,
        })
    }

    fn open_recovery(&mut self, kind: Injection, now: Instant) {
        self.recoveries.push(Recovery {
            kind,
            fired_ms: self.ms(now),
            keyframe_request: None,
            keyframe_requested_ms: None,
            idr: None,
            idr_sent_ms: None,
            recovered_ms: None,
            acknowledgements: Vec::new(),
        });
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Access units without real NALs: key every `gop`, one byte payloads.
    pub struct Synthetic {
        pub codec: Codec,
        pub len: usize,
        pub gop: usize,
        pub param_sets_missing: bool,
    }

    impl Source for Synthetic {
        fn codec(&self) -> Codec {
            self.codec
        }
        fn width(&self) -> u32 {
            1920
        }
        fn height(&self) -> u32 {
            1080
        }
        fn len(&self) -> usize {
            self.len
        }
        fn is_key(&self, index: usize) -> bool {
            index % self.gop == 0
        }
        fn has_param_sets(&self, index: usize) -> bool {
            !(self.param_sets_missing && self.is_key(index))
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
            // The payload marks its own index so tests can check ordering.
            let mut out = vec![0, 0, 0, 1, 0x09, 0xf0];
            out.extend_from_slice(&(index as u32).to_be_bytes());
            out
        }
    }

    fn model(len: usize, gop: usize, injections: Injections) -> (Model, Instant) {
        let now = Instant::now();
        let source: Arc<dyn Source> = Arc::new(Synthetic {
            codec: Codec::H264,
            len,
            gop,
            param_sets_missing: false,
        });
        let config = ModelConfig {
            sources: vec![source],
            fps: 60,
            bitrate_bps: 20_000_000,
            injections,
            run_start: now,
        };
        (Model::new(config), now)
    }

    fn emitted(action: Action) -> Emitted {
        match action {
            Action::Emit(e) => e,
            other => panic!("expected an emitted frame, got {other:?}"),
        }
    }

    fn ready(model: &mut Model, now: Instant) {
        model.begin_epoch(now);
        assert_eq!(model.start(Some("h264")), StartOutcome::Started);
        model.media_attach();
    }

    #[test]
    fn nothing_is_sent_before_start_or_media_attach() {
        let (mut model, now) = model(10, 5, Injections::default());
        assert!(matches!(model.tick(now), Action::Idle));
        model.begin_epoch(now);
        model.start(None);
        assert!(
            matches!(model.tick(now), Action::Idle),
            "no media socket yet"
        );
        model.media_attach();
        assert_eq!(
            emitted(model.tick(now)).key,
            true,
            "the first frame of a session is an IDR"
        );
    }

    #[test]
    fn flow_cap_holds_the_fifth_frame_until_an_ack_retires_one() {
        let (mut model, now) = model(100, 100, Injections::default());
        ready(&mut model, now);
        let mut seqs = Vec::new();
        for _ in 0..4 {
            seqs.push(emitted(model.tick(now)).seq);
        }
        assert!(
            matches!(model.tick(now), Action::Held),
            "fifth frame waits for an ACK"
        );
        assert_eq!(model.in_flight(), 4);
        assert_eq!(
            model.ack(now, model.generation(), seqs[0]),
            AckResult::Retired(1)
        );
        let fifth = emitted(model.tick(now));
        assert_eq!(
            fifth.seq,
            seqs[3] + 1,
            "held frames are not skipped: the next access unit goes next"
        );
        assert_eq!(model.counters.max_inflight, 4);
    }

    #[test]
    fn byte_budget_holds_frames_when_airtime_exceeds_150_ms() {
        // 150 ms at 20 Mb/s is 375 000 bytes. One 400 000-byte IDR fills it.
        let source = Arc::new(Synthetic {
            codec: Codec::H264,
            len: 50,
            gop: 50,
            param_sets_missing: false,
        });
        struct Big(Arc<Synthetic>);
        impl Source for Big {
            fn codec(&self) -> Codec {
                self.0.codec()
            }
            fn width(&self) -> u32 {
                self.0.width()
            }
            fn height(&self) -> u32 {
                self.0.height()
            }
            fn len(&self) -> usize {
                self.0.len()
            }
            fn is_key(&self, i: usize) -> bool {
                self.0.is_key(i)
            }
            fn has_param_sets(&self, i: usize) -> bool {
                self.0.has_param_sets(i)
            }
            fn next_key_at_or_after(&self, i: usize) -> usize {
                self.0.next_key_at_or_after(i)
            }
            fn payload(&self, _i: usize) -> Vec<u8> {
                vec![0u8; 400_000]
            }
        }
        assert_eq!(Flow::budget_bytes(20_000_000), 375_000);
        let now = Instant::now();
        let big: Arc<dyn Source> = Arc::new(Big(source));
        let config = ModelConfig {
            sources: vec![big],
            fps: 60,
            bitrate_bps: 20_000_000,
            injections: Injections::default(),
            run_start: now,
        };
        let mut model = Model::new(config);
        ready(&mut model, now);
        emitted(model.tick(now));
        assert!(
            matches!(model.tick(now), Action::Held),
            "one frame over the byte budget blocks the next"
        );
    }

    #[test]
    fn acks_retire_cumulatively_and_ignore_duplicates_and_unknown_generations() {
        let (mut model, now) = model(100, 100, Injections::default());
        ready(&mut model, now);
        let generation = model.generation();
        let seqs: Vec<u32> = (0..3).map(|_| emitted(model.tick(now)).seq).collect();
        assert_eq!(
            model.ack(now, generation, seqs[1]),
            AckResult::Retired(2),
            "one ACK covers the earlier frame too"
        );
        assert_eq!(
            model.ack(now, generation, seqs[1]),
            AckResult::Duplicate,
            "a repeated ACK retires nothing"
        );
        assert_eq!(
            model.ack(now, generation, seqs[0]),
            AckResult::Duplicate,
            "an older ACK is stale"
        );
        assert_eq!(
            model.ack(now, generation + 9, seqs[2]),
            AckResult::UnknownGeneration
        );
        assert_eq!(model.in_flight(), 1);
        assert_eq!(model.counters.frames_retired_by_ack, 2);
        assert_eq!(model.ack(now, generation, seqs[2]), AckResult::Retired(1));
        assert_eq!(model.in_flight(), 0);
        assert_eq!(model.ack(now, generation, seqs[2]), AckResult::Duplicate);
        assert_eq!(model.counters.frames_retired_by_ack, 3);
        assert_eq!(model.counters.acks_duplicate_or_stale, 3);
        assert_eq!(model.counters.acks_unknown_generation, 1);
    }

    #[test]
    fn sequence_numbers_wrap_without_losing_retirement() {
        let mut flow = Flow::default();
        let now = Instant::now();
        flow.on_sent(now, 1, u32::MAX - 1, 10);
        flow.on_sent(now, 1, u32::MAX, 10);
        flow.on_sent(now, 1, 0, 10);
        assert_eq!(flow.on_ack(now, 1, 0).unwrap().retired, 3);
        assert_eq!(flow.in_flight(), 0);
    }

    #[test]
    fn a_stall_forgets_frames_after_three_seconds_and_asks_for_no_keyframe() {
        let (mut model, now) = model(100, 100, Injections::default());
        ready(&mut model, now);
        for _ in 0..4 {
            emitted(model.tick(now));
        }
        let later = now + Duration::from_millis(2_999);
        model.tick(later);
        assert_eq!(
            model.in_flight(),
            4,
            "unacked frames remain until three seconds"
        );
        let resumed = emitted(model.tick(now + Duration::from_secs(3)));
        assert!(!resumed.key, "expiry resumes with the next delta");
        assert_eq!(
            model.in_flight(),
            1,
            "expired frames are gone; the newly emitted frame remains"
        );
        assert_eq!(model.counters.frames_forgotten_on_stall, 4);
        assert!(
            !model.need_key,
            "the receiver decides whether it needs a keyframe"
        );
    }

    #[test]
    fn keyframe_request_seeks_to_the_next_idr_and_the_loop_lands_on_idr_zero() {
        let (mut model, now) = model(10, 4, Injections::default());
        ready(&mut model, now);
        let first = emitted(model.tick(now));
        assert_eq!((first.index, first.key), (0, true));
        let second = emitted(model.tick(now));
        assert_eq!((second.index, second.key), (1, false));
        model.ack(now, model.generation(), second.seq);
        model.keyframe_request("sequence-gap", now);
        let idr = emitted(model.tick(now));
        assert_eq!(idr.index, 4, "the next IDR after unit 2 is unit 4");
        assert!(idr.key);
        assert_eq!(model.counters.keyframe_seeks, 1);
        // Walk to the end: the wrap must land on unit 0, an IDR.
        let mut last = idr;
        for _ in 0..20 {
            model.ack(now, model.generation(), last.seq);
            last = emitted(model.tick(now));
            if last.index == 0 {
                assert!(last.key);
                break;
            }
        }
        assert_eq!(last.index, 0);
        assert!(model.counters.loops >= 1);
    }

    #[test]
    fn keyframes_without_parameter_sets_are_counted_when_the_payload_is_completed() {
        let source: Arc<dyn Source> = Arc::new(Synthetic {
            codec: Codec::H264,
            len: 4,
            gop: 4,
            param_sets_missing: true,
        });
        let now = Instant::now();
        let config = ModelConfig {
            sources: vec![source],
            fps: 60,
            bitrate_bps: 20_000_000,
            injections: Injections::default(),
            run_start: now,
        };
        let mut model = Model::new(config);
        ready(&mut model, now);
        let idr = emitted(model.tick(now));
        assert_eq!(model.counters.parameter_sets_injected, 1);
        assert!(idr.packet.len() > 28);
    }

    #[test]
    fn seq_gap_skips_one_number_once_and_recovers_on_the_next_idr() {
        let injections = Injections {
            seq_gap_at: Some(Duration::from_millis(100)),
            ..Injections::default()
        };
        let (mut model, now) = model(120, 120, injections);
        ready(&mut model, now);
        let mut frames = vec![emitted(model.tick(now))];
        let mut t = now;
        for _ in 0..20 {
            t += Duration::from_millis(17);
            if let Action::Emit(e) = model.tick(t) {
                model.ack(t, e.generation, e.seq);
                frames.push(e);
            }
        }
        let gap = frames
            .windows(2)
            .find(|w| w[1].seq != w[0].seq + 1)
            .expect("one gap");
        assert_eq!(gap[1].seq, gap[0].seq + 2);
        assert_eq!(model.counters.seq_gaps_injected, 1, "once per run");
        assert_eq!(model.recoveries.len(), 1);
        assert_eq!(model.recoveries[0].kind, Injection::SeqGap);
        // The receiver asks, the next IDR (unit 120) comes out, and the ACK closes the recovery.
        model.keyframe_request("sequence-gap", t);
        let mut idr = None;
        for _ in 0..400 {
            t += Duration::from_millis(17);
            if let Action::Emit(e) = model.tick(t) {
                if e.key {
                    idr = Some(e);
                    break;
                }
                model.ack(t, e.generation, e.seq);
            }
        }
        let idr = idr.expect("the receiver's keyframe request brings an IDR");
        model.ack(t, idr.generation, idr.seq);
        let recovery = &model.recoveries[0];
        assert_eq!(recovery.keyframe_request.as_deref(), Some("sequence-gap"));
        assert!(recovery.recovery_ms().unwrap() > 0.0);
    }

    #[test]
    fn generation_change_bumps_once_on_a_delta_and_stays_bumped() {
        let injections = Injections {
            generation_at: Some(Duration::ZERO),
            ..Injections::default()
        };
        let (mut model, now) = model(50, 50, injections);
        ready(&mut model, now);
        let key = emitted(model.tick(now));
        model.ack(now, key.generation, key.seq);
        let delta = emitted(model.tick(now));
        assert_eq!(delta.generation, key.generation + 1);
        assert_eq!(model.counters.generation_changes_injected, 1);
        let after = emitted(model.tick(now));
        assert_eq!(
            after.generation, delta.generation,
            "the new generation carries on"
        );
    }

    #[test]
    fn recovery_keeps_bounded_ack_evidence_for_frames_after_the_idr() {
        let injections = Injections {
            seq_gap_at: Some(Duration::ZERO),
            ..Injections::default()
        };
        let (mut model, now) = model(120, 60, injections);
        ready(&mut model, now);
        let key = emitted(model.tick(now));
        model.ack(now, key.generation, key.seq);
        let gap = emitted(model.tick(now));
        model.ack(now, gap.generation, gap.seq);
        model.keyframe_request("sequence-gap", now);
        let idr = emitted(model.tick(now));
        model.ack(now + Duration::from_millis(20), idr.generation, idr.seq);
        let delta = emitted(model.tick(now));
        model.ack(now + Duration::from_millis(40), delta.generation, delta.seq);
        let trace = &model.recoveries[0].acknowledgements;
        assert!(trace
            .iter()
            .any(|ack| ack.seq == delta.seq && ack.received_ms == 40.0));
        for _ in 0..200 {
            model.ack(now + Duration::from_millis(50), delta.generation, delta.seq);
        }
        assert_eq!(model.recoveries[0].acknowledgements.len(), 160);
        model.ack(now + Duration::from_secs(3), delta.generation, delta.seq);
        assert_eq!(model.recoveries[0].acknowledgements.len(), 160);
    }

    #[test]
    fn socket_drop_is_reported_once_and_the_next_epoch_restarts_at_a_keyframe() {
        let injections = Injections {
            socket_drop_at: Some(Duration::from_millis(50)),
            ..Injections::default()
        };
        let (mut model, now) = model(50, 50, injections);
        ready(&mut model, now);
        let first = emitted(model.tick(now));
        assert!(first.key);
        let t = now + Duration::from_millis(60);
        assert!(matches!(model.tick(t), Action::SocketDrop));
        assert_eq!(model.in_flight(), 0);
        assert_eq!(model.counters.socket_drops, 1);
        assert_eq!(model.counters.frames_lost_to_disconnect, 1);
        assert!(
            matches!(model.tick(t), Action::Idle),
            "not streaming until the new epoch starts"
        );
        let old_generation = model.generation();
        model.begin_epoch(t);
        assert_eq!(model.generation(), old_generation + 1);
        model.media_attach();
        model.start(Some("h264"));
        let restart = emitted(model.tick(t));
        assert!(restart.key);
        model.ack(t, restart.generation, restart.seq);
        assert!(model.recoveries[0].recovery_ms().is_some());
        assert!(matches!(
            model.tick(t + Duration::from_millis(1)),
            Action::Held | Action::Emit(_)
        ));
        assert_eq!(model.counters.socket_drops, 1, "never a second drop");
    }

    #[test]
    fn stop_retains_inflight_frames_until_their_late_acks_arrive() {
        let (mut model, now) = model(50, 50, Injections::default());
        ready(&mut model, now);
        let frame = emitted(model.tick(now));
        model.stop();
        assert!(matches!(model.tick(now), Action::Idle));
        assert_eq!(model.counters.frames_lost_to_disconnect, 0);
        assert_eq!(
            model.ack(now, frame.generation, frame.seq),
            AckResult::Retired(1)
        );
        assert_eq!(model.counters.frames_retired_by_ack, 1);
        model.media_detach();
        assert_eq!(model.counters.frames_lost_to_disconnect, 0);
    }

    #[test]
    fn draining_stops_new_frames_but_leaves_acks_working() {
        let (mut model, now) = model(50, 50, Injections::default());
        ready(&mut model, now);
        let frame = emitted(model.tick(now));
        model.set_draining();
        assert!(matches!(model.tick(now), Action::Idle));
        assert_eq!(
            model.ack(now, frame.generation, frame.seq),
            AckResult::Retired(1)
        );
    }

    #[test]
    fn start_selects_the_recording_the_browser_names() {
        let now = Instant::now();
        let h264: Arc<dyn Source> = Arc::new(Synthetic {
            codec: Codec::H264,
            len: 10,
            gop: 10,
            param_sets_missing: false,
        });
        let hevc: Arc<dyn Source> = Arc::new(Synthetic {
            codec: Codec::Hevc,
            len: 10,
            gop: 10,
            param_sets_missing: false,
        });
        let config = ModelConfig {
            sources: vec![h264, hevc],
            fps: 60,
            bitrate_bps: 20_000_000,
            injections: Injections::default(),
            run_start: now,
        };
        let mut model = Model::new(config);
        model.begin_epoch(now);
        assert_eq!(model.start(Some("hevc")), StartOutcome::Started);
        assert_eq!(model.counters.negotiated_codec.as_deref(), Some("hevc"));
        model.media_attach();
        let frame = emitted(model.tick(now));
        assert!(frame.key);
        assert_eq!(
            frame.packet[6],
            Codec::Hevc.bpv4(),
            "the BPV4 header names the negotiated codec"
        );

        // A second start for the same codec while streaming changes nothing.
        assert_eq!(model.start(Some("hevc")), StartOutcome::AlreadyStreaming);
        // Switching while streaming serves the other recording from its start.
        assert_eq!(model.start(Some("h264")), StartOutcome::Started);
        assert_eq!(model.counters.negotiated_codec.as_deref(), Some("h264"));
        let frame = emitted(model.tick(now));
        assert_eq!(frame.packet[6], Codec::H264.bpv4());
        assert_eq!(frame.index, 0);
        assert_eq!(model.start(Some("av1")), StartOutcome::CodecMismatch);
        assert_eq!(model.counters.codec_mismatches, 1);
    }

    #[test]
    fn codec_mismatch_is_refused_and_counted() {
        let (mut model, now) = model(10, 10, Injections::default());
        model.begin_epoch(now);
        assert_eq!(model.start(Some("hevc")), StartOutcome::CodecMismatch);
        assert_eq!(
            model.start(Some("hev1.2.4.L120.B0")),
            StartOutcome::CodecMismatch
        );
        assert_eq!(model.start(Some("avc1.64002A")), StartOutcome::Started);
        assert_eq!(model.counters.codec_mismatches, 2);
    }
}
