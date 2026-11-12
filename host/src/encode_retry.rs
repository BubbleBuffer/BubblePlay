//! One backoff for every path of the frame loop that retries after a failed
//! texture preparation or encode. Without it each failure re-runs capture and
//! encoder creation on the very next loop pass.

use std::time::{Duration, Instant};
use tracing::warn;

/// Delay after the first failure in a row.
pub const ENCODE_RETRY_INITIAL_BACKOFF_MS: u64 = 100;
/// Cap of the doubling delay.
pub const ENCODE_RETRY_MAX_BACKOFF_MS: u64 = 5_000;

pub struct EncodeRetryGate {
    failures: u32,
    delay: Duration,
    not_before: Instant,
}

impl EncodeRetryGate {
    pub fn new(now: Instant) -> Self {
        Self {
            failures: 0,
            delay: Duration::ZERO,
            not_before: now,
        }
    }

    /// True when the frame path may run again.
    pub fn ready(&self, now: Instant) -> bool {
        now >= self.not_before
    }

    /// Earliest instant of the next attempt; waits must not wake before it.
    pub fn not_before(&self) -> Instant {
        self.not_before
    }

    /// Records a failed attempt and defers the next one.
    pub fn failed(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.delay = if self.delay.is_zero() {
            Duration::from_millis(ENCODE_RETRY_INITIAL_BACKOFF_MS)
        } else {
            (self.delay * 2).min(Duration::from_millis(ENCODE_RETRY_MAX_BACKOFF_MS))
        };
        self.not_before = now + self.delay;
        warn!(
            failures = self.failures,
            retry_in_ms = self.delay.as_millis() as u64,
            "encode path failed; next attempt deferred"
        );
    }

    /// A frame was encoded: the next failure starts from the initial delay.
    pub fn succeeded(&mut self) {
        self.failures = 0;
        self.delay = Duration::ZERO;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_failures_back_off_to_the_cap_and_success_resets() {
        let start = Instant::now();
        let mut gate = EncodeRetryGate::new(start);
        assert!(gate.ready(start));
        gate.failed(start);
        assert!(!gate.ready(start));
        assert!(gate.ready(start + Duration::from_millis(ENCODE_RETRY_INITIAL_BACKOFF_MS)));
        let mut now = start;
        for _ in 0..10 {
            now = gate.not_before();
            gate.failed(now);
        }
        assert_eq!(
            gate.not_before() - now,
            Duration::from_millis(ENCODE_RETRY_MAX_BACKOFF_MS)
        );
        gate.succeeded();
        gate.failed(now);
        assert_eq!(
            gate.not_before() - now,
            Duration::from_millis(ENCODE_RETRY_INITIAL_BACKOFF_MS)
        );
    }
}
