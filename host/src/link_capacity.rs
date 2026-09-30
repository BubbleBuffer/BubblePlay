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
//! slower than the sending. An unconstrained link has no estimate and so no
//! budget (no behaviour change).
//!
//! As in BBR, a frame sent while less than one BDP (estimate x min RTT) was
//! in flight marks the samples up to its delivery as app-limited: the pipe
//! had room, so a slow delivery or a longer RTT says nothing about the link.
//! App-limited samples are never queue evidence and never lower the
//! estimate. Without this, a trickle of small frames with RTT jitter walks a
//! constrained estimate down to the trickle's own rate.
//!
//! Like BBR, a constrained estimate is held while the sender is app-limited
//! (idle screens say nothing about the link), raised by any faster delivery,
//! and probed upward by 25 % after `PROBE_INTERVAL` of busy sending with no
//! queue, so a link that got faster is found again. It is only re-measured
//! downward by fresh queue evidence.

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
/// Shorter sample intervals are dominated by timer and ACK jitter.
pub const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(8);
/// Smaller deliveries cannot show a queue reliably.
pub const MIN_SAMPLE_BYTES: u64 = 4_096;
/// Per-frame budget = capacity x frame interval x 80 %.
pub const BUDGET_UTILIZATION_PCT: u64 = 80;
/// Busy sending without queue evidence for this long probes upward.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// Probe step, percent of the current estimate.
pub const PROBE_GAIN_PCT: u64 = 125;
/// "Busy": the sending rate over the last `PROBE_INTERVAL` reached this
/// share of the estimate (the sender was using the link, not idle).
pub const PROBE_BUSY_PCT: u64 = 60;
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

#[derive(Debug)]
pub struct CapacityEstimator {
    sent: VecDeque<SentFrame>,
    delivered: u64,
    delivered_time: Instant,
    first_sent_time: Instant,
    min_rtt: VecDeque<(Instant, Duration)>,
    /// (time, bps, app-limited)
    samples: VecDeque<(Instant, u64, bool)>,
    queue_evidence: VecDeque<Instant>,
    capacity_bps: Option<u64>,
    /// Last queue-evidence update or probe of `capacity_bps`.
    last_update: Instant,
    /// Bytes sent over the last `PROBE_INTERVAL`.
    recent_sends: VecDeque<(Instant, u64)>,
    recent_bytes: u64,
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
            capacity_bps: None,
            last_update: now,
            recent_sends: VecDeque::new(),
            recent_bytes: 0,
            app_limited_until: 0,
        }
    }

    /// The receiver's decoder chain is gone (reconnect / new peer): frames in
    /// flight will never be ACKed. Measurements of the path are kept.
    pub fn forget_inflight(&mut self) {
        self.sent.clear();
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
        self.recent_sends.push_back((now, bytes));
        self.recent_bytes += bytes;
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
        self.maybe_probe(now);
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
        if link_limited {
            self.queue_evidence.push_back(now);
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

    fn update_capacity(&mut self, now: Instant, bps: u64, link_limited: bool) {
        if link_limited && self.queue_evidence.len() >= CONSTRAINED_MIN_SAMPLES {
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
            self.last_update = now;
            return;
        }
        if let Some(capacity) = self.capacity_bps {
            self.capacity_bps = Some(capacity.max(bps));
        }
    }

    /// Busy sending with no queue for a probe interval: step the estimate
    /// up. Idle time proves nothing and holds it.
    fn maybe_probe(&mut self, now: Instant) {
        while self
            .recent_sends
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > PROBE_INTERVAL)
        {
            let (_, bytes) = self.recent_sends.pop_front().unwrap_or((now, 0));
            self.recent_bytes -= bytes;
        }
        let Some(capacity) = self.capacity_bps else {
            return;
        };
        if now.saturating_duration_since(self.last_update) < PROBE_INTERVAL {
            return;
        }
        let send_bps =
            u128::from(self.recent_bytes) * 8_000_000 / PROBE_INTERVAL.as_micros().max(1);
        if send_bps * 100 >= u128::from(capacity) * u128::from(PROBE_BUSY_PCT) {
            self.capacity_bps = Some(capacity * PROBE_GAIN_PCT / 100);
            self.last_update = now;
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
            .is_some_and(|at| now.saturating_duration_since(*at) > CONSTRAINED_HOLD)
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
        self.capacity_bps.is_some()
    }

    /// The capacity of a constrained link; `None` until the link has shown
    /// a queue (unconstrained: no budget, no behaviour change).
    pub fn capacity(&self) -> Option<Capacity> {
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
        // 6 Mbps of frames into a 2 Mbps bottleneck: the queue grows and the
        // back-to-back ACKs measure the link.
        let mut link = Link {
            bps: 2_000_000,
            prop: Duration::from_millis(20),
            busy_until: Instant::now(),
        };
        let (estimator, _) = run(&mut link, 60, 25_000);
        let capacity = estimator.capacity().expect("constrained");
        assert!(
            (1_800_000..=2_200_000).contains(&capacity.bps),
            "{capacity:?}"
        );
        assert_eq!(capacity.target_bps(), capacity.bps * 8 / 10);
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
    fn estimate_is_held_through_idle_and_probed_up_when_busy() {
        let start = Instant::now();
        let mut link = Link {
            bps: 2_000_000,
            prop: Duration::from_millis(20),
            busy_until: start,
        };
        let mut estimator = CapacityEstimator::new(start);
        let mut seq = 0;
        let mut send =
            |estimator: &mut CapacityEstimator, link: &mut Link, at: Instant, bytes: u64| {
                estimator.on_sent(at, 1, seq, bytes);
                let ack = link.deliver(at, bytes);
                seq += 1;
                (ack, seq - 1)
            };
        // Overload: 25 KB frames at 30 fps into 2 Mbps.
        let mut pending = VecDeque::new();
        for n in 0..60u32 {
            let at = start + FRAME * n;
            while pending.front().is_some_and(|(ack, _)| *ack <= at) {
                let (ack, s) = pending.pop_front().unwrap();
                estimator.on_ack(ack, 1, s);
            }
            pending.push_back(send(&mut estimator, &mut link, at, 25_000));
        }
        let mut last = start;
        for (ack, s) in pending.drain(..) {
            estimator.on_ack(ack, 1, s);
            last = ack;
        }
        let measured = estimator.capacity().unwrap().bps;
        // Ten idle seconds of tiny frames: nothing learned, estimate held.
        let idle_start = last + Duration::from_millis(100);
        for n in 0..300u32 {
            let at = idle_start + FRAME * n;
            let (ack, s) = send(&mut estimator, &mut link, at, 300);
            estimator.on_ack(ack, 1, s);
        }
        assert_eq!(estimator.capacity().unwrap().bps, measured);
        // The link got faster (10 Mbps) and the sender follows the estimate
        // (80 %): no queue forms, so the estimate is probed up step by step.
        link.bps = 10_000_000;
        let mut at = idle_start + FRAME * 300 + Duration::from_millis(100);
        for _ in 0..300u32 {
            let target = estimator.capacity().unwrap().target_bps();
            let (ack, s) = send(&mut estimator, &mut link, at, target / 30 / 8);
            estimator.on_ack(ack, 1, s);
            at += FRAME;
        }
        assert!(
            estimator.capacity().unwrap().bps > measured * 3 / 2,
            "{:?}",
            estimator.capacity()
        );
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
        let held = estimator.capacity().unwrap().bps;
        assert!(held >= measured * 9 / 10, "{held} vs {measured}");
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
