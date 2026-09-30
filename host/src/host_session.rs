//! Binary-owned session boundary; preserves existing streaming policy.
use super::{
    BTreeMap, Controller, Duration, Instant, MediaCodec, Profile, StreamMode, VideoCodec,
    WSS_BALANCED_BITRATE_BPS, WSS_HIGH_BITRATE_BPS, WSS_SAVER_BITRATE_BPS,
};

#[derive(Debug, Clone, Copy)]
pub(super) struct WssAdjustment {
    pub(super) mode: VideoMode,
    pub(super) target_bps: u32,
    pub(super) reason: &'static str,
}

#[derive(Debug)]
pub(super) struct WssQualityController {
    pub(super) generation: Option<u32>,
    pub(super) last_sample: Instant,
    pub(super) last_recovered: u64,
    pub(super) last_dependency_losses: u64,
    pub(super) last_decoder_drops: u64,
    pub(super) last_lanes: [u32; 4],
    pub(super) lane_progress_at: [Instant; 4],
    pub(super) last_reconstructed: u32,
    pub(super) unhealthy_since: Option<Instant>,
    pub(super) floor_since: Option<Instant>,
    pub(super) clean_since: Option<Instant>,
    pub(super) last_bitrate_change: Instant,
    pub(super) upgrade_blocked_until: Instant,
    pub(super) last_preencode_skips: u64,
    pub(super) preencode_pressure_until: Option<Instant>,
}

