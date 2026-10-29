//! Binary-owned capture boundary; preserves existing streaming policy.
use super::{
    capture, pick_monitor, warn, Args, Capture, Context, DesktopRegion, Duration, Instant, Result,
};
use bubbleplay_host::secure_helper::client::{SecureHelperLink, SecureState};
use std::sync::{Arc, Mutex};

pub(super) type SharedSecureHelper = Arc<Mutex<SecureHelperLink>>;

/// An overdue snapshot timer publishes one wake. If transport admission
/// blocks its attempt, capacity notifications (or bounded housekeeping)
/// retry the still-due work without repeatedly publishing the same timer.
#[derive(Default)]
struct SnapshotDeadlineWake {
    notified: Option<Instant>,
}

impl SnapshotDeadlineWake {
    fn deadline(&mut self, deadline: Option<Instant>, now: Instant) -> Option<Instant> {
        let deadline = deadline?;
        if deadline > now {
            return Some(deadline);
        }
        if self.notified == Some(deadline) {
            return None;
        }
        self.notified = Some(deadline);
        Some(deadline)
    }
}

fn finish_normal_recovery(pending: &mut bool, recovered: bool) -> bool {
    let pending = std::mem::take(pending);
    recovered || pending
}

const JOIN_RECOVERY_GRACE: Duration = Duration::from_secs(2);

/// Status replay waits briefly for a recoverable Default source. This never
/// grants media/input authorization: desktop_state still requires a frame.
#[derive(Default)]
struct JoinRecoveryWindow {
    deadline: Option<Instant>,
    exhausted: bool,
}

impl JoinRecoveryWindow {
    fn begin(&mut self, secure: bool, recoverable: bool, now: Instant) {
        if !secure && recoverable && !self.exhausted && self.deadline.is_none() {
            self.deadline = Some(now + JOIN_RECOVERY_GRACE);
        }
    }

    fn suppress(
        &self,
        available: bool,
        secure: bool,
        recoverable: bool,
        viewer_attached: bool,
        now: Instant,
    ) -> bool {
        !available
            && !secure
            && recoverable
            && (!viewer_attached || self.deadline.is_some_and(|deadline| now < deadline))
    }

    fn take_failed(
        &mut self,
        available: bool,
        secure: bool,
        recoverable: bool,
        now: Instant,
    ) -> bool {
        if secure {
            self.deadline = None;
            return false;
        }
        if available {
            if self.deadline.is_some_and(|deadline| now >= deadline) {
                self.deadline = None;
                self.exhausted = true;
            }
            return false;
        }
        if self
            .deadline
            .is_some_and(|deadline| !recoverable || now >= deadline)
        {
            self.deadline = None;
            self.exhausted = true;
            return true;
        }
        false
    }
}

pub(super) enum CaptureSource {
    Local(LocalCaptureSource),
}

pub(super) struct LocalCaptureSource {
    pub(super) capture: Option<Capture>,
    pub(super) selector: String,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) region: DesktopRegion,
    pub(super) idle_snapshot_requested: bool,
    pub(super) secure_helper: SharedSecureHelper,
    pub(super) secure_state: Option<SecureState>,
    pub(super) secure_streaming: bool,
    secure_upload: Option<capture::SecureFrameUpload>,
    normal_recovery_pending: bool,
    secure_loss_pending: bool,
    next_secure_kind_probe: Instant,
    next_desktop_check: Instant,
    desktop_default: bool,
    helper_secure: bool,
    event_activity: Option<bubbleplay_host::activity::Activity>,
    capture_demand: bool,
    snapshot_wake: SnapshotDeadlineWake,
    join_recovery: JoinRecoveryWindow,
}

impl LocalCaptureSource {
    pub(super) fn unavailable_reason(&self) -> String {
        if self.secure_state.as_ref().is_some_and(|state| state.secure) {
            return "secure-desktop".into();
        }
        self.capture
            .as_ref()
            .and_then(Capture::unavailable_reason)
            .map_or_else(
                || "awaiting-default-frame".into(),
                |reason| reason.to_string(),
            )
    }

