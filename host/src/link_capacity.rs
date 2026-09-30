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
//! case on a desktop stream) only say "at least this much"; the link counts
//! as constrained only while ACKs show a queue: several samples within a
//! short hold whose RTT exceeds the minimum by `QUEUE_EVIDENCE` and whose
//! delivery was slower than the sending. An unconstrained link has no
//! estimate and so no budget.

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateSample {
    pub bps: u64,
    pub rtt: Duration,
    /// The delivery was queue-limited (the link, not the sender, set it).
    pub link_limited: bool,
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
    samples: VecDeque<(Instant, u64)>,
    queue_evidence: VecDeque<Instant>,
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
        self.sent.push_back(SentFrame {
            generation,
            seq,
            bytes,
            sent_at: now,
            delivered: self.delivered,
            delivered_time: self.delivered_time,
            first_sent_time: self.first_sent_time,
            restart,
        });
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
        let link_limited = delivered >= MIN_SAMPLE_BYTES
            && rtt >= min_rtt + QUEUE_EVIDENCE
            && ack_elapsed > send_elapsed;
        self.samples.push_back((now, bps));
        if link_limited {
            self.queue_evidence.push_back(now);
        }
        self.expire(now);
        Some(RateSample {
            bps,
            rtt,
            link_limited,
        })
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
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > CAPACITY_WINDOW)
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
            .filter(|(at, _)| now.saturating_duration_since(*at) <= CAPACITY_WINDOW)
            .map(|(_, bps)| *bps)
            .max()
    }

    pub fn is_constrained(&self, now: Instant) -> bool {
        self.queue_evidence
            .iter()
            .filter(|at| now.saturating_duration_since(**at) <= CONSTRAINED_HOLD)
            .count()
            >= CONSTRAINED_MIN_SAMPLES
    }

    /// The capacity of a constrained link; `None` while the link shows no
    /// queue (unconstrained: no budget, no behaviour change).
    pub fn capacity(&self, now: Instant) -> Option<Capacity> {
        if !self.is_constrained(now) {
            return None;
        }
        self.max_delivery_bps(now).map(|bps| Capacity { bps })
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
        assert!(!estimator.is_constrained(now));
        assert_eq!(estimator.capacity(now), None);
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
        let (estimator, now) = run(&mut link, 60, 25_000);
        let capacity = estimator.capacity(now).expect("constrained");
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
        assert!(!estimator.is_constrained(now + Duration::from_millis(10)));
    }

    #[test]
    fn constrained_state_expires_without_fresh_queue_evidence() {
        let mut link = Link {
            bps: 2_000_000,
            prop: Duration::from_millis(20),
            busy_until: Instant::now(),
        };
        let (estimator, now) = run(&mut link, 60, 25_000);
        assert!(estimator.capacity(now).is_some());
        let later = now + CONSTRAINED_HOLD + Duration::from_millis(1);
        assert_eq!(estimator.capacity(later), None);
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