impl WssQualityController {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            last_sample: now,
            last_recovered: 0,
            last_dependency_losses: 0,
            last_decoder_drops: 0,
            generation: None,
            last_lanes: [0; 4],
            lane_progress_at: [now; 4],
            last_reconstructed: 0,
            unhealthy_since: None,
            floor_since: None,
            clean_since: None,
            last_bitrate_change: now,
            upgrade_blocked_until: now,
            last_preencode_skips: 0,
            preencode_pressure_until: None,
        }
    }
    pub(super) fn observe_preencode_skips(&mut self, now: Instant, total: u64) {
        if total > self.last_preencode_skips {
            self.last_preencode_skips = total;
            self.preencode_pressure_until = Some(now + Duration::from_millis(600));
        }
    }
    pub(super) fn sample(
        &mut self,
        now: Instant,
        generation: u32,
        quality: &Controller,
        profiles: &ProfileTransition,
        lanes: [u32; 4],
        reconstructed: u32,
        presented: u32,
        recovered: u64,
        dependency_losses: u64,
        decoder_drops: u64,
    ) -> Option<WssAdjustment> {
        if self.generation != Some(generation) {
            self.generation = Some(generation);
            self.last_lanes = [0; 4];
            self.lane_progress_at = [now; 4];
            self.last_reconstructed = 0;
            self.last_recovered = recovered;
            self.last_dependency_losses = dependency_losses;
            self.last_decoder_drops = decoder_drops;
            self.unhealthy_since = None;
            self.floor_since = None;
            self.clean_since = None;
        }
        for index in 0..lanes.len() {
            if lanes[index] != self.last_lanes[index]
                && sequence_reached(lanes[index], self.last_lanes[index])
            {
                self.lane_progress_at[index] = now;
            }
        }
        // ACKs are batched and four TCP lanes naturally report at different
        // instants. Judge lane health over a real window, never one message.
        let healthy_lanes = self
            .lane_progress_at
            .iter()
            .filter(|&&at| now.saturating_duration_since(at) <= Duration::from_secs(2))
            .count();
        let stream_progressed = reconstructed != self.last_reconstructed
            && sequence_reached(reconstructed, self.last_reconstructed);
        let presentation_lag = reconstructed.wrapping_sub(presented);
        let presentation_backlogged = presentation_lag < (1 << 31) && presentation_lag > 4;
        let preencode_pressure = self
            .preencode_pressure_until
            .is_some_and(|until| now <= until);
        let emergency = dependency_losses > self.last_dependency_losses
            || decoder_drops > self.last_decoder_drops
            || presentation_backlogged
            || preencode_pressure;
        // Reconstructing from parity is a successful delivery, not loss. It is
        // intentionally diagnostic only; expiry/backlog/decoder drops are the
        // signals that the recovered stream failed to stay healthy.
        let degraded = stream_progressed && healthy_lanes < 3;
        let unhealthy = emergency || degraded;
        self.last_sample = now;
        self.last_recovered = recovered;
        self.last_dependency_losses = dependency_losses;
        self.last_lanes = lanes;
        self.last_reconstructed = reconstructed;
        self.last_decoder_drops = decoder_drops;
        if unhealthy {
            self.clean_since = None;
            let since = self.unhealthy_since.get_or_insert(now);
            let stream_mode = profiles.stream_mode();
            let floor = wss_mode_range(profiles.active, stream_mode).0;
            if quality.target() <= floor {
                self.floor_since.get_or_insert(now);
            }
            let bitrate_hold = Duration::from_secs(match stream_mode {
                StreamMode::Work => 8,
                StreamMode::Game => 3,
            });
            if now.duration_since(*since) >= bitrate_hold {
                let target = (quality.target().saturating_mul(85) / 100).max(floor);
                if target < quality.target() {
                    self.unhealthy_since = None;
                    self.last_bitrate_change = now;
                    self.upgrade_blocked_until = now
                        + Duration::from_secs(match stream_mode {
                            StreamMode::Work => 60,
                            StreamMode::Game => 30,
                        });
                    self.floor_since = None;
                    return Some(WssAdjustment {
                        mode: profiles.active,
                        target_bps: target,
                        reason: "wss-congestion",
                    });
                }
                if self.floor_since.is_some_and(|floor_since| {
                    now.duration_since(floor_since)
                        >= Duration::from_secs(match stream_mode {
                            StreamMode::Work => 12,
                            StreamMode::Game => 6,
                        })
                }) {
                    let mode = wss_mode_down(profiles.active, stream_mode);
                    if mode != profiles.active {
                        self.floor_since = None;
                        self.unhealthy_since = None;
                        self.clean_since = None;
                        self.upgrade_blocked_until = now
                            + Duration::from_secs(match stream_mode {
                                StreamMode::Work => 60,
                                StreamMode::Game => 30,
                            });
                        return Some(WssAdjustment {
                            mode,
                            target_bps: wss_mode_range(mode, stream_mode).1.min(quality.target()),
                            reason: "wss-sustained-down",
                        });
                    }
                }
            }
            return None;
        }
        self.unhealthy_since = None;
        self.floor_since = None;
        let clean = self.clean_since.get_or_insert(now);
        if now >= self.upgrade_blocked_until
            && now.duration_since(*clean)
                >= Duration::from_secs(match profiles.stream_mode() {
                    StreamMode::Work => 20,
                    StreamMode::Game => 10,
                })
            && now.duration_since(self.last_bitrate_change) >= Duration::from_secs(10)
        {
            let ceiling = wss_mode_range(profiles.active, profiles.stream_mode()).1;
            let target = (quality.target().saturating_mul(110) / 100).min(ceiling);
            if target > quality.target() {
                self.last_bitrate_change = now;
                return Some(WssAdjustment {
                    mode: profiles.active,
                    target_bps: target,
                    reason: "wss-probe",
                });
            }
        }
        if now >= self.upgrade_blocked_until
            && now.duration_since(*clean)
                >= Duration::from_secs(match profiles.stream_mode() {
                    StreamMode::Work => 60,
                    StreamMode::Game => 30,
                })
        {
            let mode = wss_mode_up(profiles.active, profiles.stream_mode());
            if mode != profiles.active
                && quality.target() >= wss_mode_range(mode, profiles.stream_mode()).0
            {
                self.clean_since = None;
                return Some(WssAdjustment {
                    mode,
                    target_bps: quality
                        .target()
                        .max(wss_mode_range(mode, profiles.stream_mode()).0),
                    reason: "wss-stable-up",
                });
            }
        }
        None
    }
}

