//! Receive-side ordering, keyframe throttle and reconnect backoff. Ports
//! `V4ReceiveGate`, `requestWssKeyframe` and `nextBackoffMsV4` from
//! `worker/public/media-v4.js` and `receiver.js`.

use std::time::{Duration, Instant};

use super::protocol::FrameHeader;

/// What the gate decided for one accepted-as-valid BPV4 frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// Decode it, in arrival order.
    Decode,
    /// Drop it. `invalidated` is true when this frame moved the gate from
    /// accepting into waiting, so the decoder must reset. `request` is the
    /// keyframe reason to send (throttled by [`KeyframeThrottle`]).
    Drop { invalidated: bool, request: String },
}

/// Receive gate: deltas decode only after a keyframe, in unbroken sequence
/// within one generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiveGate {
    generation: Option<u32>,
    expected_seq: Option<u32>,
    waiting_for_key: bool,
    pending_reason: String,
}

impl Default for ReceiveGate {
    fn default() -> Self {
        let mut gate = Self {
            generation: None,
            expected_seq: None,
            waiting_for_key: true,
            pending_reason: String::new(),
        };
        gate.reset("connect");
        gate
    }
}

impl ReceiveGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start over: nothing decodes until a keyframe. Used on every new connection
    /// and on a host join.
    pub fn reset(&mut self, reason: &str) {
        self.generation = None;
        self.expected_seq = None;
        self.waiting_for_key = true;
        self.pending_reason = reason.to_owned();
    }

    /// A decoder error or queue overflow: drop deltas until the next keyframe.
    /// Returns `true` when the gate was accepting before (a transition).
    pub fn invalidate(&mut self, reason: &str) -> bool {
        let was_accepting = !self.waiting_for_key;
        self.waiting_for_key = true;
        self.pending_reason = reason.to_owned();
        was_accepting
    }

    pub fn is_waiting_for_key(&self) -> bool {
        self.waiting_for_key
    }

    /// Decide on a valid frame. Keyframes always decode and set the expected sequence.
    pub fn accept(&mut self, header: &FrameHeader) -> GateVerdict {
        if header.key {
            self.generation = Some(header.generation);
            self.expected_seq = Some(header.seq.wrapping_add(1));
            self.waiting_for_key = false;
            self.pending_reason.clear();
            return GateVerdict::Decode;
        }
        let reason = if self.waiting_for_key {
            Some(if self.pending_reason.is_empty() {
                "awaiting-keyframe".to_owned()
            } else {
                self.pending_reason.clone()
            })
        } else if Some(header.generation) != self.generation {
            Some("generation-change".to_owned())
        } else if Some(header.seq) != self.expected_seq {
            Some("sequence-gap".to_owned())
        } else {
            None
        };
        match reason {
            Some(reason) => {
                let invalidated = !self.waiting_for_key;
                self.waiting_for_key = true;
                self.pending_reason = reason.clone();
                GateVerdict::Drop {
                    invalidated,
                    request: reason,
                }
            }
            None => {
                self.expected_seq = Some(header.seq.wrapping_add(1));
                GateVerdict::Decode
            }
        }
    }
}

/// At most one keyframe request per interval. A throttled request is simply
/// not sent; the next dropped delta asks again.
#[derive(Debug, Clone)]
pub struct KeyframeThrottle {
    interval: Duration,
    last_sent: Option<Instant>,
}

impl KeyframeThrottle {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_sent: None,
        }
    }

    /// True (and records `now`) when a request may go out.
    pub fn allow(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last_sent {
            if now.saturating_duration_since(last) < self.interval {
                return false;
            }
        }
        self.last_sent = Some(now);
        true
    }
}