    pub(super) fn open(selector: Option<&str>) -> Result<Self> {
        // Monitor enumeration is cheap and does not create D3D/DDA resources.
        // Resolve geometry now so signaling and cursor contracts remain
        // truthful while the actual duplication session stays lazy.
        let monitor = pick_monitor(selector).context("select capture monitor")?;
        let selector = monitor.device_name().unwrap_or_else(|_| "primary".into());
        let width = monitor.width().context("capture monitor width")?;
        let height = monitor.height().context("capture monitor height")?;
        let region = capture::monitor_region(monitor).context("capture monitor bounds")?;
        Ok(Self {
            capture: None,
            selector,
            width,
            height,
            region,
            idle_snapshot_requested: false,
            secure_helper: Arc::new(Mutex::new(SecureHelperLink::new())),
            secure_state: None,
            secure_streaming: false,
            secure_upload: None,
            normal_recovery_pending: false,
            secure_loss_pending: false,
            next_secure_kind_probe: Instant::now(),
            next_desktop_check: Instant::now(),
            desktop_default: false,
            helper_secure: false,
            event_activity: None,
            capture_demand: false,
            snapshot_wake: SnapshotDeadlineWake::default(),
            join_recovery: JoinRecoveryWindow::default(),
        })
    }

    pub(super) fn ensure_capture(&mut self) -> Result<&mut Capture> {
        if self.capture.is_none() {
            let mut capture = Capture::new(Some(&self.selector)).context("capture init")?;
            if let Some(activity) = self.event_activity.as_ref() {
                capture.enable_event_capture(activity.clone());
            }
            self.width = capture.width();
            self.height = capture.height();
            self.region = capture.region();
            if self.idle_snapshot_requested {
                capture.request_idle_snapshot();
                self.idle_snapshot_requested = false;
            }
            self.capture = Some(capture);
        }
        Ok(self.capture.as_mut().expect("capture initialized"))
    }

    pub(super) fn monitors(&self) -> Result<Vec<capture::MonitorInfo>> {
        capture::enumerate_monitors(Some(&self.selector))
    }

    pub(super) fn switch_monitor(&mut self, selector: &str) -> Result<()> {
        if self.secure_state.as_ref().is_some_and(|state| state.secure) {
            anyhow::bail!("secure desktop active; monitor selection is suspended until recovery");
        }
        if let Some(capture) = self.capture.as_mut() {
            capture.switch_monitor(selector)?;
            self.selector = capture.monitor_selector().to_string();
            self.width = capture.width();
            self.height = capture.height();
            self.region = capture.region();
            return Ok(());
        }

        let monitor = pick_monitor(Some(selector)).context("select capture monitor")?;
        self.selector = monitor
            .device_name()
            .unwrap_or_else(|_| selector.to_string());
        self.width = monitor.width().context("capture monitor width")?;
        self.height = monitor.height().context("capture monitor height")?;
        self.region = capture::monitor_region(monitor).context("capture monitor bounds")?;
        Ok(())
    }

    pub(super) fn request_idle_snapshot(&mut self) {
        self.idle_snapshot_requested = true;
        if let Some(activity) = self.event_activity.as_ref() {
            activity.notify();
        }
        if let Some(capture) = self.capture.as_mut() {
            capture.request_idle_snapshot();
            self.idle_snapshot_requested = false;
        }
    }

    pub(super) fn take_availability_transition(&mut self) -> Option<capture::CaptureAvailability> {
        if std::mem::take(&mut self.secure_loss_pending) {
            self.join_recovery.deadline = None;
            if let Some(capture) = self.capture.as_mut() {
                let _ = capture.take_availability_transition();
            }
            return Some(capture::CaptureAvailability::Unavailable);
        }
        let transition = self
            .capture
            .as_mut()
            .and_then(Capture::take_availability_transition);
        let available = !self.normal_recovery_pending
            && self.capture.as_ref().is_none_or(|capture| {
                capture.availability() == capture::CaptureAvailability::Active
            });
        let secure = self.secure_state.as_ref().is_some_and(|state| state.secure);
        let recoverable = self.can_recover_for_join();
        let now = Instant::now();
        if self
            .join_recovery
            .take_failed(available, secure, recoverable, now)
        {
            return Some(capture::CaptureAvailability::Unavailable);
        }
        transition
    }