pub(super) fn wss_codec(codec: VideoCodec) -> MediaCodec {
    match codec {
        VideoCodec::Hevc => MediaCodec::Hevc,
        VideoCodec::H264 => MediaCodec::H264,
    }
}

pub(super) fn parse_media_codec(value: Option<&str>) -> Option<VideoCodec> {
    match value {
        Some("hevc") => Some(VideoCodec::Hevc),
        Some("h264") => Some(VideoCodec::H264),
        _ => None,
    }
}

pub(super) fn valid_v2_epoch(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn should_replace_wss_media_session(
    has_active_media: bool,
    active_epoch: Option<&str>,
    requested_epoch: &str,
) -> bool {
    has_active_media && active_epoch != Some(requested_epoch)
}

pub(super) fn valid_monitor_refresh_control(
    protocol: Option<u8>,
    epoch: Option<&str>,
    _active_wss_epoch: Option<&str>,
) -> bool {
    matches!(protocol, Some(2..=4)) && epoch.is_some_and(valid_v2_epoch)
}

pub(super) fn valid_stripe_reconnect_recovery_control(
    action: &str,
    reason: Option<&str>,
    protocol: Option<u8>,
    epoch: Option<&str>,
    active_wss_epoch: Option<&str>,
) -> bool {
    action == "keyframe"
        && reason == Some("stripe-reconnect")
        && protocol == Some(3)
        && epoch.is_some_and(|value| {
            valid_v2_epoch(value) && active_wss_epoch.is_some_and(|active| active == value)
        })
}

pub(super) fn accept_session_codec(current: VideoCodec, requested: VideoCodec) -> bool {
    current == requested || matches!((current, requested), (VideoCodec::Hevc, VideoCodec::H264))
}

pub(super) struct WssAckRate {
    pub(super) generation: u32,
    pub(super) pending: BTreeMap<u32, usize>,
    pub(super) window_started: Instant,
    pub(super) acked_bytes: u64,
}

impl WssAckRate {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            generation: 0,
            pending: BTreeMap::new(),
            window_started: now,
            acked_bytes: 0,
        }
    }
    pub(super) fn submitted(&mut self, generation: u32, sequence: u32, bytes: usize) {
        if generation != self.generation {
            self.generation = generation;
            self.pending.clear();
            self.acked_bytes = 0;
            self.window_started = Instant::now();
        }
        self.pending.insert(sequence, bytes);
        while self.pending.len() > 256 {
            if let Some(oldest) = self.pending.keys().next().copied() {
                self.pending.remove(&oldest);
            }
        }
    }
    pub(super) fn acknowledged(
        &mut self,
        now: Instant,
        generation: u32,
        sequence: u32,
    ) -> Option<u32> {
        if generation != self.generation {
            return None;
        }
        let acknowledged: Vec<u32> = self
            .pending
            .range(..=sequence)
            .map(|(&key, _)| key)
            .collect();
        if acknowledged.is_empty() {
            return None;
        }
        for key in acknowledged {
            if let Some(bytes) = self.pending.remove(&key) {
                self.acked_bytes = self.acked_bytes.saturating_add(bytes as u64);
            }
        }
        let elapsed = now.duration_since(self.window_started);
        if elapsed < Duration::from_secs(1) {
            return None;
        }
        let bps = ((self.acked_bytes as f64 * 8.0) / elapsed.as_secs_f64()) as u64;
        self.window_started = now;
        self.acked_bytes = 0;
        Some(bps.min(u32::MAX as u64) as u32)
    }
}

