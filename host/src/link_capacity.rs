//! BBR-style link capacity estimate from WSS v4 ACKs, and the per-frame byte
//! budget derived from it.
//!
//! Every ACK of a frame still in flight yields a delivery-rate sample, as in
//! BBR: bytes delivered since the frame's reference point over
//! `max(send_elapsed, ack_elapsed)`. A frame sent into an empty pipe restarts
//! the reference at its own send time; its ACK interval then includes one
//! propagation delay, so the minimum RTT is taken off it.
//!
//! Delivery rates are lower bounds of the capacity, so the estimate is a
//! windowed maximum. Samples taken while the sender is app-limited (the usual
//! case on a desktop stream) only say "at least this much". A link becomes
//! constrained once ACKs show a queue: several samples within a short hold
//! whose RTT exceeds the minimum by `QUEUE_EVIDENCE` and whose delivery was
//! slower than the sending, arriving in at least two separate ACK bursts
//! spread over at least `EVIDENCE_MIN_SPAN` (a receiver that stalls and then
//! ACKs its backlog is one short flush, not a slow link; in the lab a 570 ms
//! stall on an unthrottled link flushed in more than one 8 ms burst). The
//! queue must also be growing: the RTT inflation of the last evidence sample
//! exceeds the first's by `QUEUE_GROWTH_MIN`. A sender that outruns the link
//! adds a queue that deepens for as long as it does, whereas a stalled or
//! jittery path shows a flat or draining inflation, and its deliveries
//! (diluted by the stall) say nothing about the link. An unconstrained link
//! has no estimate and so no budget (no behaviour change).
//!
//! As in BBR, a frame sent while less than one BDP (estimate x min RTT) was
//! in flight marks the samples up to its delivery as app-limited: the pipe
//! had room, so a slow delivery or a longer RTT says nothing about the link.
//! App-limited samples are never queue evidence and never lower the
//! estimate. Without this, a trickle of small frames with RTT jitter walks a
//! constrained estimate down to the trickle's own rate.
//!
//! A constrained estimate is raised by any faster delivery and re-measured by
//! fresh queue evidence. Evidence must be re-confirmed within
//! `CONFIRM_TIMEOUT`; otherwise the link is no longer constrained (no budget,
//! no pacing) and the remaining estimate only caps the bitrate while it is
//! probed up by 25 % every `PROBE_STEP`, so the encoder climbs back instead of
//! jumping into the queue it just left. A probe that reaches the profile
//! maximum is released by the owner ([`CapacityEstimator::release`]).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Delivery-rate samples older than this leave the max filter.
pub const CAPACITY_WINDOW: Duration = Duration::from_secs(2);
/// Minimum-RTT window.
pub const MIN_RTT_WINDOW: Duration = Duration::from_secs(10);
/// A sample shows a queue when its frame's RTT exceeds the minimum by this.
pub const QUEUE_EVIDENCE: Duration = Duration::from_millis(20);
/// Queue evidence stays valid this long.
pub const CONSTRAINED_HOLD: Duration = Duration::from_secs(2);
/// Queue-evidence samples needed within the hold (one jitter spike is not a
/// constrained link).
pub const CONSTRAINED_MIN_SAMPLES: usize = 3;
/// ...spread over at least this many ACK bursts.
pub const CONSTRAINED_MIN_BURSTS: usize = 2;
/// The queue must deepen by this much, first to last evidence sample.
pub const QUEUE_GROWTH_MIN: Duration = Duration::from_millis(20);
/// Samples kept for the `link constrained` log line.
const TRACE_SAMPLES: usize = 48;
/// Samples written to the log line.
const TRACE_LOGGED: usize = 16;
/// ACKs closer than this belong to one burst.
pub const ACK_BURST_GAP: Duration = Duration::from_millis(8);
/// ...and persist at least this long, first to last sample.
pub const EVIDENCE_MIN_SPAN: Duration = Duration::from_millis(250);
/// A constrained link whose queue evidence is this old is released.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);
/// Shorter sample intervals are dominated by timer and ACK jitter.
pub const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(8);
/// Smaller deliveries cannot show a queue reliably.
pub const MIN_SAMPLE_BYTES: u64 = 4_096;
/// Per-frame budget = capacity x frame interval x 80 %.
pub const BUDGET_UTILIZATION_PCT: u64 = 80;
/// A released estimate is probed up one step this often.
pub const PROBE_STEP: Duration = Duration::from_secs(1);
/// Probe step, percent of the current estimate.
pub const PROBE_GAIN_PCT: u64 = 125;
/// A lone frame (sent into an empty pipe) must be this large to count as
/// queue evidence; smaller ones are dominated by RTT jitter.
pub const LONE_FRAME_EVIDENCE_BYTES: u64 = 4 * MIN_SAMPLE_BYTES;
const MAX_TRACKED_FRAMES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SentFrame {
    generation: u32,
    seq: u32,
    bytes: u64,
    sent_at: Instant,
    /// Bytes delivered when this frame was sent.
    delivered: u64,
    /// When `delivered` was last advanced (or the pipe restarted).
    delivered_time: Instant,
    /// Send time at the start of this frame's send interval.
    first_sent_time: Instant,
    /// Sent into an empty pipe: the reference point is a send, not an ACK.
    restart: bool,
    /// Sent while the pipe had room (less than one BDP in flight).
    app_limited: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateSample {
    pub bps: u64,
    pub rtt: Duration,
    /// The delivery was queue-limited (the link, not the sender, set it).
    pub link_limited: bool,
    /// The sender left the pipe room: the sample can only raise the estimate.
    pub app_limited: bool,
}

/// A constrained link's measured capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub bps: u64,
}

