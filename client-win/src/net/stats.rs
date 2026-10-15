//! Transport counters and state, shared between the net thread and any reader.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
struct Window {
    received: VecDeque<Instant>,
    gate_drops: VecDeque<Instant>,
    queue_overflows: VecDeque<Instant>,
    keyframes: VecDeque<Instant>,
}

fn prune(queue: &mut VecDeque<Instant>, now: Instant) {
    while queue
        .front()
        .is_some_and(|at| now.saturating_duration_since(*at) > Duration::from_secs(1))
    {
        queue.pop_front();
    }
}
fn note(queue: &mut VecDeque<Instant>, at: Instant) {
    prune(queue, at);
    if queue.len() >= 8192 {
        queue.pop_front();
    }
    queue.push_back(at);
}

/// Where the transport is in its cycle. Readable at any time.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetState {
    #[default]
    Idle = 0,
    Signaling = 1,
    MediaHandshake = 2,
    Starting = 3,
    Streaming = 4,
    Backoff = 5,
    Stopped = 6,
}

impl NetState {
    fn from_u8(value: u8) -> NetState {
        match value {
            1 => NetState::Signaling,
            2 => NetState::MediaHandshake,
            3 => NetState::Starting,
            4 => NetState::Streaming,
            5 => NetState::Backoff,
            6 => NetState::Stopped,
            _ => NetState::Idle,
        }
    }
}

/// Live counters. Create one per session with [`TransportStats::new`], share it
/// through an `Arc`, and pass it to [`crate::net::run`].
#[derive(Debug, Default)]
pub struct TransportStats {
    pub(crate) state: AtomicU8,
    pub(crate) attempts: AtomicU64,
    pub(crate) established: AtomicU64,
    pub(crate) local_epoch: AtomicU64,
    pub(crate) frames_received: AtomicU64,
    pub(crate) frames_accepted: AtomicU64,
    pub(crate) gate_drops: AtomicU64,
    pub(crate) queue_overflows: AtomicU64,
    pub(crate) rejected_packets: AtomicU64,
    pub(crate) rejected_controls: AtomicU64,
    pub(crate) bpa1_ignored: AtomicU64,
    pub(crate) acks_sent: AtomicU64,
    pub(crate) keyframe_requests: AtomicU64,
    pub(crate) heartbeats_sent: AtomicU64,
    pub(crate) decoded: AtomicU64,
    pub(crate) last_decoded_seq: AtomicU64,
    pub(crate) stale_feedback: AtomicU64,
    last_error: Mutex<Option<String>>,
    window: Mutex<Window>,
    pub(crate) webrtc: Mutex<super::webrtc::stats::WebRtcStats>,
}

/// A point-in-time copy of [`TransportStats`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TransportSnapshot {
    pub received_fps: f64,
    pub gate_drops_1s: u32,
    pub queue_overflows_1s: u32,
    pub keyframe_requests_1s: u32,
    pub state: NetState,
    /// Connection attempts, including the first.
    pub attempts: u64,
    /// Connections that reached the media socket `hello-ok`.
    pub established: u64,
    /// The local epoch of the current (or last) connection.
    pub local_epoch: u64,
    /// Valid BPV4 frames received (before the gate).
    pub frames_received: u64,
    /// Frames handed to the decoder.
    pub frames_accepted: u64,
    /// Frames dropped by the receive gate.
    pub gate_drops: u64,
    /// Frames dropped because the event queue was full.
    pub queue_overflows: u64,
    /// Binary messages refused by the BPV4 parser.
    pub rejected_packets: u64,
    /// Text messages refused on the media socket.
    pub rejected_controls: u64,
    /// BPA1 audio messages ignored.
    pub bpa1_ignored: u64,
    pub acks_sent: u64,
    /// Keyframe requests sent after throttling.
    pub keyframe_requests: u64,
    pub heartbeats_sent: u64,
    /// Decoder feedback that was current.
    pub decoded: u64,
    /// Sequence of the last decoded frame, if any frame has been decoded.
    pub last_decoded_seq: Option<u32>,
    /// Feedback from an older local epoch, ignored.
    pub stale_feedback: u64,
}