/// WSS ACK throughput is capped by the bytes the encoder chose to produce, so
/// it is not a direct bandwidth estimate. The adaptive controller keeps 20%
/// headroom (`target = capacity * 0.8`); report at least the inverse of the
/// current target or that headroom would recursively reduce the target even
/// while every submitted frame is arriving.
pub(super) fn wss_capacity_observation(delivery_bps: u32, encoder_target_bps: u32) -> u32 {
    let target_with_headroom = (encoder_target_bps as u64)
        .saturating_mul(5)
        .div_ceil(4)
        .min(u32::MAX as u64) as u32;
    delivery_bps.max(target_with_headroom)
}

/// Poll the one-shot idle snapshot state only after a peer can receive it.
/// The capture state remains authoritative after an IDR is cached because
/// viewport transitions can explicitly re-arm one activation frame.
pub(super) fn should_snapshot_idle_desktop(ice_connected: bool, _latest_idr_missing: bool) -> bool {
    ice_connected
}

#[derive(Debug, Default)]
pub(super) struct WssRecoverySnapshotGate {
    pub(super) last_request: Option<Instant>,
}

impl WssRecoverySnapshotGate {
    pub(super) fn should_request(
        &mut self,
        now: Instant,
        needs_recovery_idr: bool,
        awaiting_recovery_ack: bool,
        retry_interval: Duration,
    ) -> bool {
        if !needs_recovery_idr && !awaiting_recovery_ack {
            self.last_request = None;
            return false;
        }
        if !needs_recovery_idr {
            return false;
        }
        if self
            .last_request
            .is_some_and(|requested| now.duration_since(requested) < retry_interval)
        {
            return false;
        }
        self.last_request = Some(now);
        true
    }
}

pub(super) fn capture_admission_ready(
    capture_available: bool,
    privileged_probe_allowed: bool,
    negotiation_active: bool,
    ice_connected: bool,
    wss_active: bool,
    rtc_reserved: bool,
    prewarm_needed: bool,
) -> bool {
    (capture_available || privileged_probe_allowed)
        && (wss_active
            || (ice_connected && rtc_reserved)
            || (negotiation_active && !ice_connected && prewarm_needed))
}

pub(super) fn privileged_no_frame_retry_delay(capture_available: bool) -> Duration {
    if capture_available {
        Duration::from_millis(1)
    } else {
        Duration::from_millis(250)
    }
}

pub(super) fn encoder_benchmark_contract_changed(
    encoder_available: bool,
    applied_profile_revision: Option<u64>,
    desired_profile_revision: Option<u64>,
) -> bool {
    encoder_available && applied_profile_revision != desired_profile_revision
}

pub(super) fn producer_poll_interval(media_demand_active: bool) -> Duration {
    if media_demand_active {
        Duration::from_millis(1)
    } else {
        Duration::from_secs(1)
    }
}

pub(super) fn frame_admission_due(
    now: Instant,
    next_admission: Instant,
    frame_interval: Duration,
    urgent: bool,
) -> bool {
    urgent
        || now
            .checked_add(frame_interval / 2)
            .is_none_or(|nearest| nearest >= next_admission)
}

pub(super) fn advance_frame_deadline(
    previous: Instant,
    now: Instant,
    frame_interval: Duration,
) -> Instant {
    let scheduled = previous.checked_add(frame_interval).unwrap_or(now);
    if scheduled
        .checked_add(frame_interval)
        .is_some_and(|stale| stale < now)
    {
        now.checked_add(frame_interval).unwrap_or(now)
    } else {
        scheduled
    }
}

/// Bounded policy for replaying the latest IDR when desktop duplication has
/// no new presents. It never schedules a stream of static frames.
#[derive(Default)]
pub(super) struct CachedIdrReplay {
    pub(super) initial_frame_pending: bool,
    pub(super) recovery_requested_at: Option<Instant>,
}