impl Capacity {
    /// Bitrate the encoder should follow: 80 % of the capacity.
    pub fn target_bps(self) -> u64 {
        self.bps * BUDGET_UTILIZATION_PCT / 100
    }

    /// Bytes one frame may use: capacity x interval x 80 %.
    pub fn frame_budget_bytes(self, frame_interval: Duration) -> u64 {
        (u128::from(self.target_bps()) * frame_interval.as_micros() / 8_000_000) as u64
    }
}

/// One delivery sample with the context the `link constrained` log carries.
#[derive(Debug, Clone, Copy)]
struct TraceSample {
    at: Instant,
    bps: u64,
    rtt: Duration,
    app_limited: bool,
    /// Host-side time since the previous ACK.
    ack_gap_ms: u64,
    /// The viewer's receive-time difference to the previous ACK, if reported.
    recv_gap_ms: Option<i64>,
}

#[derive(Debug)]
pub struct CapacityEstimator {
    sent: VecDeque<SentFrame>,
    delivered: u64,
    delivered_time: Instant,
    first_sent_time: Instant,
    min_rtt: VecDeque<(Instant, Duration)>,
    /// (time, bps, app-limited)
    samples: VecDeque<(Instant, u64, bool)>,
    /// (time, ACK burst, RTT above the minimum) of link-limited samples.
    queue_evidence: VecDeque<(Instant, u64, Duration)>,
    trace: VecDeque<TraceSample>,
    /// The viewer's receive time of the ACK about to be applied.
    pending_recv_ms: Option<f64>,
    last_recv_ms: Option<f64>,
    /// The estimate: binding while `constrained`, a probing cap otherwise.
    capacity_bps: Option<u64>,
    constrained: bool,
    /// Last qualifying queue evidence.
    last_evidence: Instant,
    /// Next probe step of a released estimate.
    next_probe: Instant,
    last_ack: Option<Instant>,
    burst: u64,
    /// Samples stay app-limited until `delivered` passes this (BBR's
    /// `app_limited` marker); 0 = not app-limited.
    app_limited_until: u64,
}

impl CapacityEstimator {
    pub fn new(now: Instant) -> Self {
        Self {
            sent: VecDeque::new(),
            delivered: 0,
            delivered_time: now,
            first_sent_time: now,
            min_rtt: VecDeque::new(),
            samples: VecDeque::new(),
            queue_evidence: VecDeque::new(),
            trace: VecDeque::new(),
            pending_recv_ms: None,
            last_recv_ms: None,
            capacity_bps: None,
            constrained: false,
            last_evidence: now,
            next_probe: now,
            last_ack: None,
            burst: 0,
            app_limited_until: 0,
        }
    }

    /// The receiver's decoder chain is gone (reconnect / new peer): frames in
    /// flight will never be ACKed. Measurements of the path are kept.
    pub fn forget_inflight(&mut self) {
        self.sent.clear();
    }

    /// The viewer's receive timestamp (ms, its own clock) of the ACK that is
    /// applied next; only its differences are used, for the log line.
    pub fn note_recv_ts(&mut self, recv_ts_ms: f64) {
        self.pending_recv_ms = Some(recv_ts_ms);
    }

