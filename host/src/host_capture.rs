//! Binary-owned capture boundary; preserves existing streaming policy.
use super::{
    capture, info, input, pick_monitor, privileged_ipc, warn, AgentClient, AgentMessage, Args,
    Capture, Context, DesktopRegion, Duration, Instant, Result, RetryPolicy,
};

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
        })
    }

    pub(super) fn ensure_capture(&mut self) -> Result<&mut Capture> {
        if self.capture.is_none() {
            let mut capture = Capture::new(Some(&self.selector)).context("capture init")?;
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
        if let Some(capture) = self.capture.as_mut() {
            capture.request_idle_snapshot();
            self.idle_snapshot_requested = false;
        }
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
}

pub(super) const AGENT_RECONNECT_LOG_INTERVAL: Duration = Duration::from_secs(30);

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
    !privileged
        || (capture_available
            && state.available
            && state.input == "enabled"
            && presented_generation == current_generation)
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
    InputStatus(privileged_ipc::InputStatistics),
    Monitors(Vec<capture::MonitorInfo>),
    MonitorSwitch {
        accepted: bool,
        monitor: String,
        error_code: i32,
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
            Self::Local(source) => source.ensure_capture()?.acquire(timeout),
            Self::Privileged(_) => Ok(None),
        }
    }

    pub(super) fn take_idle_snapshot_frame(&mut self) -> Option<capture::CapturedFrame<'_>> {
        match self {
            Self::Local(source) => source
                .capture
                .as_mut()
                .and_then(Capture::take_idle_snapshot_frame),
            Self::Privileged(_) => None,
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
            Self::Local(_) => PrivilegedDesktopState {
                available: true,
                desktop: "default",
                input: "enabled",
                agent_generation: 0,
            },
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
