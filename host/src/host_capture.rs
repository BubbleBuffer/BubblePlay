//! Binary-owned capture boundary; preserves existing streaming policy.
use super::{
    capture, info, input, pick_monitor, privileged_ipc, warn, AgentClient, AgentMessage, Args,
    Capture, Context, DesktopRegion, Duration, Instant, Result, RetryPolicy,
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

pub(super) enum CaptureSource {
    Local(LocalCaptureSource),
    Privileged(PrivilegedSource),
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
}

impl LocalCaptureSource {
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
            if let Some(capture) = self.capture.as_mut() {
                let _ = capture.take_availability_transition();
            }
            return Some(capture::CaptureAvailability::Unavailable);
        }
        self.capture
            .as_mut()
            .and_then(Capture::take_availability_transition)
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
            if let Some(region) = frame.desktop_region() {
                self.region = region;
                self.width = region.width;
                self.height = region.height;
            }
            (frame, changed, recovered)
        }))
    }
}

pub(super) struct PrivilegedSource {
    pub(super) client: Option<AgentClient>,
    pub(super) next_agent_connect: Instant,
    pub(super) agent_reconnect_step: usize,
    pub(super) disconnect_event_pending: bool,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) monitors: Vec<capture::MonitorInfo>,
    pub(super) selected_monitor: Option<u32>,
    pub(super) desktop_available: bool,
    pub(super) desktop: &'static str,
    pub(super) input_state: &'static str,
    pub(super) generation: u32,
    pub(super) reconnect_log: ReconnectLogGate,
    event_activity: Option<bubbleplay_host::activity::Activity>,
}

pub(super) const AGENT_RECONNECT_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Floor for the producer's retry after a failed privileged permit. The
/// reconnect backoff supplies the real cadence; this only guarantees that a
/// failure can never be retried in the same instant it was observed.
pub(super) const MIN_AGENT_ENCODE_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Default)]
pub(super) struct ReconnectLogGate {
    pub(super) last_emitted: Option<Instant>,
    pub(super) suppressed: u64,
}

impl ReconnectLogGate {
    pub(super) fn admit(&mut self, now: Instant) -> Option<u64> {
        if self
            .last_emitted
            .is_some_and(|last| now.saturating_duration_since(last) < AGENT_RECONNECT_LOG_INTERVAL)
        {
            self.suppressed = self.suppressed.saturating_add(1);
            return None;
        }
        self.last_emitted = Some(now);
        Some(std::mem::take(&mut self.suppressed))
    }
}

pub(super) fn agent_reconnect_delay(step: usize) -> Duration {
    pub(super) const DELAYS: [Duration; 4] = [
        Duration::from_millis(250),
        Duration::from_millis(500),
        Duration::from_secs(1),
        Duration::from_secs(2),
    ];
    DELAYS[step.min(DELAYS.len() - 1)]
}