    pub(super) fn unavailable_announcement_deferred(
        &self,
        viewer_attached: bool,
        now: Instant,
    ) -> bool {
        let secure = self.secure_state.as_ref().is_some_and(|state| state.secure);
        self.join_recovery.suppress(
            false,
            secure,
            self.can_recover_for_join(),
            viewer_attached,
            now,
        )
    }

    fn can_recover_for_join(&self) -> bool {
        self.capture.as_ref().is_none_or(|capture| {
            capture.can_recover_for_join()
                || capture.unavailable_reason()
                    == Some(capture::CaptureUnavailableReason::DisplayOff)
        })
    }

    fn tick_secure(&mut self, viewer: bool, now: Instant) {
        let mut link = self
            .secure_helper
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        link.tick(viewer, self.region, now);
        let next = link.state();
        drop(link);
        // Helper transitions are immediate. The fallback host desktop probe
        // is bounded to 250 ms rather than repeated on every producer wake.
        // Compare raw helper observations, not the merged fallback state:
        // without a helper that fallback can stay secure while next is None.
        let helper_secure = next.as_ref().is_some_and(|state| state.secure);
        let helper_transition = self.helper_secure != helper_secure;
        self.helper_secure = helper_secure;
        if now >= self.next_desktop_check || helper_transition {
            self.desktop_default = bubbleplay_host::desktop_session::input_desktop_is_default();
            self.next_desktop_check = now + Duration::from_millis(250);
        }
        let desktop_default = self.desktop_default;
        let mut fallback_kind = self
            .secure_state
            .as_ref()
            .map_or("other", |state| state.kind);
        if !desktop_default
            && !next.as_ref().is_some_and(|state| state.secure)
            && now >= self.next_secure_kind_probe
        {
            fallback_kind =
                bubbleplay_host::desktop_session::secure_desktop_kind().unwrap_or("other");
            self.next_secure_kind_probe = now + capture::CAPTURE_RECOVERY_INTERVAL;
        }
        self.observe_desktop(next, desktop_default, fallback_kind);
    }

    fn observe_desktop(
        &mut self,
        next: Option<SecureState>,
        desktop_default: bool,
        fallback_kind: &'static str,
    ) {
        // Neither a missing helper nor a delayed Default report can override
        // the host's observation that the input desktop is not Default.
        let next = if !desktop_default && !next.as_ref().is_some_and(|state| state.secure) {
            Some(SecureState {
                secure: true,
                kind: fallback_kind,
                caps: vec![],
                available: false,
                desktop_epoch: 0,
            })
        } else {
            next
        };
        let secure = next.as_ref().is_some_and(|state| state.secure);
        let was_secure = self.secure_state.as_ref().is_some_and(|state| state.secure);
        let epoch_changed = secure
            && self
                .secure_state
                .as_ref()
                .zip(next.as_ref())
                .is_some_and(|(previous, next)| previous.desktop_epoch != next.desktop_epoch);
        if secure && !was_secure {
            self.normal_recovery_pending = true;
            if let Some(capture) = self.capture.as_mut() {
                capture.suspend_for_secure_desktop();
            }
            self.secure_loss_pending = true;
        }
        if self.secure_streaming
            && (epoch_changed
                || !next
                    .as_ref()
                    .is_some_and(|state| state.secure && state.available))
        {
            self.secure_streaming = false;
            self.secure_upload = None;
            self.secure_loss_pending = true;
        }
        self.secure_state = next;
    }