    pub fn on_sent(&mut self, now: Instant, generation: u32, seq: u32, bytes: u64) {
        let restart = self.sent.is_empty();
        if restart {
            self.first_sent_time = now;
            self.delivered_time = now;
        }
        if self.sent.len() >= MAX_TRACKED_FRAMES {
            self.sent.pop_front();
        }
        let inflight = self.sent.iter().map(|frame| frame.bytes).sum::<u64>() + bytes;
        if let Some(bdp) = self.bdp_bytes(now) {
            if inflight < bdp {
                self.app_limited_until = (self.delivered + inflight).max(1);
            }
        }
        let app_limited = self.app_limited_until > self.delivered;
        self.sent.push_back(SentFrame {
            generation,
            seq,
            bytes,
            sent_at: now,
            delivered: self.delivered,
            delivered_time: self.delivered_time,
            first_sent_time: self.first_sent_time,
            restart,
            app_limited,
        });
    }

    /// Bytes one estimate (or, before a queue was seen, the best delivery
    /// so far) carries in one minimum RTT.
    fn bdp_bytes(&self, now: Instant) -> Option<u64> {
        let bps = self.capacity_bps.or_else(|| self.max_delivery_bps(now))?;
        let min_rtt = self.min_rtt()?;
        Some((u128::from(bps) * min_rtt.as_micros() / 8_000_000) as u64)
    }

    /// Retire every frame up to and including `seq` (cumulative, like the
    /// send policy) and sample the delivery rate at the exact frame.
    pub fn on_ack(&mut self, now: Instant, generation: u32, seq: u32) -> Option<RateSample> {
        if !self.sent.iter().any(|frame| frame.generation == generation) {
            return None;
        }
        let ack_gap_ms = self.last_ack.map_or(0, |last| {
            now.saturating_duration_since(last).as_millis() as u64
        });
        let recv_ms = self.pending_recv_ms.take();
        let recv_gap_ms = recv_ms
            .zip(self.last_recv_ms)
            .map(|(now_ms, last_ms)| (now_ms - last_ms).round() as i64);
        if recv_ms.is_some() {
            self.last_recv_ms = recv_ms;
        }
        if self
            .last_ack
            .is_none_or(|last| now.saturating_duration_since(last) > ACK_BURST_GAP)
        {
            self.burst += 1;
        }
        self.last_ack = Some(now);
        let mut acked = None;
        while let Some(front) = self.sent.front().copied() {
            if front.generation == generation && !seq_reached(seq, front.seq) {
                break;
            }
            self.sent.pop_front();
            self.delivered += front.bytes;
            if front.generation == generation && front.seq == seq {
                acked = Some(front);
            }
        }
        self.delivered_time = now;
        if self.app_limited_until != 0 && self.delivered > self.app_limited_until {
            self.app_limited_until = 0;
        }
        self.advance(now);
        let frame = acked?;
        self.first_sent_time = frame.sent_at;
        let rtt = now.saturating_duration_since(frame.sent_at);
        self.note_rtt(now, rtt);
        let min_rtt = self.min_rtt().unwrap_or(rtt);
        let send_elapsed = frame
            .sent_at
            .saturating_duration_since(frame.first_sent_time);
        let mut ack_elapsed = now.saturating_duration_since(frame.delivered_time);
        if frame.restart {
            ack_elapsed = ack_elapsed.saturating_sub(min_rtt);
        }
        let interval = send_elapsed.max(ack_elapsed);
        if interval < MIN_SAMPLE_INTERVAL {
            return None;
        }
        let delivered = self.delivered - frame.delivered;
        let bps = (u128::from(delivered) * 8_000_000 / interval.as_micros().max(1)) as u64;
        let min_bytes = if frame.restart {
            LONE_FRAME_EVIDENCE_BYTES
        } else {
            MIN_SAMPLE_BYTES
        };
        let app_limited = frame.app_limited;
        let link_limited = !app_limited
            && delivered >= min_bytes
            && rtt >= min_rtt + QUEUE_EVIDENCE
            && ack_elapsed > send_elapsed;
        self.samples.push_back((now, bps, app_limited));
        if self.trace.len() >= TRACE_SAMPLES {
            self.trace.pop_front();
        }
        self.trace.push_back(TraceSample {
            at: now,
            bps,
            rtt,
            app_limited,
            ack_gap_ms,
            recv_gap_ms,
        });
        if link_limited {
            self.queue_evidence
                .push_back((now, self.burst, rtt.saturating_sub(min_rtt)));
        }
        self.expire(now);
        self.update_capacity(now, bps, link_limited);
        Some(RateSample {
            bps,
            rtt,
            link_limited,
            app_limited,
        })
    }

