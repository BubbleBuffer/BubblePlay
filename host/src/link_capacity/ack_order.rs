//! Ordering of ACKs against in-flight frames, shared by the send policy
//! (`wss_v4`) and the capacity estimator so the two retire the same frames.

/// Half of the u32 circle: the largest distance still read as "earlier".
const WRAP_HALF: u32 = 0x8000_0000;

/// True when `candidate` is at or after `target` on the wrapping u32 circle.
pub(crate) fn seq_reached(candidate: u32, target: u32) -> bool {
    candidate.wrapping_sub(target) < WRAP_HALF
}

/// True when an ACK for (`ack_generation`, `ack_seq`) retires the in-flight
/// frame (`frame_generation`, `frame_seq`): an earlier generation is
/// superseded, the same generation is retired up to `ack_seq`, and a later
/// generation is still in flight.
pub(crate) fn ack_retires(
    ack_generation: u32,
    ack_seq: u32,
    frame_generation: u32,
    frame_seq: u32,
) -> bool {
    if frame_generation == ack_generation {
        seq_reached(ack_seq, frame_seq)
    } else {
        seq_reached(ack_generation, frame_generation)
    }
}

#[cfg(test)]
mod tests {
    use super::ack_retires;
    use crate::link_capacity::CapacityEstimator;
    use crate::wss_v4::SendPolicy;
    use std::time::{Duration, Instant};

    const FRAME_BYTES: u64 = 1_000;
    const ACK_DELAY: Duration = Duration::from_millis(20);

    #[test]
    fn ack_retires_older_generations_and_keeps_later_ones() {
        assert!(ack_retires(5, 3, 5, 3));
        assert!(!ack_retires(5, 3, 5, 4));
        assert!(ack_retires(5, 3, 4, 99));
        assert!(!ack_retires(5, 3, 6, 0));
        // The generation counter wraps; 0 follows u32::MAX.
        assert!(ack_retires(0, 0, u32::MAX, 7));
        assert!(!ack_retires(u32::MAX, 7, 0, 0));
    }

    #[test]
    fn policy_ack_keeps_frames_of_a_later_generation_in_flight() {
        let now = Instant::now();
        let mut policy = SendPolicy::new(now);
        policy.on_sent(now, 1, 1, FRAME_BYTES, true);
        policy.on_sent(now, 1, 2, FRAME_BYTES, false);
        policy.on_sent(now, 2, 1, FRAME_BYTES, true);
        policy.on_sent(now, 2, 2, FRAME_BYTES, false);
        assert!(policy.on_ack(now + ACK_DELAY, 1, 2).is_some());
        assert_eq!(policy.inflight_frames(), 2);
        assert_eq!(policy.inflight_bytes(), 2 * FRAME_BYTES);
    }

    #[test]
    fn estimator_ack_does_not_count_a_later_generation_as_delivered() {
        let now = Instant::now();
        let mut estimator = CapacityEstimator::new(now);
        estimator.on_sent(now, 1, 1, FRAME_BYTES);
        estimator.on_sent(now, 2, 1, FRAME_BYTES);
        estimator.on_ack(now + ACK_DELAY, 1, 1);
        // The generation-2 frame is still tracked: its own ACK is accepted.
        assert!(estimator.on_ack(now + 2 * ACK_DELAY, 2, 1).is_some());
    }
}