/// Keeps controller intent separate from dimensions actually configured in
/// NVENC and from dimensions already admitted by the RTC sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VideoMode {
    pub(super) profile: Profile,
    pub(super) label: &'static str,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) fps: u32,
}
impl VideoMode {
    pub(super) fn rung(profile: Profile) -> Self {
        let (width, height, fps) = profile.dimensions();
        Self {
            profile,
            label: profile.name(),
            width,
            height,
            fps,
        }
    }
    pub(super) fn for_stream(profile: Profile, mode: StreamMode) -> Self {
        let (width, height) = match profile {
            Profile::Mobile => (854, 480),
            Profile::Low => (960, 540),
            Profile::Balanced => (1280, 720),
            Profile::Full => (1920, 1080),
        };
        let fps = match (mode, profile) {
            (_, Profile::Mobile) => 24,
            (StreamMode::Work, Profile::Low) => 24,
            (StreamMode::Work, _) => 30,
            (StreamMode::Game, Profile::Low) => 30,
            (StreamMode::Game, Profile::Balanced | Profile::Full) => 60,
        };
        let label = match (profile, fps) {
            (Profile::Mobile, _) => "480p24",
            (Profile::Low, 24) => "540p24",
            (Profile::Low, _) => "540p30",
            (Profile::Balanced, 60) => "720p60",
            (Profile::Balanced, _) => "720p30",
            (Profile::Full, 30) => "1080p30",
            (Profile::Full, _) => "1080p60",
        };
        Self {
            profile,
            label,
            width,
            height,
            fps,
        }
    }
    pub(super) fn native(width: u32, height: u32, fps: u32) -> Self {
        Self {
            profile: Profile::Full,
            label: "native",
            width,
            height,
            fps,
        }
    }
}
pub(super) struct ProfileTransition {
    pub(super) active: VideoMode,
    pub(super) staged: Option<(VideoMode, &'static str)>,
    pub(super) announced: Option<VideoMode>,
    pub(super) announcement_reason: &'static str,
    pub(super) pinned: bool,
    pub(super) stream_mode: Option<StreamMode>,
}

pub(super) struct CachedIdr {
    pub(super) data: Vec<u8>,
    pub(super) mode: VideoMode,
}

/// A profile IDR has been encoded and accepted by the one-slot AU queue, but
/// is not applied until the transport confirms that exact AU reached str0m.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PendingSubmittedIdr {
    pub(super) id: u64,
    pub(super) mode: VideoMode,
    pub(super) reason: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PendingWssIdr {
    pub(super) generation: u32,
    pub(super) sequence: u32,
    pub(super) mode: VideoMode,
    pub(super) reason: &'static str,
}

pub(super) fn sequence_reached(current: u32, target: u32) -> bool {
    current.wrapping_sub(target) < (1 << 31)
}

pub(super) fn next_media_sequence(current: u32) -> u32 {
    current.wrapping_add(1)
}

pub(super) fn commit_media_sequence(current: &mut u32, admitted: u32) {
    *current = admitted;
}

pub(super) fn take_matching_transition(
    pending: &mut Option<PendingSubmittedIdr>,
    id: u64,
) -> Option<PendingSubmittedIdr> {
    pending
        .as_ref()
        .is_some_and(|candidate| candidate.id == id)
        .then(|| pending.take())
        .flatten()
}

/// Select a recovery contract before touching the encoder.  A usable scaler
/// lets us return to the last *exact* mode; otherwise only the exact capture
/// dimensions are safe.
pub(super) fn fallback_mode(
    old: VideoMode,
    native_width: u32,
    native_height: u32,
    scaler_available: bool,
) -> VideoMode {
    if scaler_available {
        old
    } else {
        VideoMode::native(native_width, native_height, old.fps)
    }
}

pub(super) fn is_fallback_reason(reason: &str) -> bool {
    reason.ends_with("fallback")
}

/// Commit only the AU that the transport actually admitted.  Fallbacks become
/// pinned at that point, never while their IDR is merely queued or encoded.
pub(super) fn commit_matching_transition(
    profiles: &mut ProfileTransition,
    pending: PendingSubmittedIdr,
    admitted_id: u64,
) -> bool {
    if pending.id != admitted_id {
        return false;
    }
    if is_fallback_reason(pending.reason) {
        profiles.pinned = true;
    }
    profiles.commit_mode(pending.mode, pending.reason);
    true
}

pub(super) fn cached_replay_transition(
    profiles: &ProfileTransition,
    cached_mode: VideoMode,
    id: u64,
) -> Option<PendingSubmittedIdr> {
    profiles
        .staged
        .filter(|(staged, _)| *staged == cached_mode)
        .map(|(mode, reason)| PendingSubmittedIdr { id, mode, reason })
}

pub(super) fn rejected_transition_requires_recovery(
    pending: &Option<PendingSubmittedIdr>,
    id: u64,
) -> bool {
    pending.as_ref().is_some_and(|candidate| candidate.id == id)
}

pub(super) fn should_forward_offer(
    ice_connected: bool,
    offer_accepted: bool,
    offer_in_flight: bool,
) -> bool {
    !ice_connected && !offer_accepted && !offer_in_flight
}

impl ProfileTransition {
    #[cfg(test)]
    pub(super) fn new(active: Profile, desired: Profile) -> Self {
        let active = VideoMode::rung(active);
        let desired = VideoMode::rung(desired);
        Self {
            active,
            staged: (desired != active).then_some((desired, "startup")),
            announced: None,
            announcement_reason: "startup",
            pinned: false,
            stream_mode: None,
        }
    }