/// Reconnect backoff: the first delay is `min`, then doubling up to `max`.
/// A `previous` below `min` (including zero, the reset value) gives `min`.
pub fn next_backoff(previous: Duration, min: Duration, max: Duration) -> Duration {
    if previous < min {
        return min;
    }
    previous
        .checked_mul(2)
        .map_or(max, |doubled| doubled.min(max))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::config::Codec;

    fn frame(key: bool, generation: u32, seq: u32) -> FrameHeader {
        FrameHeader {
            key,
            refine: false,
            codec: Codec::H264,
            generation,
            seq,
            capture_ts_us: 0,
            width: 1,
            height: 1,
        }
    }

    fn drop_request(verdict: GateVerdict) -> (bool, String) {
        match verdict {
            GateVerdict::Drop {
                invalidated,
                request,
            } => (invalidated, request),
            GateVerdict::Decode => panic!("expected drop, got decode"),
        }
    }

    #[test]
    fn nothing_decodes_before_the_first_keyframe_and_the_connect_reason_is_requested() {
        let mut gate = ReceiveGate::new();
        assert!(gate.is_waiting_for_key());
        let (invalidated, request) = drop_request(gate.accept(&frame(false, 1, 1)));
        assert_eq!(request, "connect");
        assert!(!invalidated, "already waiting is not a transition");
    }

    #[test]
    fn keyframe_opens_the_gate_and_consecutive_deltas_decode() {
        let mut gate = ReceiveGate::new();
        assert_eq!(gate.accept(&frame(true, 7, 10)), GateVerdict::Decode);
        assert_eq!(gate.accept(&frame(false, 7, 11)), GateVerdict::Decode);
        assert_eq!(gate.accept(&frame(false, 7, 12)), GateVerdict::Decode);
    }

    #[test]
    fn sequence_gap_drops_and_asks_for_a_keyframe_once_per_transition() {
        let mut gate = ReceiveGate::new();
        gate.accept(&frame(true, 7, 10));
        let (invalidated, request) = drop_request(gate.accept(&frame(false, 7, 12)));
        assert!(invalidated);
        assert_eq!(request, "sequence-gap");
        let (invalidated, request) = drop_request(gate.accept(&frame(false, 7, 13)));
        assert!(!invalidated, "later drops are not new transitions");
        assert_eq!(
            request, "sequence-gap",
            "pending reason repeats until a keyframe"
        );
    }

    #[test]
    fn generation_change_without_keyframe_drops() {
        let mut gate = ReceiveGate::new();
        gate.accept(&frame(true, 7, 10));
        let (_, request) = drop_request(gate.accept(&frame(false, 8, 11)));
        assert_eq!(request, "generation-change");
    }

    #[test]
    fn keyframe_after_gap_resumes_at_its_own_sequence() {
        let mut gate = ReceiveGate::new();
        gate.accept(&frame(true, 7, 10));
        drop_request(gate.accept(&frame(false, 7, 40)));
        assert_eq!(gate.accept(&frame(true, 8, 50)), GateVerdict::Decode);
        assert_eq!(gate.accept(&frame(false, 8, 51)), GateVerdict::Decode);
    }

    #[test]
    fn keyframe_resets_the_expected_sequence_even_when_generation_changes() {
        let mut gate = ReceiveGate::new();
        gate.accept(&frame(true, 1, 0xffff_ffff));
        assert_eq!(
            gate.accept(&frame(false, 1, 0)),
            GateVerdict::Decode,
            "u32 wrap is in sequence"
        );
    }

    #[test]
    fn reset_drops_everything_until_a_keyframe_and_names_the_reason() {
        let mut gate = ReceiveGate::new();
        gate.accept(&frame(true, 7, 10));
        gate.reset("host-joined");
        let (invalidated, request) = drop_request(gate.accept(&frame(false, 7, 11)));
        assert!(
            !invalidated,
            "reset is not counted as an invalidation transition"
        );
        assert_eq!(request, "host-joined");
        assert_eq!(gate.accept(&frame(true, 7, 12)), GateVerdict::Decode);
    }

    #[test]
    fn invalidate_reports_only_the_accepting_to_waiting_transition() {
        let mut gate = ReceiveGate::new();
        assert!(!gate.invalidate("decode-error"), "already waiting");
        gate.accept(&frame(true, 1, 1));
        assert!(gate.invalidate("queue-overflow"));
        assert!(!gate.invalidate("decode-error"));
        let (_, request) = drop_request(gate.accept(&frame(false, 1, 2)));
        assert_eq!(
            request, "decode-error",
            "the latest invalidation reason is requested"
        );
    }

    #[test]
    fn a_keyframe_clears_the_pending_reason() {
        let mut gate = ReceiveGate::new();
        gate.invalidate("queue-overflow");
        assert_eq!(gate.accept(&frame(true, 1, 1)), GateVerdict::Decode);
        assert!(gate.accept(&frame(false, 1, 3)) != GateVerdict::Decode);
    }

    #[test]
    fn empty_pending_reason_falls_back_to_awaiting_keyframe() {
        let mut gate = ReceiveGate::new();
        gate.reset("");
        let (_, request) = drop_request(gate.accept(&frame(false, 1, 1)));
        assert_eq!(request, "awaiting-keyframe");
    }

    #[test]
    fn keyframe_throttle_allows_one_request_per_interval() {
        let base = Instant::now();
        let mut throttle = KeyframeThrottle::new(Duration::from_millis(250));
        assert!(throttle.allow(base));
        assert!(!throttle.allow(base + Duration::from_millis(249)));
        assert!(throttle.allow(base + Duration::from_millis(250)));
        assert!(!throttle.allow(base + Duration::from_millis(260)));
    }

    #[test]
    fn throttled_request_does_not_restart_the_interval() {
        let base = Instant::now();
        let mut throttle = KeyframeThrottle::new(Duration::from_millis(250));
        assert!(throttle.allow(base));
        assert!(!throttle.allow(base + Duration::from_millis(100)));
        assert!(
            throttle.allow(base + Duration::from_millis(250)),
            "measured from the sent request"
        );
    }

    #[test]
    fn backoff_starts_at_250ms_doubles_and_caps_at_5s() {
        let min = Duration::from_millis(250);
        let max = Duration::from_secs(5);
        let mut delays = Vec::new();
        let mut current = Duration::ZERO;
        for _ in 0..7 {
            current = next_backoff(current, min, max);
            delays.push(current.as_millis());
        }
        assert_eq!(delays, vec![250, 500, 1000, 2000, 4000, 5000, 5000]);
    }

    #[test]
    fn backoff_below_minimum_resets_to_minimum() {
        let min = Duration::from_millis(250);
        let max = Duration::from_secs(5);
        assert_eq!(next_backoff(Duration::from_millis(10), min, max), min);
        assert_eq!(
            next_backoff(Duration::MAX, min, max),
            max,
            "overflow saturates to the cap"
        );
    }
}