    fn acquire(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<(capture::CapturedFrame<'_>, bool, bool)>> {
        if self.secure_state.as_ref().is_some_and(|state| state.secure) {
            let frame = self
                .secure_helper
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take_frame();
            let Some(frame) = frame else { return Ok(None) };
            if frame.width != self.width
                || frame.height != self.height
                || !self.secure_state.as_ref().is_some_and(|state| {
                    state.available && state.desktop_epoch == frame.desktop_epoch
                })
            {
                warn!("secure helper frame does not match selected monitor; preserving session without media");
                self.secure_streaming = false;
                self.secure_loss_pending = true;
                return Ok(None);
            }
            if self.secure_upload.is_none() {
                let device = self.capture.as_ref().and_then(Capture::device);
                match capture::SecureFrameUpload::new(device) {
                    Ok(upload) => self.secure_upload = Some(upload),
                    Err(error) => {
                        warn!(%error, "secure frame upload unavailable; preserving viewer session");
                        self.secure_streaming = false;
                        self.secure_loss_pending = true;
                        return Ok(None);
                    }
                }
            }
            let uploaded = self
                .secure_upload
                .as_mut()
                .expect("secure upload present")
                .upload(frame.width, frame.height, frame.stride, &frame.pixels);
            return match uploaded {
                Ok(frame) => {
                    let recovered = !self.secure_streaming;
                    self.secure_streaming = true;
                    Ok(Some((frame, true, recovered)))
                }
                Err(error) => {
                    warn!(%error, "secure frame upload failed; preserving viewer session");
                    self.secure_streaming = false;
                    self.secure_loss_pending = true;
                    Ok(None)
                }
            };
        }
        self.ensure_capture()?;
        let frame = self
            .capture
            .as_mut()
            .expect("capture initialized")
            .acquire(timeout)?;
        Ok(frame.map(|(frame, changed, recovered)| {
            let recovered = finish_normal_recovery(&mut self.normal_recovery_pending, recovered);
            self.join_recovery = JoinRecoveryWindow {
                deadline: None,
                exhausted: true,
            };
            if let Some(region) = frame.desktop_region() {
                self.region = region;
                self.width = region.width;
                self.height = region.height;
            }
            (frame, changed, recovered)
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DesktopState {
    pub(super) available: bool,
    pub(super) desktop: &'static str,
    pub(super) input: &'static str,
}

pub(super) fn input_ready_after_presentation(
    capture_available: bool,
    presented_generation: u32,
    current_generation: u32,
) -> bool {
    presented_generation == current_generation && capture_available
}

/// The display a session must capture: the runtime selection when one exists,
/// otherwise the launch selector. The host never substitutes another display
/// for a remembered selection.
pub(super) fn session_capture_selector<'a>(
    selected: Option<&'a str>,
    launch: Option<&'a str>,
) -> Option<&'a str> {
    selected.or(launch)
}

impl CaptureSource {
    /// `selector_override` is the runtime selection the host remembered across
    /// sessions. It wins over the launch selector; when it no longer resolves,
    /// opening fails instead of silently capturing a different display.
    pub(super) fn open(args: &Args, selector_override: Option<&str>) -> Result<Self> {
        Ok(Self::Local(LocalCaptureSource::open(
            session_capture_selector(selector_override, args.monitor.as_deref()),
        )?))
    }

    /// Canonical selector for the live local capture.
    pub(super) fn monitor_selector(&self) -> String {
        match self {
            Self::Local(source) => source.selector.clone(),
        }
    }

    pub(super) fn width(&self) -> u32 {
        match self {
            Self::Local(source) => source.width,
        }
    }

    pub(super) fn height(&self) -> u32 {
        match self {
            Self::Local(source) => source.height,
        }
    }

    pub(super) fn region(&self) -> DesktopRegion {
        match self {
            Self::Local(source) => source.region,
        }
    }

    pub(super) fn monitors(&mut self) -> Result<Vec<capture::MonitorInfo>> {
        match self {
            Self::Local(source) => source.monitors(),
        }
    }

    pub(super) fn switch_monitor(&mut self, selector: &str) -> Result<()> {
        if !self.desktop_state().available || self.desktop_state().desktop == "secure" {
            anyhow::bail!("capture unavailable; monitor selection is suspended until recovery");
        }
        match self {
            Self::Local(source) => source.switch_monitor(selector),
        }
    }

    pub(super) fn request_idle_snapshot(&mut self) {
        let Self::Local(source) = self;
        source.request_idle_snapshot();
    }

    /// Capture watchdog: re-create a local duplication that delivers nothing.
    /// Returns false when nothing was re-created (no open duplication, or a
    /// recovery already pending).
    pub(super) fn recreate_stalled_duplication(&mut self, now: Instant) -> bool {
        let Self::Local(source) = self;
        source
            .capture
            .as_mut()
            .is_some_and(|capture| capture.recreate_stalled_duplication(now))
    }

    pub(super) fn request_idle_followup_snapshot(&mut self, delay: Duration) {
        let Self::Local(source) = self;
        if let Some(capture) = source.capture.as_mut() {
            capture.request_idle_followup_snapshot(delay);
        }
    }

    pub(super) fn cancel_idle_followup_snapshot(&mut self) {
        let Self::Local(source) = self;
        if let Some(capture) = source.capture.as_mut() {
            capture.cancel_idle_followup_snapshot();
        }
    }

    pub(super) fn acquire(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<(capture::CapturedFrame<'_>, bool, bool)>> {
        match self {
            Self::Local(source) => source.acquire(timeout),
        }
    }

    pub(super) fn set_event_activity(&mut self, activity: bubbleplay_host::activity::Activity) {
        let Self::Local(source) = self;
        source.event_activity = Some(activity.clone());
        if let Some(capture) = source.capture.as_mut() {
            capture.enable_event_capture(activity);
        }
    }

    pub(super) fn set_capture_demand(&mut self, enabled: bool) -> Result<()> {
        let Self::Local(source) = self;
        source.capture_demand = enabled;
        if let Some(capture) = source.capture.as_mut() {
            capture.set_capture_demand(enabled);
        }
        Ok(())
    }

    pub(super) fn end_viewer_session(&mut self) {
        let Self::Local(source) = self;
        source.join_recovery = JoinRecoveryWindow::default();
    }

    pub(super) fn defer_recoverable_join_state(&mut self, now: Instant) -> bool {
        let state = self.desktop_state();
        let Self::Local(source) = self;
        let secure = source
            .secure_state
            .as_ref()
            .is_some_and(|state| state.secure);
        let recoverable = source.can_recover_for_join();
        source.join_recovery.begin(secure, recoverable, now);
        source
            .join_recovery
            .suppress(state.available, secure, recoverable, true, now)
    }

    pub(super) fn maintain_parked(&mut self, viewer_attached: bool, now: Instant) -> Result<()> {
        let Self::Local(source) = self;
        if !viewer_attached
            && !source
                .secure_state
                .as_ref()
                .is_some_and(|state| state.secure)
        {
            if let Some(capture) = source.capture.as_mut() {
                capture.maintain_parked(now)?;
                source.width = capture.width();
                source.height = capture.height();
                source.region = capture.region();
            }
        }
        Ok(())
    }

    pub(super) fn parked_maintenance_deadline(&self) -> Option<Instant> {
        match self {
            Self::Local(source)
                if !source
                    .secure_state
                    .as_ref()
                    .is_some_and(|state| state.secure) =>
            {
                source
                    .capture
                    .as_ref()
                    .and_then(Capture::parked_maintenance_deadline)
            }
            _ => None,
        }
    }

    pub(super) fn request_event_frame_at(&mut self, not_before: Instant) -> Result<()> {
        let Self::Local(source) = self;
        if source.capture_demand
            && !source
                .secure_state
                .as_ref()
                .is_some_and(|state| state.secure)
        {
            source.ensure_capture()?.request_frame_at(not_before)?;
        }
        Ok(())
    }

    pub(super) fn next_event_deadline(&mut self, snapshot_eligible: bool) -> Option<Instant> {
        let Self::Local(source) = self;
        if !source.capture_demand {
            return None;
        }
        let mut deadline = source.next_desktop_check;
        if let Some(join) = source.join_recovery.deadline {
            deadline = deadline.min(join);
        }
        if let Some(capture) = source.capture.as_ref() {
            // The session owns its recovery-probe deadline. Capture's cached
            // desktop timer can stay expired between those probes, so it is
            // never a separate reason to wake this producer.
            if snapshot_eligible && capture.can_recover_for_join() {
                if let Some(snapshot) = source
                    .snapshot_wake
                    .deadline(capture.idle_snapshot_deadline(), Instant::now())
                {
                    deadline = deadline.min(snapshot);
                }
            }
        }
        Some(deadline)
    }

    #[cfg(test)]
    pub(super) fn take_idle_snapshot_frame(&mut self) -> Option<capture::CapturedFrame<'_>> {
        self.take_idle_snapshot_frame_with_recovery()
            .map(|(frame, _)| frame)
    }

    pub(super) fn take_idle_snapshot_frame_with_recovery(
        &mut self,
    ) -> Option<(capture::CapturedFrame<'_>, bool)> {
        match self {
            Self::Local(source)
                if !source
                    .secure_state
                    .as_ref()
                    .is_some_and(|state| state.secure) =>
            {
                let (frame, recovered) = source
                    .capture
                    .as_mut()?
                    .take_idle_snapshot_frame_with_recovery()?;
                let recovered =
                    finish_normal_recovery(&mut source.normal_recovery_pending, recovered);
                // A healthy snapshot can beat the first waiter response. Keep
                // the original bound in case that response discovers a loss
                // which Windows did not announce while parked.
                if recovered {
                    source.join_recovery = JoinRecoveryWindow {
                        deadline: None,
                        exhausted: true,
                    };
                }
                Some((frame, recovered))
            }
            _ => None,
        }
    }

    pub(super) fn acknowledge_idle_snapshot_idr(&mut self) {
        let Self::Local(source) = self;
        if let Some(capture) = source.capture.as_mut() {
            capture.acknowledge_idle_snapshot_idr();
        }
    }

    pub(super) fn local_capture_stats(&self) -> Option<(u64, u64)> {
        match self {
            Self::Local(source) => source.capture.as_ref().map(Capture::stats),
        }
    }

    pub(super) fn tick_secure(&mut self, viewer: bool, now: Instant) {
        let Self::Local(source) = self;
        source.tick_secure(viewer, now);
    }

    pub(super) fn desktop_state(&self) -> DesktopState {
        match self {
            Self::Local(source) => {
                if let Some(state) = source.secure_state.as_ref().filter(|state| state.secure) {
                    return DesktopState {
                        available: source.secure_streaming && state.available,
                        desktop: "secure",
                        input: if source.secure_streaming
                            && state.available
                            && state.caps.contains(&"input")
                        {
                            "enabled"
                        } else if source.secure_streaming && state.available {
                            "read-only"
                        } else {
                            "suspended"
                        },
                    };
                }
                let available = !source.normal_recovery_pending
                    && source.capture.as_ref().is_none_or(|capture| {
                        capture.availability() == capture::CaptureAvailability::Active
                    });
                DesktopState {
                    available,
                    desktop: if available { "default" } else { "none" },
                    input: if available { "enabled" } else { "suspended" },
                }
            }
        }
    }

    pub(super) fn secure_helper(&self) -> Option<SharedSecureHelper> {
        match self {
            Self::Local(source) => Some(source.secure_helper.clone()),
        }
    }

    pub(super) fn secure_helper_ready(&self) -> bool {
        matches!(self, Self::Local(source) if source.secure_helper.lock()
            .unwrap_or_else(|error| error.into_inner()).state().is_some())
    }

    pub(super) fn secure_state(&self) -> Option<SecureState> {
        match self {
            Self::Local(source) => source.secure_state.clone().filter(|state| state.secure),
        }
    }

    pub(super) fn secure_input_allowed(&self) -> bool {
        self.secure_state()
            .is_none_or(|state| self.desktop_state().available && state.caps.contains(&"input"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_deadlines_wake_when_due_once_and_rearm_for_changed_work() {
        let now = Instant::now();
        let mut wake = SnapshotDeadlineWake::default();
        let due = now + Duration::from_millis(34);
        assert_eq!(wake.deadline(Some(due), now), Some(due));
        assert_eq!(
            wake.deadline(Some(due), now + Duration::from_millis(10)),
            Some(due)
        );
        assert_eq!(wake.deadline(Some(due), due), Some(due));
        // Admission blocked this attempt. The snapshot remains due, but the
        // unchanged expired timer cannot spin the producer.
        assert_eq!(wake.deadline(Some(due), due + Duration::from_secs(1)), None);
        let requested_again = due + Duration::from_secs(1);
        assert_eq!(
            wake.deadline(Some(requested_again), requested_again),
            Some(requested_again)
        );
        assert_eq!(wake.deadline(None, requested_again), None);
    }

    #[test]
    fn secure_exit_recovers_once_with_warm_or_cold_duplication() {
        for dda_recovered in [false, true] {
            let mut pending = true;
            assert!(finish_normal_recovery(&mut pending, dda_recovered));
            assert!(!finish_normal_recovery(&mut pending, false));
        }
    }

    #[test]
    fn session_replay_does_not_announce_unavailable_for_healthy_or_recoverable_capture() {
        let now = Instant::now();
        let mut healthy = JoinRecoveryWindow::default();
        healthy.begin(false, true, now);
        assert!(!healthy.suppress(true, false, true, true, now));
        assert!(!healthy.take_failed(true, false, true, now));
        let mut recovering = JoinRecoveryWindow::default();
        recovering.begin(false, true, now);
        assert!(recovering.suppress(false, false, true, true, now));
        // An offer replay and media start share one bound, even if repeated.
        recovering.begin(false, true, now + Duration::from_millis(900));
        assert!(recovering.take_failed(false, false, true, now + JOIN_RECOVERY_GRACE));
        recovering.begin(false, true, now + JOIN_RECOVERY_GRACE);
        assert!(!recovering.suppress(false, false, true, true, now + JOIN_RECOVERY_GRACE));
    }

    #[test]
    fn parked_join_recovers_silently_but_unavailable_is_announced_after_two_seconds() {
        let now = Instant::now();
        let mut normal = JoinRecoveryWindow::default();
        normal.begin(false, true, now);
        assert!(normal.suppress(false, false, true, true, now + Duration::from_millis(500)));
        assert!(!normal.take_failed(true, false, true, now + Duration::from_millis(500)));
        assert!(!normal.take_failed(true, false, true, now + JOIN_RECOVERY_GRACE));
        let mut down = JoinRecoveryWindow::default();
        down.begin(false, true, now);
        assert!(down.suppress(false, false, true, true, now + Duration::from_millis(1999)));
        assert!(down.take_failed(false, false, true, now + Duration::from_secs(2)));
        assert!(!down.suppress(false, false, true, true, now + Duration::from_secs(2)));
        assert!(!down.take_failed(false, false, true, now + Duration::from_secs(3)));
    }

    #[test]
    fn parked_recoverable_status_waits_for_pixels_but_real_failure_stays_visible() {
        let now = Instant::now();
        let parked = JoinRecoveryWindow::default();
        assert!(parked.suppress(false, false, true, false, now));
        assert!(!parked.suppress(false, false, false, false, now));
        assert!(!parked.suppress(false, true, true, false, now));
        assert!(!parked.suppress(false, false, true, true, now));
    }

    #[test]
    fn real_failure_and_secure_desktop_are_never_hidden_by_join_recovery() {
        let now = Instant::now();
        let mut failed = JoinRecoveryWindow::default();
        failed.begin(false, false, now);
        assert!(!failed.suppress(false, false, false, true, now));
        let mut recovering = JoinRecoveryWindow::default();
        recovering.begin(false, true, now);
        assert!(!recovering.suppress(false, false, false, true, now));
        assert!(recovering.take_failed(false, false, false, now));
        let mut secure = JoinRecoveryWindow::default();
        secure.begin(true, true, now);
        assert!(!secure.suppress(false, true, true, true, now));
        assert!(!secure.take_failed(false, true, true, now));
    }

    #[test]
    fn secure_source_pins_monitor_and_requires_frame_before_input() {
        let mut source = LocalCaptureSource {
            capture: None,
            selector: "DISPLAY2".into(),
            width: 1920,
            height: 1080,
            region: DesktopRegion::new(-1920, 0, 1920, 1080),
            idle_snapshot_requested: false,
            secure_helper: Arc::new(Mutex::new(SecureHelperLink::new())),
            secure_state: Some(SecureState {
                secure: true,
                kind: "uac",
                caps: vec!["view", "deny", "input"],
                available: true,
                desktop_epoch: 1,
            }),
            secure_streaming: false,
            secure_upload: None,
            normal_recovery_pending: true,
            secure_loss_pending: false,
            next_secure_kind_probe: Instant::now(),
            next_desktop_check: Instant::now(),
            desktop_default: false,
            helper_secure: false,
            event_activity: None,
            capture_demand: false,
            snapshot_wake: SnapshotDeadlineWake::default(),
            join_recovery: JoinRecoveryWindow::default(),
        };
        assert!(source.switch_monitor("DISPLAY1").is_err());
        assert_eq!(source.selector, "DISPLAY2");
        assert_eq!(source.region.x, -1920);
        let mut capture = CaptureSource::Local(source);
        assert!(!capture.desktop_state().available);
        assert!(!capture.secure_input_allowed());
        {
            let CaptureSource::Local(source) = &mut capture;
            source.secure_streaming = true;
        }
        assert!(capture.desktop_state().available);
        assert!(capture.secure_input_allowed());
        {
            let CaptureSource::Local(source) = &mut capture;
            source.secure_state.as_mut().unwrap().caps = vec!["view", "deny"];
        }
        assert!(!capture.secure_input_allowed());
        assert!(capture.switch_monitor("DISPLAY1").is_err());
        {
            let CaptureSource::Local(source) = &mut capture;
            source.secure_state = None;
        }
        assert!(!capture.desktop_state().available);
        assert!(capture.take_idle_snapshot_frame().is_none());
        // A warm source with no viewer never lazily opens capture or arms a
        // worker, even if a scheduling call is accidentally made.
        capture.set_capture_demand(false).unwrap();
        capture.request_event_frame_at(Instant::now()).unwrap();
        assert!(capture.next_event_deadline(true).is_none());
        {
            let CaptureSource::Local(source) = &capture;
            assert!(source.capture.is_none());
        }
    }

    #[test]
    fn secure_input_desktop_overrides_missing_or_default_helper_report() {
        for helper in [
            None,
            Some(SecureState {
                secure: false,
                kind: "other",
                caps: vec![],
                available: false,
                desktop_epoch: 1,
            }),
        ] {
            let mut source = LocalCaptureSource {
                capture: None,
                selector: "DISPLAY2".into(),
                width: 1920,
                height: 1080,
                region: DesktopRegion::new(-1920, 0, 1920, 1080),
                idle_snapshot_requested: false,
                secure_helper: Arc::new(Mutex::new(SecureHelperLink::new())),
                secure_state: None,
                secure_streaming: false,
                secure_upload: None,
                normal_recovery_pending: false,
                secure_loss_pending: false,
                next_secure_kind_probe: Instant::now(),
                next_desktop_check: Instant::now(),
                desktop_default: false,
                helper_secure: false,
                event_activity: None,
                capture_demand: false,
                snapshot_wake: SnapshotDeadlineWake::default(),
                join_recovery: JoinRecoveryWindow::default(),
            };
            source.observe_desktop(helper, false, "uac");
            assert_eq!(
                source.take_availability_transition(),
                Some(capture::CaptureAvailability::Unavailable)
            );
            assert!(source.acquire(Duration::ZERO).unwrap().is_none());
            assert!(
                source.capture.is_none(),
                "must not initialize DDA on the secure input desktop"
            );
            assert!(source.normal_recovery_pending);
            let mut capture = CaptureSource::Local(source);
            assert_eq!(capture.desktop_state().desktop, "secure");
            assert!(!capture.desktop_state().available);
            assert!(!capture.secure_input_allowed());
            assert_eq!(capture.secure_state().unwrap().caps, Vec::<&str>::new());
            {
                let CaptureSource::Local(source) = &mut capture;
                source.observe_desktop(None, true, "other");
            }
            assert!(
                !capture.desktop_state().available,
                "Default alone is not a recovered DDA frame"
            );
        }
    }
}
