use serde::Serialize;
use std::{
    collections::{HashSet, VecDeque},
    time::Instant,
};

#[derive(Debug, Clone, Serialize)]
pub struct StatePoint {
    pub at_ms: f64,
    pub local_epoch: u64,
    pub state: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct Fallback {
    pub at_ms: f64,
    pub offer_to_fallback_ms: f64,
    pub reason: String,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct RtpStats {
    pub packets: u64,
    pub bytes: u64,
    pub packets_lost: u64,
    pub loss_fraction: Option<f32>,
    pub nacks: u64,
    pub plis: u64,
    pub firs: u64,
    pub jitter_ms: Option<f64>,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct WebRtcStats {
    pub enabled: bool,
    pub ice_checking_timeout_ms: Option<u64>,
    pub selected_transport: Option<String>,
    pub ice_timeline: VecDeque<StatePoint>,
    pub selected_candidate_pair_type: Option<String>,
    pub rtt_ms: Option<f64>,
    pub video: RtpStats,
    pub audio: RtpStats,
    pub fallbacks: VecDeque<Fallback>,
    pub input_control_sent: u64,
    pub input_motion_sent: u64,
    pub first_frame_retries: u64,
    pub first_frame_failures: u64,
    pub malformed_candidates: u64,
    pub rejected_datagrams: u64,
    pub depacketization_drops: u64,
    pub stun_requests: u64,
    pub srflx_candidates: u64,
}
impl WebRtcStats {
    pub fn state(&mut self, epoch: u64, state: &str, at: Instant, origin: Instant) {
        if self.ice_timeline.len() >= 128 {
            self.ice_timeline.pop_front();
        }
        self.ice_timeline.push_back(StatePoint {
            at_ms: at.saturating_duration_since(origin).as_secs_f64() * 1000.0,
            local_epoch: epoch,
            state: state.into(),
        });
    }
    pub fn fallback(&mut self, reason: &str, at: Instant, origin: Instant, offer_at: Instant) {
        self.selected_transport = Some("wss".into());
        if self.fallbacks.len() >= 64 {
            self.fallbacks.pop_front();
        }
        self.fallbacks.push_back(Fallback {
            at_ms: at.saturating_duration_since(origin).as_secs_f64() * 1000.0,
            offer_to_fallback_ms: at.saturating_duration_since(offer_at).as_secs_f64() * 1000.0,
            reason: reason.into(),
        });
    }
}

/// RFC 3550 transit jitter. Sequence accounting uses extended str0m sequence
/// numbers and retains a bounded reorder history; recovered gaps lower loss.
pub struct RtpTracker {
    base: Option<u64>,
    max: u64,
    unique: u64,
    seen: VecDeque<u64>,
    seen_set: HashSet<u64>,
    last_transit: Option<f64>,
    jitter: f64,
    origin: Instant,
}
impl RtpTracker {
    pub fn new(origin: Instant) -> Self {
        Self {
            base: None,
            max: 0,
            unique: 0,
            seen: VecDeque::new(),
            seen_set: HashSet::new(),
            last_transit: None,
            jitter: 0.0,
            origin,
        }
    }
    pub fn packet(
        &mut self,
        seq: u64,
        timestamp: u32,
        frequency: f64,
        at: Instant,
        stats: &mut RtpStats,
    ) {
        if self.seen_set.insert(seq) {
            self.base = Some(self.base.map_or(seq, |base| base.min(seq)));
            self.max = self.max.max(seq);
            self.unique += 1;
            if self.seen.len() >= 4096 {
                if let Some(expired) = self.seen.pop_front() {
                    self.seen_set.remove(&expired);
                }
            }
            self.seen.push_back(seq);
        }
        stats.packets_lost = self
            .base
            .map_or(0, |base| (self.max - base + 1).saturating_sub(self.unique));
        // Difference timestamps modulo 2^32 so wrap cannot produce a jitter spike.
        let transit = at.saturating_duration_since(self.origin).as_secs_f64() * frequency;
        let delta = transit - f64::from(timestamp);
        if let Some(last) = self.last_transit {
            let d = delta - last;
            let wrapped = (d + 2147483648.0).rem_euclid(4294967296.0) - 2147483648.0;
            self.jitter += (wrapped.abs() - self.jitter) / 16.0;
        }
        self.last_transit = Some(delta);
        stats.jitter_ms = Some(self.jitter / frequency * 1000.0);
    }
}