    pub(super) fn new_for_mode(active: Profile, desired: Profile, stream_mode: StreamMode) -> Self {
        let active = VideoMode::for_stream(active, stream_mode);
        let desired = VideoMode::for_stream(desired, stream_mode);
        Self {
            active,
            staged: (desired != active).then_some((desired, "startup")),
            announced: None,
            announcement_reason: "startup",
            pinned: false,
            stream_mode: Some(stream_mode),
        }
    }

    pub(super) fn stream_mode(&self) -> StreamMode {
        self.stream_mode.unwrap_or(StreamMode::Work)
    }

    pub(super) fn set_stream_mode(&mut self, mode: StreamMode) -> bool {
        self.stream_mode = Some(mode);
        self.stage_mode(
            VideoMode::for_stream(self.active.profile, mode),
            "stream-mode",
        )
    }

    #[cfg(test)]
    pub(super) fn active(&self) -> Profile {
        self.active.profile
    }
    pub(super) fn staged(&self) -> Option<Profile> {
        self.staged.map(|(mode, _)| mode.profile)
    }
    #[cfg(test)]
    pub(super) fn active_fps(&self) -> u32 {
        self.active.fps
    }
    #[cfg(test)]
    pub(super) fn announced(&self) -> Option<Profile> {
        self.announced.map(|mode| mode.profile)
    }
    #[cfg(test)]
    pub(super) fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Returns true when an unapplied dimension change is now staged. The
    /// caller uses this to invalidate any IDR cached at the old dimensions.
    pub(super) fn stage(&mut self, desired: Profile, reason: &'static str) -> bool {
        let mode = self
            .stream_mode
            .map(|stream| VideoMode::for_stream(desired, stream))
            .unwrap_or_else(|| VideoMode::rung(desired));
        self.stage_mode(mode, reason)
    }

    pub(super) fn stage_mode(&mut self, desired_mode: VideoMode, reason: &'static str) -> bool {
        if self.pinned {
            // A scaler fallback has no trustworthy alternate texture path.
            // Keep the exact native encoder contract until this session ends;
            // bitrate control remains independent in apply_quality_decision.
            self.staged = None;
            return false;
        }
        if desired_mode == self.active {
            self.staged = None;
            return false;
        }
        self.staged = Some((desired_mode, reason));
        true
    }

