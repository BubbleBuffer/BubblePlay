//! Categorical counters only: never retain or log input values or pixels.
use super::protocol::InputReceipt;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
pub struct Counters {
    pub queued: u64,
    pub forwarded: u64,
    pub received: u64,
    pub accepted: u64,
    pub applied: u64,
    pub rejected: BTreeMap<&'static str, u64>,
    pub rejected_requests: BTreeMap<&'static str, u64>,
    pub requests: u64,
    pub last_error: u32,
    last_log: Instant,
    /// Everything since the last info line (the per-second windows summed).
    rolled: Rolled,
    last_info: Instant,
    /// Identity of the last log call, for the line written when the counters are dropped.
    last_ctx: Option<(&'static str, u64, bool, bool)>,
}

/// Seconds between info lines. The per-second summary goes to debug.
pub const INFO_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Rolled {
    requests: u64,
    queued: u64,
    forwarded: u64,
    received: u64,
    accepted: u64,
    applied: u64,
    rejected: BTreeMap<&'static str, u64>,
    rejected_requests: BTreeMap<&'static str, u64>,
    last_error: u32,
}

impl Rolled {
    fn add(&mut self, c: &Counters) {
        self.requests += c.requests;
        self.queued += c.queued;
        self.forwarded += c.forwarded;
        self.received += c.received;
        self.accepted += c.accepted;
        self.applied += c.applied;
        for (k, v) in &c.rejected {
            *self.rejected.entry(k).or_default() += v;
        }
        for (k, v) in &c.rejected_requests {
            *self.rejected_requests.entry(k).or_default() += v;
        }
        if c.last_error != 0 {
            self.last_error = c.last_error;
        }
    }

    /// Nothing happened: not worth an info line.
    fn is_idle(&self) -> bool {
        self.requests == 0
            && self.queued == 0
            && self.forwarded == 0
            && self.received == 0
            && self.accepted == 0
            && self.applied == 0
            && self.rejected.is_empty()
            && self.last_error == 0
    }
}
impl Default for Counters {
    fn default() -> Self {
        Self {
            queued: 0,
            forwarded: 0,
            received: 0,
            accepted: 0,
            applied: 0,
            rejected: BTreeMap::new(),
            rejected_requests: BTreeMap::new(),
            requests: 0,
            last_error: 0,
            last_log: Instant::now(),
            rolled: Rolled::default(),
            last_info: Instant::now(),
            last_ctx: None,
        }
    }
}
impl Counters {
    pub fn reject(&mut self, records: u64, reason: &str) {
        *self
            .rejected
            .entry(super::protocol::reason_name(reason))
            .or_default() += records;
        *self
            .rejected_requests
            .entry(super::protocol::reason_name(reason))
            .or_default() += 1;
    }
    pub fn receipt(&mut self, r: &InputReceipt) {
        self.requests += 1;
        self.received += u64::from(r.received);
        self.accepted += u64::from(r.accepted);
        self.applied += u64::from(r.applied);
        if r.error_code != 0 {
            self.last_error = r.error_code;
        }
        if r.reason != "ok" {
            self.reject(u64::from(r.received - r.applied), &r.reason);
        }
    }
    pub fn log(&mut self, side: &'static str, epoch: u64, secure: bool, admin: bool) {
        if self.last_log.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_log = Instant::now();
        self.last_ctx = Some((side, epoch, secure, admin));
        tracing::debug!(side,epoch,secure,admin_mode=admin,requests=self.requests,queued=self.queued,forwarded=self.forwarded,received=self.received,accepted=self.accepted,applied=self.applied,rejected=?self.rejected,rejected_requests=?self.rejected_requests,last_error=self.last_error,"secure input summary");
        let mut rolled = std::mem::take(&mut self.rolled);
        rolled.add(self);
        self.rolled = rolled;
        self.queued = 0;
        self.forwarded = 0;
        self.received = 0;
        self.accepted = 0;
        self.applied = 0;
        self.rejected.clear();
        self.last_error = 0;
        self.requests = 0;
        self.rejected_requests.clear();
        if self.last_info.elapsed() >= INFO_INTERVAL {
            self.log_rolled();
        }
    }

    /// Write the summed counters as one info line (skipped when nothing happened) and start over.
    fn log_rolled(&mut self) {
        self.last_info = Instant::now();
        let rolled = std::mem::take(&mut self.rolled);
        let Some((side, epoch, secure, admin)) = self.last_ctx else {
            return;
        };
        if rolled.is_idle() {
            return;
        }
        tracing::info!(side,epoch,secure,admin_mode=admin,requests=rolled.requests,queued=rolled.queued,forwarded=rolled.forwarded,received=rolled.received,accepted=rolled.accepted,applied=rolled.applied,rejected=?rolled.rejected,rejected_requests=?rolled.rejected_requests,last_error=rolled.last_error,"secure input summary");
    }
}
impl Drop for Counters {
    /// The session ended: write what is left (including the current partial second).
    fn drop(&mut self) {
        let mut rolled = std::mem::take(&mut self.rolled);
        rolled.add(self);
        self.rolled = rolled;
        self.log_rolled();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn later_success_cannot_erase_failure_before_interval_log() {
        let mut c = Counters::default();
        c.receipt(&InputReceipt {
            id: 1,
            received: 2,
            accepted: 2,
            applied: 0,
            error_code: 5,
            reason: "partial-send-input".into(),
        });
        c.receipt(&InputReceipt {
            id: 2,
            received: 1,
            accepted: 1,
            applied: 1,
            error_code: 0,
            reason: "ok".into(),
        });
        assert_eq!(c.last_error, 5);
        assert_eq!((c.received, c.accepted, c.applied), (3, 3, 1));
        assert_eq!(c.rejected.get("partial-send-input"), Some(&2));
        c.last_log = Instant::now() - Duration::from_secs(2);
        c.log("fixture", 1, true, true);
        assert_eq!(
            (c.received, c.accepted, c.applied, c.last_error),
            (0, 0, 0, 0)
        );
        assert!(c.rejected.is_empty());
    }

    #[test]
    fn counters_roll_up_to_one_info_line_per_interval() {
        let mut c = Counters::default();
        c.receipt(&InputReceipt {
            id: 1,
            received: 2,
            accepted: 2,
            applied: 2,
            error_code: 0,
            reason: "ok".into(),
        });
        c.last_log = Instant::now() - Duration::from_secs(2);
        c.log("fixture", 1, true, true);
        // The window is reset; the minute keeps the sum and is not released early.
        assert_eq!(c.received, 0);
        assert_eq!(c.rolled.received, 2);
        assert!(!c.rolled.is_idle());
        c.last_log = Instant::now() - Duration::from_secs(2);
        c.last_info = Instant::now() - INFO_INTERVAL - Duration::from_secs(1);
        c.log("fixture", 1, true, true);
        assert!(c.rolled.is_idle(), "released and started over");
        assert!(c.last_info.elapsed() < Duration::from_secs(5));
    }
}
