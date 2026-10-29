//! Binary-owned session boundary; preserves existing streaming policy.
use super::{
    Duration, Instant, MediaCodec, Profile, StreamMode, VideoCodec, WSS_BALANCED_BITRATE_BPS,
    WSS_HIGH_BITRATE_BPS, WSS_SAVER_BITRATE_BPS,
};

#[derive(Debug, Clone, Copy)]
pub(super) struct WssAdjustment {
    pub(super) mode: VideoMode,
    pub(super) target_bps: u32,
    pub(super) reason: &'static str,
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

pub(super) fn valid_media_epoch(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A media start may name only the authenticated session's epoch. A start without an epoch
/// (WebRTC) passes; WSS media then refuses it for the missing epoch.
pub(super) fn media_start_epoch_allowed(requested: Option<&str>, session: Option<&str>) -> bool {
    match requested {
        None => true,
        Some(requested) => valid_media_epoch(requested) && session == Some(requested),
    }
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
    protocol == Some(4) && epoch.is_some_and(valid_media_epoch)
}

pub(super) fn accept_session_codec(current: VideoCodec, requested: VideoCodec) -> bool {
    current == requested || matches!((current, requested), (VideoCodec::Hevc, VideoCodec::H264))
}

/// Poll the one-shot idle snapshot state for a live peer or its first prewarm.
/// The capture state remains authoritative after an IDR is cached because
/// viewport transitions can explicitly re-arm one activation frame.
pub(super) fn should_snapshot_idle_desktop(media_ready: bool, latest_idr_missing: bool) -> bool {
    media_ready || latest_idr_missing
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

/// Socket/channel activity interrupts this bounded housekeeping wait.
pub(super) fn producer_wait_duration(
    now: Instant,
    deadlines: impl IntoIterator<Item = Instant>,
) -> Duration {
    deadlines
        .into_iter()
        .fold(Duration::from_millis(100), |wait, deadline| {
            wait.min(deadline.saturating_duration_since(now))
        })
}

pub(super) fn frame_admission_wake_at(
    next_admission: Instant,
    frame_interval: Duration,
) -> Instant {
    next_admission
        .checked_sub(frame_interval / 2)
        .unwrap_or(next_admission)
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
    /// Local Default pixels need fresh authorization before cached replay.
    /// Secure helper and privileged Agent frames retain their own policy.
    pub(super) requires_default: bool,
    /// Media generation the IDR was encoded in. A cached IDR is never replayed
    /// or reused for admission in another generation: on WebRTC its fresh RTP
    /// timestamp would otherwise pass the client's in-place transition gate
    /// with old-desktop pixels.
    pub(super) generation: u32,
}

impl CachedIdr {
    pub(super) fn belongs_to(&self, generation: u32) -> bool {
        self.generation == generation
    }

    pub(super) fn replay_authorized(&self, default_check: impl FnOnce() -> bool) -> bool {
        !self.requires_default || default_check()
    }
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

/// Host input readiness when a WebRTC offer arrives. The privileged Agent
/// always waits for presentation; the user host waits too when the offer
/// starts a new peer, since its helper can stream the secure desktop. An offer
/// that is not forwarded keeps the user host's previous rule.
pub(super) fn input_ready_on_offer(
    privileged: bool,
    capture_available: bool,
    starts_peer: bool,
) -> bool {
    !privileged && !starts_peer && capture_available
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
    pub(super) rtc_idr_id: Option<u64>,
    /// RTP timestamp of the admitted WebRTC IDR, repeated on `admitted`.
    pub(super) rtc_rtp_timestamp: Option<u32>,
    pub(super) armed_at: Instant,
    pub(super) recovery: FirstFrameRecovery,
}

const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(8);
const FIRST_FRAME_RECOVERY_LIMIT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(super) struct FirstFrameRecovery {
    recovering: bool,
    deadline: Option<Instant>,
}

impl FirstFrameRecovery {
    pub(super) fn expired(
        &mut self,
        armed_at: Instant,
        viewer_attached: bool,
        recovering: bool,
        now: Instant,
    ) -> bool {
        if viewer_attached && recovering {
            self.recovering = true;
            self.deadline = Some(armed_at + FIRST_FRAME_RECOVERY_LIMIT);
        } else if self.recovering {
            self.recovering = false;
            // Give the recovered source time to encode and present, keeping
            // the original total bound even after repeated capture losses.
            self.deadline =
                Some((now + FIRST_FRAME_TIMEOUT).min(armed_at + FIRST_FRAME_RECOVERY_LIMIT));
        }
        now >= self.deadline.unwrap_or(armed_at + FIRST_FRAME_TIMEOUT)
    }
}

impl PendingFirstFrame {
    pub(super) fn rebind_after_recovery(&mut self, generation: u32) {
        self.generation = generation;
        self.admitted = false;
        self.rtc_idr_id = None;
        self.rtc_rtp_timestamp = None;
    }
    pub(super) fn reject_rtc_access_unit(&mut self, id: u64) {
        if !self.admitted && self.rtc_idr_id == Some(id) {
            self.rtc_idr_id = None;
        }
    }
    pub(super) fn admit_rtc_access_unit(&mut self, generation: u32, id: u64) -> bool {
        if self.transport != "webrtc"
            || self.generation != generation
            || self.admitted
            || self.rtc_idr_id != Some(id)
        {
            return false;
        }
        self.admitted = true;
        true
    }
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

#[allow(clippy::too_many_arguments)]
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

/// Relay tickets from the signaling `hello-ok` expire 12 h after issue (worker
/// `TICKET_TTL_MS`, relay-rs `ticket_ttl_ms`), although the signaling socket
/// can stay open for days. The relay then refuses every media and file socket
/// that presents the ticket. Re-register an idle host before that.
pub(super) const RELAY_TICKET_REFRESH_AGE: Duration = Duration::from_secs(11 * 60 * 60);

/// An idle host's signaling registration is old enough that its ticket must be
/// renewed by re-registering.
pub(super) fn relay_ticket_refresh_due(
    registered_at: Instant,
    viewer_attached: bool,
    now: Instant,
) -> bool {
    !viewer_attached && now.saturating_duration_since(registered_at) >= RELAY_TICKET_REFRESH_AGE
}

/// What the session does when a WSS v4 link stopped on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WssLinkEndAction {
    /// The relay keeps refusing this signaling session's ticket or route: end
    /// the session so the next one registers and gets a fresh ticket.
    Reregister,
    /// The relay retired the epoch; the viewer's next media start replaces it.
    AwaitNextStart,
}

pub(super) fn wss_link_end_action(end: &bubbleplay_host::wss_v4::V4LinkEnd) -> WssLinkEndAction {
    match end {
        bubbleplay_host::wss_v4::V4LinkEnd::Refused { .. } => WssLinkEndAction::Reregister,
        bubbleplay_host::wss_v4::V4LinkEnd::Retired { .. } => WssLinkEndAction::AwaitNextStart,
    }
}

/// The capture demand inputs that are false, for a first-frame failure log.
pub(super) fn capture_demand_gaps(
    viewer_attached: bool,
    offer_in_flight: bool,
    offer_accepted: bool,
    ice_connected: bool,
    wss_connected: bool,
    wss_peer_present: bool,
) -> String {
    let mut gaps = Vec::new();
    if !viewer_attached {
        gaps.push("viewer_attached");
    }
    let wss_peer_live = wss_connected && wss_peer_present;
    if !(offer_in_flight || offer_accepted || ice_connected || wss_peer_live) {
        gaps.push("offer_in_flight");
        gaps.push("offer_accepted");
        gaps.push("ice_connected");
        gaps.push(if wss_connected {
            "wss_peer_present"
        } else {
            "wss_connected"
        });
    }
    gaps.join(",")
}

/// No new captured frame for this long while one is needed re-creates the
/// duplication.
pub(super) const CAPTURE_STALL_LIMIT: Duration = Duration::from_secs(5);

/// Capture watchdog. A frame is needed while a viewer waits for its first
/// frame, or while a live viewer has no IDR to decode from. A static desktop
/// with an IDR delivered needs no frames and never arms it.
#[derive(Debug, Default)]
pub(super) struct CaptureStallWatchdog {
    waiting_since: Option<Instant>,
}

impl CaptureStallWatchdog {
    /// A new desktop frame (or idle snapshot) arrived.
    pub(super) fn frame_captured(&mut self) {
        self.waiting_since = None;
    }

    /// Returns the reason once per `CAPTURE_STALL_LIMIT` without a frame while
    /// capture is demanded, available and a frame is needed.
    pub(super) fn poll(
        &mut self,
        demand: bool,
        capture_available: bool,
        first_frame_pending: bool,
        idr_missing: bool,
        now: Instant,
    ) -> Option<&'static str> {
        if !(demand && capture_available && (first_frame_pending || idr_missing)) {
            self.waiting_since = None;
            return None;
        }
        let since = *self.waiting_since.get_or_insert(now);
        if now.saturating_duration_since(since) < CAPTURE_STALL_LIMIT {
            return None;
        }
        // One occurrence per stall window; the next fires a full window later.
        self.waiting_since = Some(now);
        Some(if first_frame_pending {
            "first-frame-pending"
        } else {
            "no-captured-frame"
        })
    }
}

#[cfg(test)]
mod stall_tests {
    use super::*;
    use bubbleplay_host::wss_v4::V4LinkEnd;

    #[test]
    fn idle_hosts_reregister_before_the_relay_ticket_expires() {
        let registered = Instant::now();
        let at = |hours: u64| registered + Duration::from_secs(hours * 60 * 60);
        assert!(!relay_ticket_refresh_due(registered, false, at(10)));
        assert!(relay_ticket_refresh_due(registered, false, at(11)));
        // The 2026-10-25 host held one registration for 58 h.
        assert!(relay_ticket_refresh_due(registered, false, at(58)));
        // A watching viewer is not cut off.
        assert!(!relay_ticket_refresh_due(registered, true, at(58)));
        assert!(RELAY_TICKET_REFRESH_AGE < Duration::from_secs(12 * 60 * 60));
    }

    #[test]
    fn a_refused_media_link_reregisters_and_a_retired_one_waits() {
        let refused = V4LinkEnd::Refused {
            code: 1008,
            reason: String::new(),
            refusals: 5,
        };
        let retired = V4LinkEnd::Retired {
            code: 1001,
            reason: "stale epoch".into(),
        };
        assert_eq!(wss_link_end_action(&refused), WssLinkEndAction::Reregister);
        assert_eq!(
            wss_link_end_action(&retired),
            WssLinkEndAction::AwaitNextStart
        );
    }

    #[test]
    fn demand_gaps_name_the_false_inputs() {
        // The 2026-10-25 log: a WSS viewer whose media socket the relay refused.
        assert_eq!(
            capture_demand_gaps(true, false, false, false, false, false),
            "offer_in_flight,offer_accepted,ice_connected,wss_connected"
        );
        assert_eq!(
            capture_demand_gaps(true, false, false, false, true, false),
            "offer_in_flight,offer_accepted,ice_connected,wss_peer_present"
        );
        assert_eq!(
            capture_demand_gaps(false, false, false, false, true, true),
            "viewer_attached"
        );
        assert_eq!(
            capture_demand_gaps(true, false, false, true, false, false),
            ""
        );
    }

    #[test]
    fn capture_watchdog_fires_once_per_stall_only_while_a_frame_is_needed() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut watchdog = CaptureStallWatchdog::default();
        assert_eq!(watchdog.poll(true, true, true, false, at(0)), None);
        assert_eq!(watchdog.poll(true, true, true, false, at(4_999)), None);
        assert_eq!(
            watchdog.poll(true, true, true, false, at(5_000)),
            Some("first-frame-pending")
        );
        // Once per occurrence: the next fires a full window later.
        assert_eq!(watchdog.poll(true, true, true, false, at(5_001)), None);
        assert_eq!(
            watchdog.poll(true, true, false, true, at(10_000)),
            Some("no-captured-frame")
        );
        // A captured frame restarts the window.
        watchdog.frame_captured();
        assert_eq!(watchdog.poll(true, true, false, true, at(12_000)), None);
        assert_eq!(watchdog.poll(true, true, false, true, at(16_999)), None);
        assert!(watchdog.poll(true, true, false, true, at(17_000)).is_some());
        // Never without demand, during an existing recovery, or on a static
        // desktop whose IDR was delivered.
        for (demand, available, first, missing) in [
            (false, true, true, true),
            (true, false, true, true),
            (true, true, false, false),
        ] {
            let mut watchdog = CaptureStallWatchdog::default();
            assert_eq!(
                watchdog.poll(demand, available, first, missing, at(0)),
                None
            );
            assert_eq!(
                watchdog.poll(demand, available, first, missing, at(60_000)),
                None
            );
        }
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    #[test]
    fn media_start_epoch_must_be_the_authenticated_one() {
        let session = "0123456789abcdef0123456789abcdef";
        let other = "fedcba9876543210fedcba9876543210";
        assert!(media_start_epoch_allowed(Some(session), Some(session)));
        assert!(media_start_epoch_allowed(None, Some(session)));
        // A well-formed epoch that is not the session's is refused.
        assert!(!media_start_epoch_allowed(Some(other), Some(session)));
        assert!(!media_start_epoch_allowed(Some(session), None));
        // A malformed epoch is refused even when it equals the session text.
        assert!(!media_start_epoch_allowed(Some("not-hex"), Some("not-hex")));
    }

    #[test]
    fn first_frame_timeout_extends_only_for_attached_capture_recovery_and_is_bounded() {
        let armed = Instant::now();
        let at = |seconds| armed + Duration::from_secs(seconds);
        let mut normal = FirstFrameRecovery::default();
        assert!(!normal.expired(armed, true, false, at(7)));
        assert!(normal.expired(armed, true, false, at(8)));
        let mut detached = FirstFrameRecovery::default();
        assert!(detached.expired(armed, false, true, at(8)));
        let mut recovering = FirstFrameRecovery::default();
        assert!(!recovering.expired(armed, true, true, at(8)));
        assert!(!recovering.expired(armed, true, true, at(29)));
        assert!(recovering.expired(armed, true, true, at(30)));
    }

    #[test]
    fn recovered_first_frame_has_encode_time_without_resetting_total_bound() {
        let armed = Instant::now();
        let at = |seconds| armed + Duration::from_secs(seconds);
        let mut recovery = FirstFrameRecovery::default();
        assert!(!recovery.expired(armed, true, true, at(8)));
        assert!(!recovery.expired(armed, true, false, at(15)));
        assert!(!recovery.expired(armed, true, false, at(22)));
        assert!(recovery.expired(armed, true, false, at(23)));
        assert!(!recovery.expired(armed, true, true, at(24)));
        assert!(!recovery.expired(armed, true, false, at(28)));
        assert!(recovery.expired(armed, true, false, at(30)));
    }

    #[test]
    fn first_frame_recovery_rebinds_admission_and_preserves_request_and_timer() {
        let armed = Instant::now();
        let mut pending = PendingFirstFrame {
            request_id: "join".into(),
            transport: "webrtc".into(),
            epoch: None,
            generation: 1,
            admitted: true,
            rtc_idr_id: Some(99),
            rtc_rtp_timestamp: None,
            armed_at: armed,
            recovery: FirstFrameRecovery::default(),
        };
        assert!(!pending
            .recovery
            .expired(armed, true, true, armed + Duration::from_secs(9)));
        pending.rebind_after_recovery(2);
        assert_eq!(pending.request_id, "join");
        assert_eq!(pending.armed_at, armed);
        assert_eq!(pending.generation, 2);
        assert!(!pending.admitted);
        assert_eq!(pending.rtc_idr_id, None);
        assert!(pending
            .recovery
            .expired(armed, true, true, armed + Duration::from_secs(30)));
    }

    #[test]
    fn shared_epoch_contract() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/wss-contract.json")).unwrap();
        for case in contract["epochs"].as_array().unwrap() {
            assert_eq!(
                valid_media_epoch(case["value"].as_str().unwrap()),
                case["accepted"].as_bool().unwrap()
            );
        }
    }
}