    #[cfg(test)]
    pub(super) fn commit(&mut self, profile: Profile, fps: u32, reason: &'static str) {
        self.commit_mode(
            VideoMode {
                fps,
                ..self
                    .stream_mode
                    .map(|stream| VideoMode::for_stream(profile, stream))
                    .unwrap_or_else(|| VideoMode::rung(profile))
            },
            reason,
        );
    }
    pub(super) fn commit_mode(&mut self, mode: VideoMode, reason: &'static str) {
        self.active = mode;
        self.staged = None;
        self.announcement_reason = reason;
    }

    #[cfg(test)]
    pub(super) fn pin(&mut self, profile: Profile, fps: u32, reason: &'static str) {
        self.pinned = true;
        self.commit(profile, fps, reason);
    }

    #[cfg(test)]
    pub(super) fn pin_native(&mut self, width: u32, height: u32, fps: u32, reason: &'static str) {
        self.pinned = true;
        self.commit_mode(VideoMode::native(width, height, fps), reason);
    }

    pub(super) fn pending_announcement(&self) -> Option<(VideoMode, &'static str)> {
        (self.announced != Some(self.active)).then_some((self.active, self.announcement_reason))
    }

    pub(super) fn on_idr_admitted(&mut self) {
        self.announced = Some(self.active);
    }
}

pub(super) struct PendingFirstFrame {
    pub(super) request_id: String,
    pub(super) transport: String,
    pub(super) epoch: Option<String>,
    pub(super) generation: u32,
    pub(super) admitted: bool,
    pub(super) armed_at: Instant,
}

impl PendingFirstFrame {
    /// Bind a WSS startup request to the generation of the first IDR that was
    /// actually accepted by the current media egress. Viewport and codec
    /// transitions may legitimately advance the generation after the request
    /// was armed but before that IDR is queued.
    pub(super) fn admit_wss_access_unit(&mut self, generation: u32, is_idr: bool) -> bool {
        if self.transport != "wss" || self.admitted || !is_idr {
            return false;
        }
        self.generation = generation;
        self.admitted = true;
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PendingViewportIdr {
    pub(super) generation: u32,
    pub(super) sequence: u32,
    pub(super) crop: ViewportCrop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ViewportCrop {
    pub(super) revision: u32,
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) width: u32,
    pub(super) height: u32,
}

pub(super) fn same_viewport_geometry(left: ViewportCrop, right: ViewportCrop) -> bool {
    left.x == right.x
        && left.y == right.y
        && left.width == right.width
        && left.height == right.height
}

pub(super) fn viewport_from_q16(
    revision: u32,
    enabled: bool,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    desktop_width: u32,
    desktop_height: u32,
) -> Option<ViewportCrop> {
    if !enabled {
        return Some(ViewportCrop {
            revision,
            x: 0,
            y: 0,
            width: desktop_width,
            height: desktop_height,
        });
    }
    if desktop_width < 320 || desktop_height < 180 || width == 0 || height == 0 {
        return None;
    }
    let to_px =
        |value: u16, extent: u32| -> u32 { ((value as u64 * extent as u64) / 65535) as u32 };
    let mut crop = ViewportCrop {
        revision,
        x: to_px(x, desktop_width),
        y: to_px(y, desktop_height),
        width: to_px(width, desktop_width),
        height: to_px(height, desktop_height),
    };
    crop.width = crop.width.max(320).min(desktop_width);
    crop.height = crop.height.max(180).min(desktop_height);
    crop.x = crop.x.min(desktop_width.saturating_sub(crop.width));
    crop.y = crop.y.min(desktop_height.saturating_sub(crop.height));
    crop.x &= !1;
    crop.y &= !1;
    crop.width &= !1;
    crop.height &= !1;
    (crop.width >= 320 && crop.height >= 180).then_some(crop)
}

pub(super) fn wss_preset(
    value: Option<&str>,
    max_bps: u32,
    stream_mode: StreamMode,
) -> WssAdjustment {
    let (mode, target_bps) = match value {
        Some("balanced") => (
            VideoMode::for_stream(Profile::Balanced, stream_mode),
            if stream_mode == StreamMode::Game {
                4_500_000
            } else {
                WSS_BALANCED_BITRATE_BPS
            },
        ),
        Some("saver") => (
            VideoMode::for_stream(Profile::Low, stream_mode),
            WSS_SAVER_BITRATE_BPS,
        ),
        _ => (
            VideoMode::for_stream(Profile::Full, stream_mode),
            if stream_mode == StreamMode::Game {
                8_000_000
            } else {
                WSS_HIGH_BITRATE_BPS
            },
        ),
    };
    WssAdjustment {
        mode,
        target_bps: target_bps.min(max_bps),
        reason: "wss-preset",
    }
}

pub(super) fn wss_mode_range(mode: VideoMode, stream_mode: StreamMode) -> (u32, u32) {
    match (stream_mode, mode.profile) {
        (_, Profile::Mobile) => (350_000, 1_000_000),
        (_, Profile::Low) => (800_000, 2_000_000),
        (StreamMode::Work, Profile::Balanced) => (1_500_000, 4_000_000),
        (StreamMode::Work, Profile::Full) => (3_200_000, 8_000_000),
        (StreamMode::Game, Profile::Balanced) => (3_000_000, 7_000_000),
        (StreamMode::Game, Profile::Full) => (6_000_000, 12_000_000),
    }
}

pub(super) fn wss_mode_down(mode: VideoMode, stream_mode: StreamMode) -> VideoMode {
    match mode.profile {
        Profile::Full => VideoMode::for_stream(Profile::Balanced, stream_mode),
        Profile::Balanced => VideoMode::for_stream(Profile::Low, stream_mode),
        Profile::Low => VideoMode::for_stream(Profile::Mobile, stream_mode),
        Profile::Mobile => mode,
    }
}

pub(super) fn wss_mode_up(mode: VideoMode, stream_mode: StreamMode) -> VideoMode {
    match mode.profile {
        Profile::Mobile => VideoMode::for_stream(Profile::Low, stream_mode),
        Profile::Low => VideoMode::for_stream(Profile::Balanced, stream_mode),
        Profile::Balanced => VideoMode::for_stream(Profile::Full, stream_mode),
        Profile::Full => mode,
    }
}

#[cfg(test)]
pub(super) fn active_encoder_dimensions(profiles: &ProfileTransition) -> (u32, u32, u32) {
    (
        profiles.active.width,
        profiles.active.height,
        profiles.active.fps,
    )
}

pub(super) fn encoder_mode_for_capture(
    profiles: &mut ProfileTransition,
    native_width: u32,
    native_height: u32,
) -> VideoMode {
    // On a fresh device, the staged mode is the next texture we will prepare.
    // It is therefore the only safe non-native encoder bootstrap candidate.
    let candidate = profiles
        .staged
        .map(|(mode, _)| mode)
        .unwrap_or(profiles.active);
    if candidate.width <= native_width && candidate.height <= native_height {
        return candidate;
    }

    // NVENC's max dimensions are bounded by the captured texture. A larger
    // rung cannot be initialized or reconfigured on this device, so make the
    // actual native contract explicit before any texture or encoder work.
    let native = VideoMode::native(native_width, native_height, candidate.fps);
    profiles.stage_mode(native, "native-size-fallback");
    native
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    #[test]
    fn shared_epoch_contract() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/wss-contract.json")).unwrap();
        for case in contract["epochs"].as_array().unwrap() {
            assert_eq!(
                valid_v2_epoch(case["value"].as_str().unwrap()),
                case["accepted"].as_bool().unwrap()
            );
        }
    }
}