impl TransportStats {
    pub fn transport_name(&self) -> &'static str {
        if self
            .webrtc
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .selected_transport
            .as_deref()
            == Some("webrtc")
        {
            "WebRTC"
        } else {
            "WSS v4"
        }
    }
    pub fn webrtc_snapshot(&self) -> serde_json::Value {
        serde_json::to_value(&*self.webrtc.lock().unwrap_or_else(|e| e.into_inner()))
            .expect("WebRTC stats serialize")
    }
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> NetState {
        NetState::from_u8(self.state.load(Relaxed))
    }

    /// The last transient or fatal error, as text.
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Development timing evidence: raw valid BPV4 receipt times before the gate.
    pub fn receive_timestamps(&self) -> Vec<Instant> {
        self.window
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .received
            .iter()
            .copied()
            .collect()
    }

    pub fn snapshot(&self) -> TransportSnapshot {
        let decoded = self.decoded.load(Relaxed);
        let mut guard = self.window.lock().unwrap_or_else(|p| p.into_inner());
        let window = &mut *guard;
        let now = Instant::now();
        for queue in [
            &mut window.received,
            &mut window.gate_drops,
            &mut window.queue_overflows,
            &mut window.keyframes,
        ] {
            prune(queue, now);
        }
        TransportSnapshot {
            received_fps: window.received.len() as f64,
            gate_drops_1s: window.gate_drops.len() as u32,
            queue_overflows_1s: window.queue_overflows.len() as u32,
            keyframe_requests_1s: window.keyframes.len() as u32,
            state: self.state(),
            attempts: self.attempts.load(Relaxed),
            established: self.established.load(Relaxed),
            local_epoch: self.local_epoch.load(Relaxed),
            frames_received: self.frames_received.load(Relaxed),
            frames_accepted: self.frames_accepted.load(Relaxed),
            gate_drops: self.gate_drops.load(Relaxed),
            queue_overflows: self.queue_overflows.load(Relaxed),
            rejected_packets: self.rejected_packets.load(Relaxed),
            rejected_controls: self.rejected_controls.load(Relaxed),
            bpa1_ignored: self.bpa1_ignored.load(Relaxed),
            acks_sent: self.acks_sent.load(Relaxed),
            keyframe_requests: self.keyframe_requests.load(Relaxed),
            heartbeats_sent: self.heartbeats_sent.load(Relaxed),
            decoded,
            last_decoded_seq: (decoded > 0).then(|| self.last_decoded_seq.load(Relaxed) as u32),
            stale_feedback: self.stale_feedback.load(Relaxed),
        }
    }

    pub(crate) fn set_state(&self, state: NetState) {
        self.state.store(state as u8, Relaxed);
    }

    pub(crate) fn record_received(&self, at: Instant) {
        count(&self.frames_received);
        note(&mut self.window.lock().unwrap().received, at);
    }
    pub(crate) fn record_gate_drop(&self) {
        count(&self.gate_drops);
        note(&mut self.window.lock().unwrap().gate_drops, Instant::now());
    }
    pub(crate) fn record_overflow(&self) {
        count(&self.queue_overflows);
        note(
            &mut self.window.lock().unwrap().queue_overflows,
            Instant::now(),
        );
    }
    pub(crate) fn record_keyframe(&self) {
        count(&self.keyframe_requests);
        note(&mut self.window.lock().unwrap().keyframes, Instant::now());
    }

    pub(crate) fn record_error(&self, message: String) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message);
    }
}

/// Increment a counter.
pub(crate) fn count(counter: &AtomicU64) {
    counter.fetch_add(1, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_stats_are_idle_and_zero() {
        let stats = TransportStats::new();
        assert_eq!(stats.state(), NetState::Idle);
        let snapshot = stats.snapshot();
        assert_eq!(snapshot, TransportSnapshot::default());
        assert_eq!(snapshot.last_decoded_seq, None);
        assert_eq!(stats.last_error(), None);
    }

    #[test]
    fn state_round_trips_through_the_atomic() {
        let stats = TransportStats::new();
        for state in [
            NetState::Signaling,
            NetState::MediaHandshake,
            NetState::Starting,
            NetState::Streaming,
            NetState::Backoff,
            NetState::Stopped,
        ] {
            stats.set_state(state);
            assert_eq!(stats.state(), state);
        }
    }

    #[test]
    fn last_decoded_seq_is_only_reported_after_a_decode() {
        let stats = TransportStats::new();
        stats.last_decoded_seq.store(41, Relaxed);
        assert_eq!(stats.snapshot().last_decoded_seq, None, "no decode yet");
        count(&stats.decoded);
        stats.last_decoded_seq.store(42, Relaxed);
        assert_eq!(stats.snapshot().last_decoded_seq, Some(42));
    }

    #[test]
    fn last_error_keeps_the_most_recent_message() {
        let stats = TransportStats::new();
        stats.record_error("first".into());
        stats.record_error("second".into());
        assert_eq!(stats.last_error().as_deref(), Some("second"));
    }
}
