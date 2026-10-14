use std::sync::Arc;
use std::time::Instant;

use crate::{
    media::{MediaStats, StatsSnapshot},
    net::TransportStats,
};

/// Small polling boundary for the UI. Receipt/drop/keyframe counters come from
/// the socket, while decoder and drawing timings come from the media pipeline.
/// Thus receive FPS includes frames ACKed and then refused by the receive gate.
#[derive(Clone)]
pub struct SessionStats {
    pub(crate) media: MediaStats,
    pub(crate) transport: Arc<TransportStats>,
    /// Local volume/mute and audio counters, retained independently of the UI.
    pub audio: super::SessionAudioControl,
}

/// Input capture state is intentionally separate from model/UI state in this
/// step. Session owners can retain this handle for later chrome wiring.
pub type SessionInputState = crate::input::CaptureStatus;

impl SessionStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        let mut stats = self.media.snapshot(Instant::now());
        let net = self.transport.snapshot();
        stats.received_fps = net.received_fps;
        stats.keyframe_requests_1s = net.keyframe_requests_1s;
        stats.drops_1s += net.gate_drops_1s + net.queue_overflows_1s;
        stats.totals.received = net.frames_received;
        stats.totals.keyframe_requests = net.keyframe_requests;
        stats.totals.dropped += net.gate_drops + net.queue_overflows;
        stats
    }
}