    /// Enough recent queue evidence, from more than one ACK burst and
    /// lasting longer than a stalled receiver's flush.
    fn evidence_qualifies(&self) -> bool {
        let mut bursts = self
            .queue_evidence
            .iter()
            .map(|(_, burst, _)| *burst)
            .collect::<Vec<_>>();
        bursts.dedup();
        self.queue_evidence.len() >= CONSTRAINED_MIN_SAMPLES
            && bursts.len() >= CONSTRAINED_MIN_BURSTS
            && self.evidence_span() >= EVIDENCE_MIN_SPAN
            && self.queue_growth() >= QUEUE_GROWTH_MIN
    }

    /// How much deeper the queue is at the last evidence sample than at the
    /// first (zero when it is shallower).
    fn queue_growth(&self) -> Duration {
        match (self.queue_evidence.front(), self.queue_evidence.back()) {
            (Some((_, _, first)), Some((_, _, last))) => last.saturating_sub(*first),
            _ => Duration::ZERO,
        }
    }

    /// The newest delivery samples, compactly, for the `link constrained`
    /// line: `age_ms/kbps/rtt_ms/A|L/ack_gap_ms/recv_gap_ms` (A app-limited,
    /// L pipe-full; recv gap `-` when the viewer sent none).
    fn trace_summary(&self, now: Instant) -> String {
        let skip = self.trace.len().saturating_sub(TRACE_LOGGED);
        self.trace
            .iter()
            .skip(skip)
            .map(|sample| {
                let recv = sample
                    .recv_gap_ms
                    .map_or_else(|| "-".to_owned(), |gap| gap.to_string());
                format!(
                    "{}/{}/{}/{}/{}/{}",
                    now.saturating_duration_since(sample.at).as_millis(),
                    sample.bps / 1_000,
                    sample.rtt.as_millis(),
                    if sample.app_limited { 'A' } else { 'L' },
                    sample.ack_gap_ms,
                    recv
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn evidence_span(&self) -> Duration {
        match (self.queue_evidence.front(), self.queue_evidence.back()) {
            (Some((first, _, _)), Some((last, _, _))) => last.saturating_duration_since(*first),
            _ => Duration::ZERO,
        }
    }

    fn update_capacity(&mut self, now: Instant, bps: u64, link_limited: bool) {
        if link_limited && self.evidence_qualifies() {
            // A queue: the window's best pipe-full delivery is the link.
            // App-limited samples only count where they are faster.
            let window_max = self
                .samples
                .iter()
                .filter(|(at, _, _)| now.saturating_duration_since(*at) <= CAPACITY_WINDOW)
                .filter(|(_, sample, app_limited)| {
                    !app_limited || self.capacity_bps.is_some_and(|capacity| *sample > capacity)
                })
                .map(|(_, sample, _)| *sample)
                .max()
                .unwrap_or(bps);
            self.capacity_bps = Some(window_max);
            if !self.constrained {
                let mut bursts = self
                    .queue_evidence
                    .iter()
                    .map(|(_, burst, _)| *burst)
                    .collect::<Vec<_>>();
                bursts.dedup();
                tracing::info!(
                    capacity_bps = window_max,
                    evidence = self.queue_evidence.len(),
                    bursts = bursts.len(),
                    span_ms = self.evidence_span().as_millis() as u64,
                    growth_ms = self.queue_growth().as_millis() as u64,
                    samples = %self.trace_summary(now),
                    "WSS v4 link constrained"
                );
            }
            self.constrained = true;
            self.last_evidence = now;
            return;
        }
        if let Some(capacity) = self.capacity_bps {
            self.capacity_bps = Some(capacity.max(bps));
        }
    }

    /// Time passes (every ACK, and the link's periodic tick): an unconfirmed
    /// constrained link is released and its estimate probed up.
    pub fn advance(&mut self, now: Instant) {
        self.expire(now);
        if self.constrained && now.saturating_duration_since(self.last_evidence) > CONFIRM_TIMEOUT {
            self.constrained = false;
            self.next_probe = now;
        }
        if self.constrained {
            return;
        }
        let Some(capacity) = self.capacity_bps else {
            return;
        };
        if now >= self.next_probe {
            self.capacity_bps = Some(capacity * PROBE_GAIN_PCT / 100);
            self.next_probe = now + PROBE_STEP;
        }
    }

    /// Drop a released estimate (its probe passed anything the sender would
    /// use). A constrained estimate is kept.
    pub fn release(&mut self) {
        if !self.constrained {
            self.capacity_bps = None;
        }
    }

    fn note_rtt(&mut self, now: Instant, rtt: Duration) {
        while self
            .min_rtt
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > MIN_RTT_WINDOW)
        {
            self.min_rtt.pop_front();
        }
        // Monotonic deque: the front is the window minimum.
        while self.min_rtt.back().is_some_and(|(_, old)| *old >= rtt) {
            self.min_rtt.pop_back();
        }
        self.min_rtt.push_back((now, rtt));
    }

    fn expire(&mut self, now: Instant) {
        while self
            .samples
            .front()
            .is_some_and(|(at, _, _)| now.saturating_duration_since(*at) > CAPACITY_WINDOW)
        {
            self.samples.pop_front();
        }
        while self
            .queue_evidence
            .front()
            .is_some_and(|(at, _, _)| now.saturating_duration_since(*at) > CONSTRAINED_HOLD)
        {
            self.queue_evidence.pop_front();
        }
    }

    pub fn min_rtt(&self) -> Option<Duration> {
        self.min_rtt.front().map(|(_, rtt)| *rtt)
    }

    /// Highest delivery rate in the window, constrained or not.
    pub fn max_delivery_bps(&self, now: Instant) -> Option<u64> {
        self.samples
            .iter()
            .filter(|(at, _, _)| now.saturating_duration_since(*at) <= CAPACITY_WINDOW)
            .map(|(_, bps, _)| *bps)
            .max()
    }

    pub fn is_constrained(&self) -> bool {
        self.constrained
    }

    /// The capacity of a constrained link; `None` unless the link has shown
    /// a queue within `CONFIRM_TIMEOUT` (unconstrained: no budget, no
    /// pacing, no behaviour change).
    pub fn capacity(&self) -> Option<Capacity> {
        self.capacity_bps
            .filter(|_| self.constrained)
            .map(|bps| Capacity { bps })
    }

    /// The estimate that caps the bitrate: the constrained capacity, or a
    /// released estimate while it is probed up.
    pub fn rate_cap(&self) -> Option<Capacity> {
        self.capacity_bps.map(|bps| Capacity { bps })
    }
}

fn seq_reached(candidate: u32, target: u32) -> bool {
    candidate.wrapping_sub(target) < 0x8000_0000
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_micros(33_333);

    /// A FIFO bottleneck of `bps` behind `prop` one-way-and-back delay.
    struct Link {
        bps: u64,
        prop: Duration,
        busy_until: Instant,
    }

    impl Link {
        fn deliver(&mut self, sent_at: Instant, bytes: u64) -> Instant {
            let start = self.busy_until.max(sent_at);
            self.busy_until = start + Duration::from_micros(bytes * 8_000_000 / self.bps);
            self.busy_until + self.prop
        }
    }

    /// Send `frames` frames of `bytes` every frame interval over `link` and
    /// ACK them in order; returns the estimator and the end time.
    fn run(link: &mut Link, frames: u32, bytes: u64) -> (CapacityEstimator, Instant) {
        run_with(link, frames, bytes, true)
    }

    /// As `run`; without `drain` it stops with the last frame sent, while the
    /// queue is still deep.
    fn run_with(
        link: &mut Link,
        frames: u32,
        bytes: u64,
        drain: bool,
    ) -> (CapacityEstimator, Instant) {
        let start = Instant::now();
        link.busy_until = start;
        let mut estimator = CapacityEstimator::new(start);
        let mut acks: VecDeque<(Instant, u32)> = VecDeque::new();
        let mut now = start;
        for seq in 0..frames {
            let sent_at = start + FRAME * seq;
            while acks.front().is_some_and(|(at, _)| *at <= sent_at) {
                let (at, acked) = acks.pop_front().unwrap();
                estimator.on_ack(at, 1, acked);
                now = at;
            }
            estimator.on_sent(sent_at, 1, seq, bytes);
            acks.push_back((link.deliver(sent_at, bytes), seq));
        }
        for (at, acked) in acks {
            if !drain {
                break;
            }
            estimator.on_ack(at, 1, acked);
            now = at;
        }
        (estimator, now)
    }

    #[test]
    fn app_limited_stream_on_a_fast_link_is_unconstrained() {
        // 6 Mbps of frames over a 100 Mbps link with 20 ms of delay.
        let mut link = Link {
            bps: 100_000_000,
            prop: Duration::from_millis(50),
            busy_until: Instant::now(),
        };
        let (estimator, now) = run(&mut link, 120, 25_000);
        assert!(!estimator.is_constrained());
        assert_eq!(estimator.capacity(), None);
        // The samples still say "at least the sending rate".
        assert!(estimator.max_delivery_bps(now).unwrap() >= 5_000_000);
    }

    #[test]
    fn queue_limited_stream_measures_the_bottleneck() {
        // 6 Mbps of frames into a slower bottleneck: the queue deepens with
        // every frame and the back-to-back ACKs measure the link. The second
        // case is the owner's relay path at 6 Mbps: a 700 kbps bottleneck must
        // still be found, with the rate cut to its capacity.
        for (bottleneck_bps, prop_ms, frames) in [(2_000_000, 20, 60), (700_000, 24, 60)] {
            let mut link = Link {
                bps: bottleneck_bps,
                prop: Duration::from_millis(prop_ms),
                busy_until: Instant::now(),
            };
            let (estimator, _) = run_with(&mut link, frames, 25_000, false);
            let capacity = estimator.capacity().expect("constrained");
            assert!(
                (bottleneck_bps * 9 / 10..=bottleneck_bps * 11 / 10).contains(&capacity.bps),
                "{bottleneck_bps}: {capacity:?}"
            );
            assert_eq!(capacity.target_bps(), capacity.bps * 8 / 10);
        }
    }

    #[test]
    fn lone_frames_subtract_the_propagation_delay() {
        // One 100 KB frame per second over 4 Mbps: each is a pipe restart
        // whose ACK is 200 ms serialization + 30 ms propagation.
        let start = Instant::now();
        let mut estimator = CapacityEstimator::new(start);
        let mut link = Link {
            bps: 4_000_000,
            prop: Duration::from_millis(30),
            busy_until: start,
        };
        // A small frame first establishes the propagation RTT.
        estimator.on_sent(start, 1, 0, 100);
        estimator.on_ack(link.deliver(start, 100), 1, 0);
        let mut last = None;
        for seq in 1..4u32 {
            let sent_at = start + Duration::from_secs(u64::from(seq));
            estimator.on_sent(sent_at, 1, seq, 100_000);
            let at = link.deliver(sent_at, 100_000);
            last = estimator.on_ack(at, 1, seq);
        }
        let sample = last.unwrap();
        assert!((3_800_000..=4_200_000).contains(&sample.bps), "{sample:?}");
        assert!(sample.link_limited);
    }

    #[test]
    fn one_jitter_spike_is_not_a_constrained_link() {
        let start = Instant::now();
        let mut estimator = CapacityEstimator::new(start);
        let mut now = start;
        for seq in 0..30u32 {
            now = start + FRAME * seq;
            estimator.on_sent(now, 1, seq, 20_000);
            let rtt = if seq == 10 { 60 } else { 10 };
            estimator.on_ack(now + Duration::from_millis(rtt), 1, seq);
        }
        let _ = now;
        assert!(!estimator.is_constrained());
    }

    #[test]
    fn an_unconfirmed_estimate_is_released_and_probed_up() {
        let mut link = Link {
            bps: 2_000_000,
            prop: Duration::from_millis(20),
            busy_until: Instant::now(),
        };
        let (mut estimator, last) = run(&mut link, 60, 25_000);
        let measured = estimator.capacity().unwrap().bps;
        // Within the confirmation timeout the link stays constrained.
        estimator.advance(last + Duration::from_secs(2));
        assert_eq!(estimator.capacity().unwrap().bps, measured);
        // Past it: no longer constrained, the estimate only caps the rate and
        // climbs 25 % per step.
        estimator.advance(last + Duration::from_millis(3_100));
        assert_eq!(estimator.capacity(), None);
        assert!(!estimator.is_constrained());
        let first = estimator.rate_cap().unwrap().bps;
        assert_eq!(first, measured * 125 / 100);
        estimator.advance(last + Duration::from_millis(3_600));
        assert_eq!(estimator.rate_cap().unwrap().bps, first);
        estimator.advance(last + Duration::from_millis(4_200));
        assert_eq!(estimator.rate_cap().unwrap().bps, first * 125 / 100);
        estimator.release();
        assert_eq!(estimator.rate_cap(), None);
    }

    #[test]
    fn fresh_queue_evidence_constrains_a_probing_link_again() {
        let mut link = Link {
            bps: 2_000_000,
            prop: Duration::from_millis(20),
            busy_until: Instant::now(),
        };
        let (mut estimator, last) = run(&mut link, 60, 25_000);
        let resume = last + Duration::from_secs(5);
        estimator.advance(resume);
        assert!(!estimator.is_constrained());
        // Overload again.
        link.busy_until = resume;
        let mut pending = VecDeque::new();
        for n in 0..60u32 {
            let at = resume + FRAME * n;
            while pending.front().is_some_and(|(ack, _)| *ack <= at) {
                let (ack, seq) = pending.pop_front().unwrap();
                estimator.on_ack(ack, 1, seq);
            }
            estimator.on_sent(at, 1, 500 + n, 25_000);
            pending.push_back((link.deliver(at, 25_000), 500 + n));
        }
        for (ack, seq) in pending {
            estimator.on_ack(ack, 1, seq);
        }
        let capacity = estimator.capacity().expect("constrained again");
        assert!(
            (1_800_000..=2_600_000).contains(&capacity.bps),
            "{capacity:?}"
        );
    }

    #[test]
    fn one_receiver_stall_is_not_a_constrained_link() {
        // A fast link whose receiver stalls for 500 ms and then ACKs its
        // whole backlog at once: slow deliveries with long RTTs, but all in
        // one ACK burst.
        let start = Instant::now();
        let mut estimator = CapacityEstimator::new(start);
        let mut seq = 0u32;
        for n in 0..30u32 {
            let at = start + FRAME * n;
            estimator.on_sent(at, 1, seq, 20_000);
            estimator.on_ack(at + Duration::from_millis(3), 1, seq);
            seq += 1;
        }
        let stall = start + FRAME * 30;
        let mut backlog = Vec::new();
        for n in 0..15u32 {
            let at = stall + FRAME * n;
            estimator.on_sent(at, 1, seq, 20_000);
            backlog.push(seq);
            seq += 1;
        }
        let release = stall + Duration::from_millis(520);
        for (n, acked) in backlog.into_iter().enumerate() {
            estimator.on_ack(release + Duration::from_micros(200 * n as u64), 1, acked);
        }
        assert!(!estimator.is_constrained());
        assert_eq!(estimator.capacity(), None);
    }

    #[test]
    fn a_stall_flushed_in_several_bursts_is_not_a_constrained_link() {
        // As on the unthrottled lab link: a 570 ms stall whose backlog comes
        // back in a few bursts over ~100 ms.
        let start = Instant::now();
        let mut estimator = CapacityEstimator::new(start);
        let mut seq = 0u32;
        for n in 0..30u32 {
            let at = start + FRAME * n;
            estimator.on_sent(at, 1, seq, 20_000);
            estimator.on_ack(at + Duration::from_millis(3), 1, seq);
            seq += 1;
        }
        let stall = start + FRAME * 30;
        let mut backlog = Vec::new();
        for n in 0..17u32 {
            estimator.on_sent(stall + FRAME * n, 1, seq, 20_000);
            backlog.push(seq);
            seq += 1;
        }
        let release = stall + Duration::from_millis(570);
        for (n, acked) in backlog.into_iter().enumerate() {
            let burst = Duration::from_millis(20 * (n as u64 / 4));
            let at = release + burst + Duration::from_micros(300 * n as u64);
            estimator.on_ack(at, 1, acked);
        }
        assert!(!estimator.is_constrained());
        assert_eq!(estimator.capacity(), None);
    }

    #[test]
    fn a_stall_flushed_over_300_ms_does_not_collapse_the_rate() {
        // Replayed from the owner's host log of 2026-11-11 13:15:10Z: 6 Mbps
        // (25 KB frames at 30 fps) over a path with a 48 ms minimum RTT, a
        // ~500 ms hiccup, and the backlog ACKed in four bursts over ~300 ms.
        // The log read "bursts 4, span 294 ms" and a capacity of 498 kbps, and
        // the rate fell from 5.5 Mbps to the 500 kbps floor. The backlog's
        // RTTs shrink burst by burst (the queue drains); a link slower than
        // the sender would show a queue that deepens.
        let start = Instant::now();
        let mut estimator = CapacityEstimator::new(start);
        let mut seq = 0u32;
        for n in 0..90u32 {
            let at = start + FRAME * n;
            estimator.on_sent(at, 1, seq, 25_000);
            estimator.on_ack(at + Duration::from_millis(48), 1, seq);
            seq += 1;
        }
        let stall = start + FRAME * 90;
        let mut backlog = Vec::new();
        for n in 0..15u32 {
            estimator.on_sent(stall + FRAME * n, 1, seq, 25_000);
            backlog.push(seq);
            seq += 1;
        }
        let release = stall + Duration::from_millis(520);
        for (n, acked) in backlog.into_iter().enumerate() {
            let burst = Duration::from_millis(90 * (n as u64 / 4));
            estimator.on_ack(
                release + burst + Duration::from_micros(300 * n as u64),
                1,
                acked,
            );
        }
        assert!(!estimator.is_constrained());
        assert_eq!(estimator.capacity(), None);
        assert_eq!(estimator.rate_cap(), None);
    }

    #[test]
    fn a_jittery_trickle_does_not_walk_the_estimate_down() {
        // Measured on the lab relay: after a 2 Mbps link was found, 30 fps of
        // 2.4 KB frames whose RTTs jitter by 25 ms were counted as queue
        // evidence and the estimate fell to the trickle's ~600 kbps.
        let mut link = Link {
            bps: 2_000_000,
            prop: Duration::from_millis(20),
            busy_until: Instant::now(),
        };
        let (mut estimator, now) = run(&mut link, 60, 25_000);
        let measured = estimator.capacity().unwrap().bps;
        let start = now + Duration::from_millis(100);
        let mut pending: Vec<(Instant, u32)> = Vec::new();
        for n in 0..300u32 {
            let at = start + FRAME * n;
            pending.sort();
            while pending.first().is_some_and(|(ack, _)| *ack <= at) {
                let (ack, seq) = pending.remove(0);
                estimator.on_ack(ack, 1, seq);
            }
            let seq = 1_000 + n;
            estimator.on_sent(at, 1, seq, 2_400);
            let jitter = if n % 20 < 8 { 25 } else { 0 };
            pending.push((link.deliver(at, 2_400) + Duration::from_millis(jitter), seq));
        }
        pending.sort();
        for (ack, seq) in pending {
            estimator.on_ack(ack, 1, seq);
        }
        // Never lowered (it was released after the confirmation timeout and
        // only probed up since).
        let held = estimator.rate_cap().unwrap().bps;
        assert!(held >= measured, "{held} vs {measured}");
    }

    #[test]
    fn faster_delivery_raises_a_held_estimate() {
        let mut link = Link {
            bps: 2_000_000,
            prop: Duration::from_millis(20),
            busy_until: Instant::now(),
        };
        let (mut estimator, now) = run(&mut link, 60, 25_000);
        let measured = estimator.capacity().unwrap().bps;
        // One large lone frame over a now-faster link measures it directly.
        let at = now + Duration::from_secs(1);
        estimator.on_sent(at, 1, 1_000, 200_000);
        let min_rtt = estimator.min_rtt().unwrap();
        estimator.on_ack(at + min_rtt + Duration::from_millis(160), 1, 1_000);
        assert!(
            estimator.capacity().unwrap().bps > measured * 4,
            "{:?}",
            estimator.capacity()
        );
    }

    #[test]
    fn frame_budget_is_eighty_percent_of_capacity_per_interval() {
        let capacity = Capacity { bps: 3_000_000 };
        // 3 Mbps x 1/30 s x 0.8 = 10 000 bytes.
        assert_eq!(
            capacity.frame_budget_bytes(Duration::from_micros(33_333)),
            9_999
        );
        assert_eq!(
            capacity.frame_budget_bytes(Duration::from_millis(100)),
            30_000
        );
    }

    #[test]
    fn late_or_unknown_acks_are_ignored_and_forget_clears_inflight() {
        let start = Instant::now();
        let mut estimator = CapacityEstimator::new(start);
        estimator.on_sent(start, 1, 5, 10_000);
        assert_eq!(estimator.on_ack(start + FRAME, 2, 5), None);
        estimator.forget_inflight();
        assert_eq!(estimator.on_ack(start + FRAME, 1, 5), None);
    }
}
