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
        tracing::info!(side,epoch,secure,admin_mode=admin,requests=self.requests,queued=self.queued,forwarded=self.forwarded,received=self.received,accepted=self.accepted,applied=self.applied,rejected=?self.rejected,rejected_requests=?self.rejected_requests,last_error=self.last_error,"secure input summary");
        self.queued = 0;
        self.forwarded = 0;
        self.received = 0;
        self.accepted = 0;
        self.applied = 0;
        self.rejected.clear();
        self.last_error = 0;
        self.requests = 0;
        self.rejected_requests.clear();
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
}