impl PrivilegedSource {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            client: None,
            next_agent_connect: now,
            agent_reconnect_step: 0,
            disconnect_event_pending: false,
            width: 1920,
            height: 1080,
            monitors: Vec::new(),
            selected_monitor: None,
            desktop_available: false,
            desktop: "none",
            input_state: "suspended",
            generation: 0,
            reconnect_log: ReconnectLogGate::default(),
            event_activity: None,
        }
    }

    pub(super) fn connect_startup(now: Instant) -> Result<Self> {
        let mut client = AgentClient::connect().context("connect initial privileged Agent")?;
        client
            .send(&AgentMessage::ListMonitors)
            .context("request initial privileged monitor topology")?;
        let mut source = Self::new(now);
        source.client = Some(client);
        info!("initial privileged Agent connection established");
        Ok(source)
    }

    pub(super) fn mark_agent_unavailable(&mut self, now: Instant, error: &anyhow::Error) {
        let was_connected = self.client.take().is_some();
        self.desktop_available = false;
        self.input_state = "suspended";
        if was_connected {
            self.disconnect_event_pending = true;
        }
        let delay = agent_reconnect_delay(self.agent_reconnect_step);
        self.agent_reconnect_step = self.agent_reconnect_step.saturating_add(1);
        self.next_agent_connect = now + delay;
        if let Some(suppressed) = self.reconnect_log.admit(now) {
            warn!(
                ?error,
                retry_ms = delay.as_millis(),
                suppressed,
                "privileged Agent unavailable; preserving network session"
            );
        }
    }

    pub(super) fn connect_agent_if_due(&mut self, now: Instant) {
        if self.client.is_some() || now < self.next_agent_connect {
            return;
        }
        let policy = RetryPolicy::new(Duration::ZERO, Duration::from_millis(1))
            .expect("valid one-shot Agent retry policy");
        match AgentClient::connect_with_retry(policy) {
            Ok(mut client) => {
                if let Some(activity) = self.event_activity.as_ref() {
                    client.set_activity(activity.clone());
                }
                // Refresh topology after every process-boundary recovery. The
                // browser may have issued its request while the Agent was down.
                if let Err(error) = client.send(&AgentMessage::ListMonitors) {
                    self.client = Some(client);
                    self.mark_agent_unavailable(now, &error);
                    return;
                }
                self.client = Some(client);
                self.agent_reconnect_step = 0;
                self.next_agent_connect = now;
                info!("privileged Agent connected");
            }
            Err(error) => self.mark_agent_unavailable(now, &error),
        }
    }

    pub(super) fn tick_agent(&mut self, now: Instant) {
        self.connect_agent_if_due(now);
        let result = self.client.as_mut().map(|client| client.tick(now));
        if let Some(Err(error)) = result {
            self.mark_agent_unavailable(now, &error);
        }
    }

    pub(super) fn try_agent_message(&mut self, now: Instant) -> Option<AgentMessage> {
        let result = self.client.as_ref()?.try_event();
        match result {
            Ok(message) => message,
            Err(error) => {
                self.mark_agent_unavailable(now, &error);
                None
            }
        }
    }

    pub(super) fn send_agent(&mut self, message: &AgentMessage) -> Result<()> {
        let result = self
            .client
            .as_mut()
            .context("privileged Agent is reconnecting")?
            .send(message);
        if let Err(error) = &result {
            self.mark_agent_unavailable(Instant::now(), error);
        }
        result
    }

    pub(super) fn encode_agent(&mut self, permit: AgentMessage) -> Result<AgentMessage> {
        let result = self
            .client
            .as_mut()
            .context("privileged Agent is reconnecting")?
            .encode(permit);
        if let Err(error) = &result {
            self.mark_agent_unavailable(Instant::now(), error);
        }
        result
    }

    /// Bounded wait before the next privileged encode attempt. The reconnect
    /// backoff already owns this cadence; the producer must respect it instead
    /// of re-issuing a permit against a client that is being reconnected.
    pub(super) fn agent_retry_delay(&self, now: Instant) -> Duration {
        self.next_agent_connect
            .saturating_duration_since(now)
            .max(MIN_AGENT_ENCODE_RETRY_DELAY)
    }

    /// Producer-side companion to `mark_agent_unavailable`: returns the bounded
    /// retry delay and reports the failure at most once per reconnect window.
    /// The failure is normally already reported by `mark_agent_unavailable`;
    /// this gate exists so the producer's retry loop cannot emit an unbounded
    /// warning per iteration (that flood trips the service log-budget guard).
    pub(super) fn note_encode_failure(&mut self, now: Instant, error: &anyhow::Error) -> Duration {
        let delay = self.agent_retry_delay(now);
        if let Some(suppressed) = self.reconnect_log.admit(now) {
            warn!(
                ?error,
                retry_ms = delay.as_millis(),
                suppressed,
                "privileged Agent encode failed; preserving network session"
            );
        }
        delay
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PrivilegedDesktopState {
    pub(super) available: bool,
    pub(super) desktop: &'static str,
    pub(super) input: &'static str,
    pub(super) agent_generation: u32,
}

pub(super) fn record_privileged_desktop_state(
    last: &mut Option<PrivilegedDesktopState>,
    next: PrivilegedDesktopState,
) -> bool {
    if *last == Some(next) {
        return false;
    }
    *last = Some(next);
    true
}

pub(super) fn privileged_input_ready_after_presentation(
    privileged: bool,
    capture_available: bool,
    state: PrivilegedDesktopState,
    presented_generation: u32,
    current_generation: u32,
) -> bool {
    presented_generation == current_generation
        && capture_available
        && (!privileged || (state.available && state.input == "enabled"))
}

pub(super) enum PrivilegedEvent {
    Desktop {
        available: bool,
        desktop: &'static str,
        input: &'static str,
        generation: u32,
        reason: &'static str,
    },
    Cursor([u8; 8]),
    CursorKind([u8; 2]),
    InputStatus(privileged_ipc::InputStatistics),
    Monitors(Vec<capture::MonitorInfo>),
    MonitorSwitch {
        accepted: bool,
        monitor: String,
        error_code: i32,
    },
    Clipboard {
        id: u32,
        index: u16,
        count: u16,
        text: String,
        png: bool,
    },
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
        if args.privileged_agent {
            // Cold service startup must prove the privileged boundary before
            // the host advertises signaling readiness. Runtime disconnects
            // after this point still use the bounded in-session reconnect path.
            // The privileged Agent owns its monitor selection; the host keeps
            // no local override for it.
            Ok(Self::Privileged(PrivilegedSource::connect_startup(
                Instant::now(),
            )?))
        } else {
            Ok(Self::Local(LocalCaptureSource::open(
                session_capture_selector(selector_override, args.monitor.as_deref()),
            )?))
        }
    }

    /// Canonical selector for the live local capture. `None` for the
    /// privileged source: its selection lives in the Agent and the host must
    /// not substitute a local display choice for it.
    pub(super) fn monitor_selector(&self) -> Option<String> {
        match self {
            Self::Local(source) => Some(source.selector.clone()),
            Self::Privileged(_) => None,
        }
    }

    pub(super) fn width(&self) -> u32 {
        match self {
            Self::Local(source) => source.width,
            Self::Privileged(source) => source.width,
        }
    }

    pub(super) fn height(&self) -> u32 {
        match self {
            Self::Local(source) => source.height,
            Self::Privileged(source) => source.height,
        }
    }

    pub(super) fn region(&self) -> DesktopRegion {
        match self {
            Self::Local(source) => source.region,
            Self::Privileged(source) => DesktopRegion::new(0, 0, source.width, source.height),
        }
    }

    pub(super) fn monitors(&mut self) -> Result<Vec<capture::MonitorInfo>> {
        match self {
            Self::Local(source) => source.monitors(),
            Self::Privileged(source) => {
                source.send_agent(&AgentMessage::ListMonitors)?;
                Ok(source.monitors.clone())
            }
        }
    }

    pub(super) fn switch_monitor(&mut self, selector: &str) -> Result<bool> {
        if !self.desktop_state().available || self.desktop_state().desktop == "secure" {
            anyhow::bail!("capture unavailable; monitor selection is suspended until recovery");
        }
        match self {
            Self::Local(source) => {
                source.switch_monitor(selector)?;
                Ok(true)
            }
            Self::Privileged(source) => {
                let monitor_id = selector
                    .strip_prefix("agent:")
                    .unwrap_or(selector)
                    .parse::<u32>()
                    .context("privileged monitor selector must be agent:<id>")?;
                source.send_agent(&AgentMessage::SelectMonitor { monitor_id })?;
                Ok(false)
            }
        }
    }

    pub(super) fn request_idle_snapshot(&mut self) {
        if let Self::Local(source) = self {
            source.request_idle_snapshot();
        }
        // Privileged permits carry allow_idle_snapshot and force_idr, so the
        // next one is already a complete static-desktop wake request.
    }

    pub(super) fn request_idle_followup_snapshot(&mut self, delay: Duration) {
        if let Self::Local(source) = self {
            if let Some(capture) = source.capture.as_mut() {
                capture.request_idle_followup_snapshot(delay);
            }
        }
    }

    pub(super) fn cancel_idle_followup_snapshot(&mut self) {
        if let Self::Local(source) = self {
            if let Some(capture) = source.capture.as_mut() {
                capture.cancel_idle_followup_snapshot();
            }
        }
    }

    pub(super) fn acquire(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<(capture::CapturedFrame<'_>, bool, bool)>> {
        match self {
            Self::Local(source) => source.acquire(timeout),
            Self::Privileged(_) => Ok(None),
        }
    }

    pub(super) fn set_event_activity(&mut self, activity: bubbleplay_host::activity::Activity) {
        match self {
            Self::Local(source) => {
                source.event_activity = Some(activity.clone());
                if let Some(capture) = source.capture.as_mut() {
                    capture.enable_event_capture(activity);
                }
            }
            Self::Privileged(source) => {
                source.event_activity = Some(activity.clone());
                if let Some(client) = source.client.as_ref() {
                    client.set_activity(activity);
                }
            }
        }
    }

    pub(super) fn set_capture_demand(&mut self, enabled: bool) -> Result<()> {
        if let Self::Local(source) = self {
            source.capture_demand = enabled;
            if let Some(capture) = source.capture.as_mut() {
                capture.set_capture_demand(enabled);
            }
        }
        Ok(())
    }

    pub(super) fn request_event_frame_at(&mut self, not_before: Instant) -> Result<()> {
        if let Self::Local(source) = self {
            if source.capture_demand
                && !source
                    .secure_state
                    .as_ref()
                    .is_some_and(|state| state.secure)
            {
                source.ensure_capture()?.request_frame_at(not_before)?;
            }
        }
        Ok(())
    }

    pub(super) fn next_event_deadline(&mut self, snapshot_eligible: bool) -> Option<Instant> {
        let Self::Local(source) = self else {
            return None;
        };
        if !source.capture_demand {
            return None;
        }
        let mut deadline = source.next_desktop_check;
        if let Some(capture) = source.capture.as_ref() {
            // The session owns its recovery-probe deadline. Capture's cached
            // desktop timer can stay expired between those probes, so it is
            // never a separate reason to wake this producer.
            if snapshot_eligible && capture.availability() == capture::CaptureAvailability::Active {
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

    pub(super) fn take_idle_snapshot_frame(&mut self) -> Option<capture::CapturedFrame<'_>> {
        match self {
            Self::Local(source)
                if !source.normal_recovery_pending
                    && !source
                        .secure_state
                        .as_ref()
                        .is_some_and(|state| state.secure) =>
            {
                source
                    .capture
                    .as_mut()
                    .and_then(Capture::take_idle_snapshot_frame)
            }
            _ => None,
        }
    }

    pub(super) fn acknowledge_idle_snapshot_idr(&mut self) {
        if let Self::Local(source) = self {
            if let Some(capture) = source.capture.as_mut() {
                capture.acknowledge_idle_snapshot_idr();
            }
        }
    }

    pub(super) fn local_capture_stats(&self) -> Option<(u64, u64)> {
        match self {
            Self::Local(source) => source.capture.as_ref().map(Capture::stats),
            Self::Privileged(_) => None,
        }
    }

    pub(super) fn tick_secure(&mut self, viewer: bool, now: Instant) {
        if let Self::Local(source) = self {
            source.tick_secure(viewer, now);
        }
    }

    pub(super) fn tick(&mut self, now: Instant) -> Result<()> {
        if let Self::Privileged(source) = self {
            source.tick_agent(now);
        }
        Ok(())
    }

    pub(super) fn try_privileged_event(&mut self) -> Result<Option<PrivilegedEvent>> {
        let Self::Privileged(source) = self else {
            return Ok(None);
        };
        if source.disconnect_event_pending {
            source.disconnect_event_pending = false;
            return Ok(Some(PrivilegedEvent::Desktop {
                available: false,
                desktop: source.desktop,
                input: "suspended",
                generation: source.generation,
                reason: "agent-disconnected",
            }));
        }
        let Some(message) = source.try_agent_message(Instant::now()) else {
            return Ok(None);
        };
        Ok(match message {
            AgentMessage::DesktopTransition {
                desktop,
                generation,
                input_enabled,
                reason,
            } => {
                let desktop = match desktop {
                    privileged_ipc::Desktop::Default => "default",
                    privileged_ipc::Desktop::Winlogon => "secure",
                    privileged_ipc::Desktop::Unknown => "none",
                };
                let input = if input_enabled {
                    "enabled"
                } else {
                    "suspended"
                };
                if source.generation != generation || desktop == "none" {
                    // A freshly-bound desktop is not safe to expose until the
                    // Agent proves capture with a frame from this generation.
                    source.desktop_available = false;
                }
                source.desktop = desktop;
                source.input_state = input;
                source.generation = generation;
                let available = source.desktop_available && desktop != "none";
                let reason = match reason {
                    privileged_ipc::DesktopTransitionReason::Startup => "agent-startup",
                    privileged_ipc::DesktopTransitionReason::DesktopSwitched => "desktop-switched",
                    privileged_ipc::DesktopTransitionReason::SessionChanged => "session-changed",
                    privileged_ipc::DesktopTransitionReason::AgentRestarted => "agent-restarted",
                };
                Some(PrivilegedEvent::Desktop {
                    available,
                    desktop,
                    input: if available { input } else { "suspended" },
                    generation,
                    reason,
                })
            }
            AgentMessage::CaptureHealth { state, .. } => {
                let available = state == privileged_ipc::CaptureState::Active;
                source.desktop_available = available;
                Some(PrivilegedEvent::Desktop {
                    available,
                    // Preserve which desktop is being recovered. In
                    // particular, reconnecting during Winlogon must retain the
                    // secure-desktop policy even while no new frame exists.
                    desktop: source.desktop,
                    input: if available {
                        source.input_state
                    } else {
                        "suspended"
                    },
                    generation: source.generation,
                    reason: if available {
                        "capture-recovered"
                    } else {
                        "capture-unavailable"
                    },
                })
            }
            AgentMessage::CursorKind(kind) => Some(PrivilegedEvent::CursorKind(kind.encode())),
            AgentMessage::Cursor(cursor) => Some(PrivilegedEvent::Cursor(
                input::CursorState {
                    acknowledged_sequence: cursor.acknowledged_sequence,
                    x: cursor.x,
                    y: cursor.y,
                    in_bounds: cursor.in_bounds,
                    visible: cursor.visible,
                }
                .encode(),
            )),
            AgentMessage::MonitorList(monitors) => {
                source.monitors = monitors
                    .into_iter()
                    .map(|monitor| capture::MonitorInfo {
                        selector: format!("agent:{}", monitor.id),
                        name: monitor.name,
                        width: monitor.width as u32,
                        height: monitor.height as u32,
                        origin_x: 0,
                        origin_y: 0,
                        selected: source.selected_monitor == Some(monitor.id)
                            || monitor.primary && source.selected_monitor.is_none(),
                    })
                    .collect();
                Some(PrivilegedEvent::Monitors(source.monitors.clone()))
            }
            AgentMessage::MonitorSwitchResult(result) => {
                if result.accepted {
                    source.selected_monitor = Some(result.monitor_id);
                    source.width = result.width as u32;
                    source.height = result.height as u32;
                    for monitor in &mut source.monitors {
                        monitor.selected =
                            monitor.selector == format!("agent:{}", result.monitor_id);
                    }
                }
                Some(PrivilegedEvent::MonitorSwitch {
                    accepted: result.accepted,
                    monitor: format!("agent:{}", result.monitor_id),
                    error_code: result.error_code,
                })
            }
            AgentMessage::InputStats(stats) => Some(PrivilegedEvent::InputStatus(stats)),
            AgentMessage::ClipboardText {
                id,
                index,
                count,
                text,
            } => Some(PrivilegedEvent::Clipboard {
                id,
                index,
                count,
                text,
                png: false,
            }),
            AgentMessage::ClipboardPng {
                id,
                index,
                count,
                text,
            } => Some(PrivilegedEvent::Clipboard {
                id,
                index,
                count,
                text,
                png: true,
            }),
            AgentMessage::Capabilities(_) | AgentMessage::ConfigAck { .. } => None,
            AgentMessage::RecoverableError(error) => {
                warn!(
                    operation = ?error.operation,
                    error_code = error.error_code,
                    session_generation = error.session_generation,
                    config_revision = error.config_revision,
                    permit_id = ?error.permit_id,
                    "privileged Agent reported a recoverable error"
                );
                None
            }
            AgentMessage::FatalError(error) => {
                warn!(
                    operation = ?error.operation,
                    error_code = error.error_code,
                    session_generation = error.session_generation,
                    config_revision = error.config_revision,
                    permit_id = ?error.permit_id,
                    "privileged Agent reported a fatal error"
                );
                source.desktop_available = false;
                source.input_state = "suspended";
                Some(PrivilegedEvent::Desktop {
                    available: false,
                    desktop: source.desktop,
                    input: "suspended",
                    generation: source.generation,
                    reason: "agent-fatal-error",
                })
            }
            _ => None,
        })
    }

    pub(super) fn desktop_state(&self) -> PrivilegedDesktopState {
        match self {
            Self::Local(source) => {
                if let Some(state) = source.secure_state.as_ref().filter(|state| state.secure) {
                    return PrivilegedDesktopState {
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
                        agent_generation: 0,
                    };
                }
                let available = !source.normal_recovery_pending
                    && source.capture.as_ref().is_none_or(|capture| {
                        capture.availability() == capture::CaptureAvailability::Active
                    });
                PrivilegedDesktopState {
                    available,
                    desktop: if available { "default" } else { "none" },
                    input: if available { "enabled" } else { "suspended" },
                    agent_generation: 0,
                }
            }
            Self::Privileged(source) => PrivilegedDesktopState {
                available: source.desktop_available,
                desktop: source.desktop,
                input: if source.desktop_available {
                    source.input_state
                } else {
                    "suspended"
                },
                agent_generation: source.generation,
            },
        }
    }

    pub(super) fn secure_helper(&self) -> Option<SharedSecureHelper> {
        match self {
            Self::Local(source) => Some(source.secure_helper.clone()),
            _ => None,
        }
    }

    pub(super) fn secure_helper_ready(&self) -> bool {
        matches!(self, Self::Local(source) if source.secure_helper.lock()
            .unwrap_or_else(|error| error.into_inner()).state().is_some())
    }

    pub(super) fn secure_state(&self) -> Option<SecureState> {
        match self {
            Self::Local(source) => source.secure_state.clone().filter(|state| state.secure),
            _ => None,
        }
    }

    pub(super) fn secure_input_allowed(&self) -> bool {
        self.secure_state()
            .is_none_or(|state| self.desktop_state().available && state.caps.contains(&"input"))
    }

    pub(super) fn send_input(&mut self, data: &[u8]) -> Result<bool> {
        match self {
            Self::Local(_) => Ok(false),
            Self::Privileged(source)
                if source.desktop_available && source.input_state == "enabled" =>
            {
                // An Agent failure must suspend input, not tear down the
                // authenticated browser/signaling session.
                let _ = source.send_agent(&AgentMessage::Input(data.to_vec()));
                Ok(true)
            }
            Self::Privileged(_) => Ok(true),
        }
    }

    pub(super) fn is_privileged(&self) -> bool {
        matches!(self, Self::Privileged(_))
    }

    pub(super) fn release_input(&mut self) {
        if let Self::Privileged(source) = self {
            let _ = source.send_agent(&AgentMessage::ReleaseAll);
        }
    }
}

#[cfg(test)]
mod agent_encode_recovery_tests {
    use super::*;

    #[test]
    fn privileged_capture_keeps_activity_before_a_client_reconnects() {
        let mut capture = CaptureSource::Privileged(PrivilegedSource::new(Instant::now()));
        capture.set_event_activity(bubbleplay_host::activity::Activity::new().unwrap());
        let CaptureSource::Privileged(source) = capture else {
            panic!("privileged source changed")
        };
        assert!(source.client.is_none());
        assert!(
            source.event_activity.is_some(),
            "reconnected clients need the same producer notifier"
        );
    }

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
        };
        assert!(source.switch_monitor("DISPLAY1").is_err());
        assert_eq!(source.selector, "DISPLAY2");
        assert_eq!(source.region.x, -1920);
        let mut capture = CaptureSource::Local(source);
        assert!(!capture.desktop_state().available);
        assert!(!capture.secure_input_allowed());
        if let CaptureSource::Local(source) = &mut capture {
            source.secure_streaming = true;
        }
        assert!(capture.desktop_state().available);
        assert!(capture.secure_input_allowed());
        if let CaptureSource::Local(source) = &mut capture {
            source.secure_state.as_mut().unwrap().caps = vec!["view", "deny"];
        }
        assert!(!capture.secure_input_allowed());
        assert!(capture.switch_monitor("DISPLAY1").is_err());
        if let CaptureSource::Local(source) = &mut capture {
            source.secure_state = None;
        }
        assert!(!capture.desktop_state().available);
        assert!(capture.take_idle_snapshot_frame().is_none());
        // A warm source with no viewer never lazily opens capture or arms a
        // worker, even if a scheduling call is accidentally made.
        capture.set_capture_demand(false).unwrap();
        capture.request_event_frame_at(Instant::now()).unwrap();
        assert!(capture.next_event_deadline(true).is_none());
        if let CaptureSource::Local(source) = &capture {
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
            if let CaptureSource::Local(source) = &mut capture {
                source.observe_desktop(None, true, "other");
            }
            assert!(
                !capture.desktop_state().available,
                "Default alone is not a recovered DDA frame"
            );
        }
    }

    fn timed_out() -> anyhow::Error {
        anyhow::anyhow!("Agent encode permit timed out")
    }

    #[test]
    fn encode_failure_retry_respects_the_reconnect_backoff() {
        let now = Instant::now();
        let mut source = PrivilegedSource::new(now);
        // Even without a recorded failure the floor forbids a same-instant
        // retry, so the producer loop can never spin at full speed.
        assert_eq!(source.agent_retry_delay(now), MIN_AGENT_ENCODE_RETRY_DELAY);
        source.mark_agent_unavailable(now, &timed_out());
        let delay = source.agent_retry_delay(now);
        assert_eq!(delay, agent_reconnect_delay(0));
        assert!(delay >= MIN_AGENT_ENCODE_RETRY_DELAY);
        // The deadline is absolute: once it passes, one retry is allowed.
        assert_eq!(
            source.agent_retry_delay(now + delay),
            MIN_AGENT_ENCODE_RETRY_DELAY
        );
    }

    #[test]
    fn repeated_encode_failures_do_not_report_per_iteration() {
        let now = Instant::now();
        let mut source = PrivilegedSource::new(now);
        let error = timed_out();
        // The first failure of a window is reported by mark_agent_unavailable.
        source.mark_agent_unavailable(now, &error);
        assert!(source.reconnect_log.last_emitted.is_some());
        let suppressed_before = source.reconnect_log.suppressed;
        // A producer that retried as fast as it could (the production flood)
        // must accumulate suppressions instead of stderr lines.
        for step in 0..5_000u64 {
            let at = now + Duration::from_micros(step);
            let delay = source.note_encode_failure(at, &error);
            assert!(delay >= MIN_AGENT_ENCODE_RETRY_DELAY);
        }
        assert!(source.reconnect_log.suppressed > suppressed_before);
        // Still inside the same window: nothing further is emitted.
        assert_eq!(
            source
                .reconnect_log
                .admit(now + AGENT_RECONNECT_LOG_INTERVAL - Duration::from_millis(1)),
            None
        );
        // A later window reports again and carries the suppressed count.
        assert!(source
            .reconnect_log
            .admit(now + AGENT_RECONNECT_LOG_INTERVAL)
            .is_some());
    }
}
