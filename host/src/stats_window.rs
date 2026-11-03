//! Per-second producer statistics rolled up into a slower info line.
//!
//! The producer samples its window every second. Logging each sample at info filled the
//! host log (about 18% of its lines were these). The samples still go out at debug; this
//! rollup sums them and releases one total per interval (and one at session end).
use std::time::{Duration, Instant};

/// Rate of the info line.
pub const INFO_INTERVAL: Duration = Duration::from_secs(60);

/// Counters of one producer window (or the sum of several).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProducerWindow {
    pub preencode_skips: u64,
    pub submission_failures: u64,
    pub idr_count: u64,
    pub delta_count: u64,
    pub span_ms: u64,
    pub loop_iterations: u64,
    pub capture_acquired: u64,
    pub capture_new: u64,
    pub max_access_unit_bytes: usize,
    pub tile_hash_us: u64,
    pub qp_max: u32,
    pub qp_sum: u64,
    pub qp_count: u64,
}

impl ProducerWindow {
    pub fn idr_percent(&self) -> u64 {
        self.idr_count
            .saturating_mul(100)
            .checked_div(self.idr_count.saturating_add(self.delta_count))
            .unwrap_or(0)
    }

    pub fn qp_mean(&self) -> u64 {
        self.qp_sum / self.qp_count.max(1)
    }

    fn merge(&mut self, other: &Self) {
        self.preencode_skips = self.preencode_skips.saturating_add(other.preencode_skips);
        self.submission_failures = self
            .submission_failures
            .saturating_add(other.submission_failures);
        self.idr_count = self.idr_count.saturating_add(other.idr_count);
        self.delta_count = self.delta_count.saturating_add(other.delta_count);
        self.span_ms = self.span_ms.saturating_add(other.span_ms);
        self.loop_iterations = self.loop_iterations.saturating_add(other.loop_iterations);
        self.capture_acquired = self.capture_acquired.saturating_add(other.capture_acquired);
        self.capture_new = self.capture_new.saturating_add(other.capture_new);
        self.max_access_unit_bytes = self.max_access_unit_bytes.max(other.max_access_unit_bytes);
        self.tile_hash_us = self.tile_hash_us.saturating_add(other.tile_hash_us);
        self.qp_max = self.qp_max.max(other.qp_max);
        self.qp_sum = self.qp_sum.saturating_add(other.qp_sum);
        self.qp_count = self.qp_count.saturating_add(other.qp_count);
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Sums samples and releases the total once per interval.
#[derive(Debug)]
pub struct Rollup {
    interval: Duration,
    next: Instant,
    sum: ProducerWindow,
}

impl Rollup {
    pub fn new(now: Instant, interval: Duration) -> Self {
        Self {
            interval,
            next: now + interval,
            sum: ProducerWindow::default(),
        }
    }

    /// Add one sample. Returns the total when the interval has passed.
    pub fn add(&mut self, now: Instant, sample: &ProducerWindow) -> Option<ProducerWindow> {
        self.sum.merge(sample);
        if now < self.next {
            return None;
        }
        self.next = now + self.interval;
        Some(std::mem::take(&mut self.sum))
    }

    /// The total of what was added since the last release, for the session end.
    pub fn finish(&mut self, now: Instant) -> Option<ProducerWindow> {
        self.next = now + self.interval;
        let sum = std::mem::take(&mut self.sum);
        (!sum.is_empty()).then_some(sum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ProducerWindow {
        ProducerWindow {
            preencode_skips: 1,
            idr_count: 1,
            delta_count: 29,
            span_ms: 1000,
            loop_iterations: 100,
            max_access_unit_bytes: 500,
            qp_max: 30,
            qp_sum: 600,
            qp_count: 30,
            ..ProducerWindow::default()
        }
    }

    #[test]
    fn one_total_per_interval_not_per_sample() {
        let start = Instant::now();
        let mut rollup = Rollup::new(start, Duration::from_secs(60));
        let mut released = Vec::new();
        for second in 1..=120u64 {
            let mut s = sample();
            if second == 7 {
                s.max_access_unit_bytes = 9000;
                s.qp_max = 40;
            }
            if let Some(total) = rollup.add(start + Duration::from_secs(second), &s) {
                released.push(total);
            }
        }
        assert_eq!(released.len(), 2, "120 samples are two info lines");
        let first = released[0];
        assert_eq!(first.span_ms, 60_000);
        assert_eq!(first.delta_count, 60 * 29);
        assert_eq!(first.idr_count, 60);
        assert_eq!(first.max_access_unit_bytes, 9000);
        assert_eq!(first.qp_max, 40);
        assert_eq!(first.qp_mean(), 20);
        assert_eq!(released[1].max_access_unit_bytes, 500);
        assert_eq!(first.idr_percent(), 3);
    }

    #[test]
    fn session_end_releases_the_remainder_once() {
        let start = Instant::now();
        let mut rollup = Rollup::new(start, Duration::from_secs(60));
        assert!(rollup
            .add(start + Duration::from_secs(1), &sample())
            .is_none());
        assert!(rollup
            .add(start + Duration::from_secs(2), &sample())
            .is_none());
        let rest = rollup.finish(start + Duration::from_secs(3)).unwrap();
        assert_eq!(rest.span_ms, 2000);
        assert!(rollup.finish(start + Duration::from_secs(4)).is_none());
    }
}
