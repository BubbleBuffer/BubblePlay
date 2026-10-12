//! Privileged per-session agent runtime.
//!
//! This module owns only the control-plane invariants around a future capture,
//! encoder, and input backend.  The concrete DDA/NVENC/InputSink objects stay
//! in the existing host until the process split is integrated deliberately.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::desktop_session::{DesktopKind, DesktopTracker, InputDesktop};
use crate::privileged_ipc::{
    AgentError, AgentOperation, CaptureState, Desktop, DesktopTransitionReason, MediaConfig,
    Message, NoFrameReason, PermitIdentity,
};

pub const LEASE_TTL: Duration = Duration::from_secs(2);
pub const CONTROL_CHANNEL_CAPACITY: usize = 64;
pub const MEDIA_CHANNEL_CAPACITY: usize = 2;
const LOOP_TICK: Duration = Duration::from_millis(20);
const DESKTOP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopBinding {
    /// The freshly-bound worker can service capture/encode permits.
    pub work_ready: bool,
    /// The freshly-bound worker can inject input on this desktop.
    pub input_ready: bool,
}

/// Narrow seam for the capture/encode/input implementation that will move
/// behind this process boundary in a later stage.
///
/// `transition_desktop` is a strict lifecycle boundary: it must stop and join
/// the old worker, create a fresh OS thread, call
/// `InputDesktop::open()?.bind_thread()` on that thread before constructing any
/// USER/D3D resources, and only then return readiness. The runtime releases all
/// input and calls `suspend` before entering it. A backend that has not done
/// this must return fail-closed readiness flags.
pub trait AgentBackend: Send + 'static {
    fn configure(&mut self, config: &MediaConfig) -> Result<()>;
    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()>;
    fn force_idr(&mut self, generation: u32) -> Result<()>;
    fn input(&mut self, packet: &[u8]) -> Result<()>;
    fn select_monitor(&mut self, monitor_id: u32) -> Result<()>;
    fn list_monitors(&mut self) -> Result<Option<Message>>;
    fn release_all(&mut self) -> Result<()>;
    /// Forget the departed producer's sequencing after releasing its input.
    /// This is an internal backend operation, not a new IPC message.
    fn reset_input_session(&mut self) -> Result<()>;
    fn suspend(&mut self) -> Result<()>;
    fn resume(&mut self) -> Result<()>;
    fn transition_desktop(
        &mut self,
        desktop: DesktopKind,
        generation: u32,
    ) -> Result<DesktopBinding>;
    fn start_work(&mut self, request: Message) -> Result<()>;
    fn poll_work(&mut self) -> Result<Option<Message>>;
    /// Check the interactive session clipboard; changed text is queued as
    /// `Message::ClipboardText` chunks returned by `poll_work`. `reset`
    /// re-baselines without reporting the current contents.
    fn poll_clipboard(&mut self, _reset: bool) -> Result<()> {
        Ok(())
    }
}

pub trait DesktopSource: Send + 'static {
    fn current(&mut self) -> Result<DesktopKind>;
}

#[derive(Debug, Default)]
pub struct WindowsInputDesktopSource;

impl DesktopSource for WindowsInputDesktopSource {
    fn current(&mut self) -> Result<DesktopKind> {
        Ok(InputDesktop::open()?.kind())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    Control(Message),
    Media(Message),
}

#[derive(Debug, Clone, Copy)]
struct Lease {
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveConfig {
    session_generation: u32,
    config_revision: u32,
}

pub struct AgentCore<B: AgentBackend> {
    backend: B,
    lease: Option<Lease>,
    last_producer_lease_id: Option<u64>,
    input_session_reset_failed: bool,
    active_config: Option<ActiveConfig>,
    require_new_session: bool,
    outstanding: Option<PermitIdentity>,
    last_permit_id: Option<u64>,
    desktop_tracker: DesktopTracker,
    desktop: Option<DesktopKind>,
    desktop_generation: u32,
    desktop_ready: bool,
    input_capable: bool,
    input_ready: bool,
    last_announced_input_enabled: Option<bool>,
    suspended: bool,
    stopped: bool,
    clipboard_poll_at: Option<Instant>,
    clipboard_reset: bool,
}

impl<B: AgentBackend> AgentCore<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            lease: None,
            last_producer_lease_id: None,
            input_session_reset_failed: false,
            active_config: None,
            require_new_session: false,
            outstanding: None,
            last_permit_id: None,
            desktop_tracker: DesktopTracker::default(),
            desktop: None,
            desktop_generation: 0,
            desktop_ready: false,
            input_capable: false,
            input_ready: false,
            last_announced_input_enabled: None,
            // Explicitly suspend the backend on the first desktop observation.
            suspended: false,
            stopped: false,
            clipboard_poll_at: None,
            clipboard_reset: true,
        }
    }

    pub fn handle(&mut self, message: Message, now: Instant) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        self.expire_lease(now, &mut events);
        match message {
            Message::RenewLease { lease_id, .. } => {
                // Expiry removes authorization, but not producer identity: a
                // different viewer must also reset a proxy that stayed warm.
                let replacing = self.last_producer_lease_id.is_some_and(|id| id != lease_id);
                if replacing {
                    self.release_all();
                    self.suspend();
                    self.cancel_outstanding(NoFrameReason::Backpressured, &mut events);
                    self.require_new_session = self.active_config.is_some();
                    self.input_ready = false;
                    self.input_session_reset_failed = match self.backend.reset_input_session() {
                        Ok(()) => false,
                        Err(error) => {
                            tracing::warn!(
                                ?error,
                                "privileged input session reset failed; input remains suspended"
                            );
                            events.push(AgentEvent::Control(Message::RecoverableError(
                                AgentError {
                                    operation: AgentOperation::Input,
                                    error_code: -1,
                                    session_generation: self
                                        .active_config
                                        .map_or(0, |config| config.session_generation),
                                    config_revision: self
                                        .active_config
                                        .map_or(0, |config| config.config_revision),
                                    permit_id: None,
                                },
                            )));
                            true
                        }
                    };
                }
                self.last_producer_lease_id = Some(lease_id);
                self.lease = Some(Lease {
                    // The privileged boundary owns the lease duration. A peer
                    // cannot expand it by placing a larger value on the wire.
                    expires_at: now + LEASE_TTL,
                });
                self.resume_if_allowed(now);
                self.announce_input_state(
                    now,
                    DesktopTransitionReason::SessionChanged,
                    &mut events,
                );
            }
            Message::EncodePermit { .. } | Message::RequestIdleSnapshot { .. } => {
                self.accept_work(message, now, &mut events);
            }
            Message::ConfigureMedia(config) => {
                let identity = ActiveConfig {
                    session_generation: config.session_generation,
                    config_revision: config.config_revision,
                };
                let accepted =
                    self.outstanding.is_none() && self.identity_is_current_or_new(identity);
                let backend_accepted = accepted && self.backend.configure(&config).is_ok();
                if backend_accepted {
                    self.apply_config_identity(identity);
                }
                events.push(AgentEvent::Control(Message::ConfigAck {
                    session_generation: config.session_generation,
                    config_revision: config.config_revision,
                    accepted: backend_accepted,
                    error_code: if backend_accepted { 0 } else { -1 },
                }));
            }
            Message::SetBitrate { bitrate_bps } => {
                if self.lease_active(now) && !self.suspended {
                    let _ = self.backend.set_bitrate(bitrate_bps);
                }
            }
            Message::ForceIdr { generation } => {
                if self.lease_active(now) && !self.suspended {
                    let _ = self.backend.force_idr(generation);
                }
            }
            Message::Input(packet) => {
                if self.lease_active(now) && !self.suspended && self.input_ready {
                    let _ = self.backend.input(&packet);
                }
            }
            Message::SelectMonitor { monitor_id } => {
                if self.lease_active(now) && !self.suspended {
                    let _ = self.backend.select_monitor(monitor_id);
                }
            }
            Message::ListMonitors => {
                // Topology discovery is read-only and is required before the
                // first media permit (and while capture is unavailable).
                // Suspension gates GPU work and input, not monitor discovery.
                if self.lease_active(now) {
                    if let Ok(Some(message)) = self.backend.list_monitors() {
                        events.push(AgentEvent::Control(message));
                    }
                }
            }
            Message::ReleaseAll => self.release_all(),
            Message::Shutdown => {
                self.release_all();
                self.suspend();
                self.cancel_outstanding(NoFrameReason::Backpressured, &mut events);
                self.lease = None;
                self.stopped = true;
            }
            // Agent-to-transport events are never valid control commands.
            Message::Capabilities(_)
            | Message::DesktopTransition { .. }
            | Message::CaptureHealth { .. }
            | Message::MonitorList(_)
            | Message::Cursor(_)
            | Message::CursorKind(_)
            | Message::ConfigAck { .. }
            | Message::NoFrame { .. }
            | Message::EncodedAccessUnit(_)
            | Message::MonitorSwitchResult(_)
            | Message::InputStats(_)
            | Message::RecoverableError(_)
            | Message::FatalError(_)
            | Message::ClipboardText { .. }
            | Message::ClipboardPng { .. } => {}
        }
        events
    }

    pub fn tick(&mut self, now: Instant) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        self.expire_lease(now, &mut events);
        self.poll_clipboard(now);
        if let Ok(Some(result)) = self.backend.poll_work() {
            match result.permit_identity() {
                Some(identity) if Some(identity) == self.outstanding => {
                    self.outstanding = None;
                    events.push(AgentEvent::Media(result));
                }
                Some(_) => {
                    // A result for an old permit/generation is intentionally dropped.
                }
                None => {
                    if matches!(result, Message::CursorKind(_))
                        && (!self.lease_active(now) || self.suspended)
                    {
                        return events;
                    }
                    if let Message::CaptureHealth { state, .. } = &result {
                        if *state == CaptureState::Active {
                            // Active is meaningful only while the correlated
                            // permit and its lease are still live. A GPU call
                            // can return after expiry; forwarding that stale
                            // health event would reopen input over an
                            // unverified browser frame.
                            if !self.lease_active(now)
                                || self.outstanding.is_none()
                                || self.input_session_reset_failed
                            {
                                return events;
                            }
                            self.input_ready = self.input_capable;
                        } else {
                            self.input_ready = false;
                            self.release_all();
                        }
                    }
                    events.push(AgentEvent::Control(result));
                    self.announce_input_state(
                        now,
                        DesktopTransitionReason::SessionChanged,
                        &mut events,
                    );
                }
            }
        }
        events
    }

    pub fn observe_desktop(&mut self, desktop: DesktopKind, now: Instant) -> Vec<AgentEvent> {
        let Some(transition) = self.desktop_tracker.observe(desktop) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if transition.previous.is_some() || self.lease_active(now) {
            self.release_all();
        }
        self.suspend();
        self.cancel_outstanding(NoFrameReason::DesktopUnavailable, &mut events);
        self.desktop = Some(desktop);
        self.desktop_generation = transition.generation;
        self.desktop_ready = false;
        self.input_capable = false;
        self.input_ready = false;

        let binding = match self
            .backend
            .transition_desktop(desktop, transition.generation)
        {
            Ok(binding) => Some(binding),
            Err(error) => {
                tracing::warn!(
                    ?error,
                    ?desktop,
                    generation = transition.generation,
                    "privileged desktop backend bind failed"
                );
                events.push(AgentEvent::Control(Message::RecoverableError(AgentError {
                    operation: AgentOperation::Desktop,
                    error_code: -1,
                    session_generation: transition.generation,
                    config_revision: self
                        .active_config
                        .map_or(0, |config| config.config_revision),
                    permit_id: None,
                })));
                None
            }
        };
        if let Some(binding) = binding {
            self.desktop_ready = binding.work_ready;
            self.input_capable = binding.input_ready;
            // A bound input desktop is necessary but not sufficient: input
            // remains disabled until CaptureHealth::Active proves a frame from
            // this exact desktop generation.
            self.input_ready = false;
            self.resume_if_allowed(now);
        } else {
            events.push(AgentEvent::Control(Message::CaptureHealth {
                state: CaptureState::Unavailable,
                error_code: -1,
            }));
        }
        let input_enabled = binding.is_some()
            && self.lease_active(now)
            && desktop != DesktopKind::Other
            && self.desktop_ready
            && self.input_ready
            && !self.suspended;
        self.last_announced_input_enabled = Some(input_enabled);
        events.push(AgentEvent::Control(Message::DesktopTransition {
            desktop: desktop_to_wire(desktop),
            generation: transition.generation,
            input_enabled,
            reason: if transition.previous.is_none() {
                DesktopTransitionReason::Startup
            } else {
                DesktopTransitionReason::DesktopSwitched
            },
        }));
        events
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    fn accept_work(&mut self, request: Message, now: Instant, events: &mut Vec<AgentEvent>) {
        let identity = request
            .permit_identity()
            .expect("work request must carry permit identity");
        let requested_config = ActiveConfig {
            session_generation: identity.session_generation,
            config_revision: identity.config_revision,
        };
        let permit_is_new = self.last_permit_id.is_none_or(|last| {
            self.active_config
                .is_some_and(|active| active.session_generation != identity.session_generation)
                || wrapping_newer_u64(identity.permit_id, last)
        });
        let accepted = self.lease_active(now)
            && self
                .desktop
                .is_some_and(|desktop| desktop != DesktopKind::Other)
            && self.desktop_ready
            && self.outstanding.is_none()
            && permit_is_new
            && self.identity_is_current_or_new(requested_config);
        if !accepted {
            events.push(AgentEvent::Media(no_frame(
                identity,
                NoFrameReason::Backpressured,
            )));
            return;
        }

        let changes_session = self
            .active_config
            .is_none_or(|active| active.session_generation != identity.session_generation);
        if changes_session {
            self.last_permit_id = None;
        }
        self.apply_config_identity(requested_config);
        self.outstanding = Some(identity);
        match self.backend.start_work(request) {
            Ok(()) => self.last_permit_id = Some(identity.permit_id),
            Err(_) => {
                self.outstanding = None;
                events.push(AgentEvent::Media(no_frame(
                    identity,
                    NoFrameReason::Backpressured,
                )));
            }
        }
    }

    fn identity_is_current_or_new(&self, requested: ActiveConfig) -> bool {
        match self.active_config {
            None => true,
            Some(active) if requested.session_generation == active.session_generation => {
                !self.require_new_session
                    && (requested.config_revision == active.config_revision
                        || wrapping_newer_u32(requested.config_revision, active.config_revision))
            }
            Some(active) => {
                wrapping_newer_u32(requested.session_generation, active.session_generation)
            }
        }
    }

    fn apply_config_identity(&mut self, requested: ActiveConfig) {
        let changes_session = self
            .active_config
            .is_some_and(|active| active.session_generation != requested.session_generation);
        if changes_session {
            self.last_permit_id = None;
        }
        self.active_config = Some(requested);
        self.require_new_session = false;
    }

    fn expire_lease(&mut self, now: Instant, events: &mut Vec<AgentEvent>) {
        let expired = self.lease.is_some_and(|lease| now >= lease.expires_at);
        if !expired {
            return;
        }
        self.lease = None;
        self.input_ready = false;
        self.release_all();
        self.suspend();
        self.cancel_outstanding(NoFrameReason::Backpressured, events);
        self.last_announced_input_enabled = Some(false);
        events.push(AgentEvent::Control(Message::CaptureHealth {
            state: CaptureState::Unavailable,
            error_code: 0,
        }));
    }

    fn lease_active(&self, now: Instant) -> bool {
        self.lease.is_some_and(|lease| now < lease.expires_at)
    }

    /// Host-to-browser clipboard sync runs only while a leased session has
    /// verified input on the interactive default desktop. Any gap (lease
    /// change, secure desktop, suspension) re-baselines so text copied while
    /// the stream was not live is never sent.
    fn poll_clipboard(&mut self, now: Instant) {
        let eligible = self.lease_active(now)
            && self.input_ready
            && !self.suspended
            && self.desktop == Some(DesktopKind::Default);
        if !eligible {
            self.clipboard_reset = true;
            self.clipboard_poll_at = None;
            return;
        }
        if self.clipboard_poll_at.is_some_and(|at| now < at) {
            return;
        }
        self.clipboard_poll_at = Some(now + CLIPBOARD_POLL_INTERVAL);
        let reset = std::mem::replace(&mut self.clipboard_reset, false);
        if self.backend.poll_clipboard(reset).is_err() {
            self.clipboard_reset = true;
        }
    }

    fn release_all(&mut self) {
        let _ = self.backend.release_all();
    }

    fn suspend(&mut self) {
        if !self.suspended {
            let _ = self.backend.suspend();
            self.suspended = true;
        }
    }

    fn resume_if_allowed(&mut self, now: Instant) {
        let desktop_available = self
            .desktop
            .is_some_and(|desktop| desktop != DesktopKind::Other);
        if self.suspended
            && self.lease_active(now)
            && desktop_available
            && self.desktop_ready
            && !self.input_session_reset_failed
            && self.backend.resume().is_ok()
        {
            self.suspended = false;
        }
    }

    fn cancel_outstanding(&mut self, reason: NoFrameReason, events: &mut Vec<AgentEvent>) {
        if let Some(identity) = self.outstanding.take() {
            events.push(AgentEvent::Media(no_frame(identity, reason)));
        }
    }

    fn announce_input_state(
        &mut self,
        now: Instant,
        reason: DesktopTransitionReason,
        events: &mut Vec<AgentEvent>,
    ) {
        let Some(desktop) = self.desktop else {
            return;
        };
        let input_enabled = self.lease_active(now)
            && desktop != DesktopKind::Other
            && self.desktop_ready
            && self.input_ready
            && !self.suspended;
        if self.last_announced_input_enabled == Some(input_enabled) {
            return;
        }
        self.last_announced_input_enabled = Some(input_enabled);
        events.push(AgentEvent::Control(Message::DesktopTransition {
            desktop: desktop_to_wire(desktop),
            generation: self.desktop_generation,
            input_enabled,
            reason,
        }));
    }
}

fn no_frame(identity: PermitIdentity, reason: NoFrameReason) -> Message {
    Message::NoFrame {
        permit_id: identity.permit_id,
        session_generation: identity.session_generation,
        config_revision: identity.config_revision,
        reason,
    }
}

fn desktop_to_wire(desktop: DesktopKind) -> Desktop {
    match desktop {
        DesktopKind::Default => Desktop::Default,
        DesktopKind::Secure => Desktop::Winlogon,
        DesktopKind::Other => Desktop::Unknown,
    }
}

fn wrapping_newer_u32(candidate: u32, current: u32) -> bool {
    let distance = candidate.wrapping_sub(current);
    distance != 0 && distance < (1u32 << 31)
}

fn wrapping_newer_u64(candidate: u64, current: u64) -> bool {
    let distance = candidate.wrapping_sub(current);
    distance != 0 && distance < (1u64 << 63)
}

pub struct AgentRuntimeHandle {
    pub control: SyncSender<Message>,
    pub control_events: Receiver<Message>,
    pub media_events: Receiver<Message>,
    join: Option<JoinHandle<()>>,
}

impl AgentRuntimeHandle {
    pub fn join(mut self) -> thread::Result<()> {
        drop(self.control);
        self.join.take().expect("runtime join handle").join()
    }
}

pub fn spawn_agent_runtime<B, S>(backend: B, desktop_source: S) -> AgentRuntimeHandle
where
    B: AgentBackend,
    S: DesktopSource,
{
    let (control_tx, control_rx) = mpsc::sync_channel(CONTROL_CHANNEL_CAPACITY);
    let (control_event_tx, control_events) = mpsc::sync_channel(CONTROL_CHANNEL_CAPACITY);
    let (media_event_tx, media_events) = mpsc::sync_channel(MEDIA_CHANNEL_CAPACITY);
    let join = thread::Builder::new()
        .name("bubbleplay-agent-runtime".into())
        .spawn(move || {
            run_loop(
                AgentCore::new(backend),
                desktop_source,
                control_rx,
                control_event_tx,
                media_event_tx,
            );
        })
        .expect("spawn BubblePlay agent runtime");

    AgentRuntimeHandle {
        control: control_tx,
        control_events,
        media_events,
        join: Some(join),
    }
}

fn run_loop<B, S>(
    mut core: AgentCore<B>,
    mut desktop_source: S,
    control_rx: Receiver<Message>,
    control_tx: SyncSender<Message>,
    media_tx: SyncSender<Message>,
) where
    B: AgentBackend,
    S: DesktopSource,
{
    let mut outputs = VecDeque::new();
    let mut next_desktop_poll = Instant::now();
    loop {
        let now = Instant::now();
        if now >= next_desktop_poll {
            if let Ok(desktop) = desktop_source.current() {
                outputs.extend(core.observe_desktop(desktop, now));
            }
            next_desktop_poll = now + DESKTOP_POLL_INTERVAL;
        }
        outputs.extend(core.tick(now));
        coalesce_pending_cursor_kinds(&mut outputs);
        coalesce_pending_clipboards(&mut outputs);

        if !flush_outputs(&mut outputs, &control_tx, &media_tx) {
            break;
        }
        if core.is_stopped() && outputs.is_empty() {
            break;
        }

        match control_rx.recv_timeout(LOOP_TICK) {
            Ok(message) => outputs.extend(core.handle(message, Instant::now())),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                outputs.extend(core.handle(Message::Shutdown, Instant::now()));
            }
        }
    }
}

fn coalesce_pending_clipboards(outputs: &mut VecDeque<AgentEvent>) {
    let snapshot = |event: &AgentEvent| match event {
        AgentEvent::Control(Message::ClipboardText { id, .. }) => Some((*id, false)),
        AgentEvent::Control(Message::ClipboardPng { id, .. }) => Some((*id, true)),
        _ => None,
    };
    let latest = outputs.iter().rev().find_map(snapshot);
    outputs.retain(|event| snapshot(event).is_none_or(|id| Some(id) == latest));
}

fn coalesce_pending_cursor_kinds(outputs: &mut VecDeque<AgentEvent>) {
    let latest = outputs
        .iter()
        .rposition(|event| matches!(event, AgentEvent::Control(Message::CursorKind(_))));
    if let Some(latest) = latest {
        let mut index = 0;
        outputs.retain(|event| {
            let keep =
                index == latest || !matches!(event, AgentEvent::Control(Message::CursorKind(_)));
            index += 1;
            keep
        });
    }
}
fn flush_outputs(
    outputs: &mut VecDeque<AgentEvent>,
    control_tx: &SyncSender<Message>,
    media_tx: &SyncSender<Message>,
) -> bool {
    while let Some(event) = outputs.pop_front() {
        match event {
            AgentEvent::Control(message) => match control_tx.try_send(message) {
                Ok(()) => {}
                Err(TrySendError::Full(message)) => {
                    outputs.push_front(AgentEvent::Control(message));
                    return true;
                }
                Err(TrySendError::Disconnected(_)) => return false,
            },
            AgentEvent::Media(message) => match media_tx.try_send(message) {
                Ok(()) => {}
                Err(TrySendError::Full(message)) => {
                    outputs.push_front(AgentEvent::Media(message));
                    return true;
                }
                Err(TrySendError::Disconnected(_)) => return false,
            },
        }
    }
    true
}

/// Placeholder backend used by the console boundary harness. It deliberately
/// does not capture or inject input yet; every accepted permit completes with
/// `NoChange`, preserving correlation and backpressure semantics.
#[derive(Default)]
pub struct BoundaryBackend {
    pending: Option<PermitIdentity>,
}

impl AgentBackend for BoundaryBackend {
    fn configure(&mut self, _config: &MediaConfig) -> Result<()> {
        Ok(())
    }
    fn set_bitrate(&mut self, _bitrate_bps: u32) -> Result<()> {
        Ok(())
    }
    fn force_idr(&mut self, _generation: u32) -> Result<()> {
        Ok(())
    }
    fn input(&mut self, _packet: &[u8]) -> Result<()> {
        Ok(())
    }
    fn select_monitor(&mut self, _monitor_id: u32) -> Result<()> {
        Ok(())
    }
    fn list_monitors(&mut self) -> Result<Option<Message>> {
        Ok(None)
    }
    fn release_all(&mut self) -> Result<()> {
        Ok(())
    }
    fn reset_input_session(&mut self) -> Result<()> {
        Ok(())
    }
    fn suspend(&mut self) -> Result<()> {
        Ok(())
    }
    fn resume(&mut self) -> Result<()> {
        Ok(())
    }
    fn transition_desktop(
        &mut self,
        _desktop: DesktopKind,
        _generation: u32,
    ) -> Result<DesktopBinding> {
        // This harness owns no USER/D3D/input objects, so it can exercise the
        // permit protocol but must never advertise privileged input readiness.
        Ok(DesktopBinding {
            work_ready: true,
            input_ready: false,
        })
    }
    fn start_work(&mut self, request: Message) -> Result<()> {
        self.pending = request.permit_identity();
        Ok(())
    }
    fn poll_work(&mut self) -> Result<Option<Message>> {
        Ok(self.pending.take().map(|identity| Message::NoFrame {
            permit_id: identity.permit_id,
            session_generation: identity.session_generation,
            config_revision: identity.config_revision,
            reason: NoFrameReason::NoChange,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::TryRecvError;
    use std::sync::{Arc, Mutex};

    use crate::privileged_ipc::{Codec, EncoderPreset, StreamMode};

    #[derive(Debug, Default)]
    struct FakeState {
        releases: usize,
        input_session_resets: usize,
        fail_input_session_reset: bool,
        suspends: usize,
        resumes: usize,
        inputs: usize,
        monitor_requests: usize,
        clipboard_polls: Vec<bool>,
        fail_transition: bool,
        transitions: Vec<(DesktopKind, u32)>,
        started: Vec<PermitIdentity>,
        pending_results: VecDeque<Message>,
    }

    #[derive(Clone, Default)]
    struct FakeBackend(Arc<Mutex<FakeState>>);

    impl AgentBackend for FakeBackend {
        fn reset_input_session(&mut self) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.input_session_resets += 1;
            if state.fail_input_session_reset {
                anyhow::bail!("input session reset failed");
            }
            Ok(())
        }
        fn configure(&mut self, _config: &MediaConfig) -> Result<()> {
            Ok(())
        }
        fn set_bitrate(&mut self, _bitrate_bps: u32) -> Result<()> {
            Ok(())
        }
        fn force_idr(&mut self, _generation: u32) -> Result<()> {
            Ok(())
        }
        fn input(&mut self, _packet: &[u8]) -> Result<()> {
            self.0.lock().unwrap().inputs += 1;
            Ok(())
        }
        fn select_monitor(&mut self, _monitor_id: u32) -> Result<()> {
            Ok(())
        }
        fn list_monitors(&mut self) -> Result<Option<Message>> {
            self.0.lock().unwrap().monitor_requests += 1;
            Ok(Some(Message::MonitorList(vec![
                crate::privileged_ipc::MonitorInfo {
                    id: 1,
                    width: 1920,
                    height: 1080,
                    primary: true,
                    name: "Fixture display".into(),
                },
            ])))
        }
        fn release_all(&mut self) -> Result<()> {
            self.0.lock().unwrap().releases += 1;
            Ok(())
        }
        fn suspend(&mut self) -> Result<()> {
            self.0.lock().unwrap().suspends += 1;
            Ok(())
        }
        fn resume(&mut self) -> Result<()> {
            self.0.lock().unwrap().resumes += 1;
            Ok(())
        }
        fn transition_desktop(
            &mut self,
            desktop: DesktopKind,
            generation: u32,
        ) -> Result<DesktopBinding> {
            self.0
                .lock()
                .unwrap()
                .transitions
                .push((desktop, generation));
            if self.0.lock().unwrap().fail_transition {
                anyhow::bail!("fixture desktop bind failed");
            }
            Ok(DesktopBinding {
                work_ready: true,
                input_ready: true,
            })
        }
        fn start_work(&mut self, request: Message) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .started
                .push(request.permit_identity().unwrap());
            Ok(())
        }
        fn poll_work(&mut self) -> Result<Option<Message>> {
            Ok(self.0.lock().unwrap().pending_results.pop_front())
        }
        fn poll_clipboard(&mut self, reset: bool) -> Result<()> {
            self.0.lock().unwrap().clipboard_polls.push(reset);
            Ok(())
        }
    }

    #[test]
    fn clipboard_backpressure_keeps_latest_snapshot_and_other_control() {
        let mut outputs = VecDeque::from([
            AgentEvent::Control(Message::ClipboardText {
                id: 1,
                index: 0,
                count: 1,
                text: "old".into(),
            }),
            AgentEvent::Control(Message::ReleaseAll),
            AgentEvent::Control(Message::ClipboardPng {
                id: 2,
                index: 0,
                count: 2,
                text: "AAAA".into(),
            }),
            AgentEvent::Control(Message::ClipboardPng {
                id: 2,
                index: 1,
                count: 2,
                text: "BBBB".into(),
            }),
        ]);
        coalesce_pending_clipboards(&mut outputs);
        assert_eq!(outputs.len(), 3);
        assert_eq!(outputs[0], AgentEvent::Control(Message::ReleaseAll));
        assert!(outputs.iter().skip(1).all(|event| matches!(
            event,
            AgentEvent::Control(Message::ClipboardPng { id: 2, .. })
        )));
    }

    #[test]
    fn clipboard_is_polled_only_while_verified_input_is_live_on_the_default_desktop() {
        let backend = FakeBackend::default();
        let state = backend.0.clone();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        core.observe_desktop(DesktopKind::Default, now);
        core.tick(now);
        assert!(state.lock().unwrap().clipboard_polls.is_empty(), "no lease");
        core.handle(
            Message::RenewLease {
                lease_id: 1,
                ttl_ms: 2_000,
            },
            now,
        );
        core.tick(now);
        assert!(
            state.lock().unwrap().clipboard_polls.is_empty(),
            "input is not verified before the first encoded frame"
        );
        assert!(core.handle(permit(1, 1, 1), now).is_empty());
        state
            .lock()
            .unwrap()
            .pending_results
            .push_back(Message::CaptureHealth {
                state: CaptureState::Active,
                error_code: 0,
            });
        core.tick(now);
        core.tick(now);
        core.tick(now + Duration::from_millis(100));
        core.tick(now + CLIPBOARD_POLL_INTERVAL);
        assert_eq!(state.lock().unwrap().clipboard_polls, vec![true, false]);
        state
            .lock()
            .unwrap()
            .pending_results
            .push_back(Message::ClipboardText {
                id: 1,
                index: 0,
                count: 1,
                text: "copied".into(),
            });
        assert!(core
            .tick(now + CLIPBOARD_POLL_INTERVAL)
            .iter()
            .any(|event| matches!(event, AgentEvent::Control(Message::ClipboardText { .. }))));

        // Moving to the secure desktop stops polling and forces a new baseline.
        core.observe_desktop(DesktopKind::Secure, now + CLIPBOARD_POLL_INTERVAL * 2);
        core.tick(now + CLIPBOARD_POLL_INTERVAL * 3);
        assert_eq!(state.lock().unwrap().clipboard_polls.len(), 2);
        assert!(core.clipboard_reset);
    }

    #[test]
    fn desktop_bind_failure_is_reported_as_structured_control_telemetry() {
        let backend = FakeBackend::default();
        backend.0.lock().unwrap().fail_transition = true;
        let mut core = AgentCore::new(backend);

        let events = core.observe_desktop(DesktopKind::Default, Instant::now());

        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Control(Message::RecoverableError(error))
                if error.operation == crate::privileged_ipc::AgentOperation::Desktop
                    && error.error_code == -1
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Control(Message::CaptureHealth {
                state: CaptureState::Unavailable,
                error_code: -1,
            })
        )));
    }

    fn permit(id: u64, session_generation: u32, config_revision: u32) -> Message {
        Message::EncodePermit {
            permit_id: id,
            session_generation,
            config_revision,
            stream_mode: StreamMode::Work,
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_bps: 3_000_000,
            codec: Codec::Hevc,
            preset: EncoderPreset::P3,
            tuning: crate::privileged_ipc::EncoderTuning::UltraLowLatency,
            multipass: crate::privileged_ipc::EncoderMultipass::Disabled,
            aq_mode: crate::privileged_ipc::EncoderAqMode::Spatial,
            aq_strength: 8,
            vbv_frames: 1,
            lookahead_depth: 0,
            weighted_prediction: false,
            gop_seconds: 2,
            crop: None,
            force_idr: false,
            allow_idle_snapshot: true,
        }
    }

    fn config(session_generation: u32, config_revision: u32) -> MediaConfig {
        MediaConfig {
            session_generation,
            config_revision,
            codec: Codec::Hevc,
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_bps: 3_000_000,
            stream_mode: StreamMode::Work,
        }
    }

    #[test]
    fn monitor_discovery_remains_available_while_media_and_input_are_suspended() {
        let backend = FakeBackend::default();
        let state = backend.0.clone();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        core.observe_desktop(DesktopKind::Default, now);
        core.handle(
            Message::RenewLease {
                lease_id: 7,
                ttl_ms: 2_000,
            },
            now,
        );
        core.suspend();

        let events = core.handle(Message::ListMonitors, now);

        assert_eq!(state.lock().unwrap().monitor_requests, 1);
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Control(Message::MonitorList(monitors)) if monitors.len() == 1
        )));
    }

    fn completed(identity: PermitIdentity) -> Message {
        Message::NoFrame {
            permit_id: identity.permit_id,
            session_generation: identity.session_generation,
            config_revision: identity.config_revision,
            reason: NoFrameReason::NoChange,
        }
    }

    #[test]
    fn lease_expiry_releases_input_suspends_work_and_completes_the_permit() {
        let backend = FakeBackend::default();
        let state = backend.0.clone();
        let mut core = AgentCore::new(backend);
        let start = Instant::now();
        core.observe_desktop(DesktopKind::Default, start);
        core.handle(
            Message::RenewLease {
                lease_id: 7,
                ttl_ms: 99_000,
            },
            start,
        );
        core.handle(permit(11, 3, 4), start);
        state
            .lock()
            .unwrap()
            .pending_results
            .push_back(Message::CaptureHealth {
                state: CaptureState::Active,
                error_code: 0,
            });

        let events = core.tick(start + LEASE_TTL + Duration::from_millis(1));
        assert!(!events.iter().any(|event| matches!(
            event,
            AgentEvent::Control(Message::CaptureHealth {
                state: CaptureState::Active,
                ..
            })
        )));
        let locked = state.lock().unwrap();
        assert_eq!(locked.releases, 1);
        assert_eq!(locked.suspends, 2); // initial desktop bind, then expiry
        drop(locked);
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Media(Message::NoFrame {
                permit_id: 11,
                session_generation: 3,
                config_revision: 4,
                reason: NoFrameReason::Backpressured,
            })
        )));

        let renewed = start + LEASE_TTL + Duration::from_millis(2);
        core.handle(
            Message::RenewLease {
                lease_id: 8,
                ttl_ms: u32::MAX,
            },
            renewed,
        );
        core.handle(Message::Input(vec![1]), renewed);
        assert_eq!(state.lock().unwrap().inputs, 0);

        // Lease 8 is a new producer, so an old-generation permit cannot
        // reopen input even though authorization has been renewed.
        assert!(matches!(
            core.handle(permit(12, 3, 4), renewed).as_slice(),
            [AgentEvent::Media(Message::NoFrame { .. })]
        ));
        core.handle(Message::Input(vec![1]), renewed);
        assert_eq!(state.lock().unwrap().inputs, 0);
        assert!(core.handle(permit(12, 4, 4), renewed).is_empty());
        state
            .lock()
            .unwrap()
            .pending_results
            .push_back(Message::CaptureHealth {
                state: CaptureState::Active,
                error_code: 0,
            });
        core.tick(renewed);
        core.handle(Message::Input(vec![1]), renewed);
        assert_eq!(state.lock().unwrap().inputs, 1);
    }

    #[test]
    fn one_permit_is_outstanding_and_stale_backend_results_do_not_clear_it() {
        let backend = FakeBackend::default();
        let state = backend.0.clone();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        core.observe_desktop(DesktopKind::Default, now);
        core.handle(
            Message::RenewLease {
                lease_id: 1,
                ttl_ms: 2_000,
            },
            now,
        );

        assert!(core.handle(permit(10, 2, 3), now).is_empty());
        let rejected = core.handle(permit(11, 2, 3), now);
        assert!(matches!(
            rejected.as_slice(),
            [AgentEvent::Media(Message::NoFrame {
                permit_id: 11,
                reason: NoFrameReason::Backpressured,
                ..
            })]
        ));

        let stale = PermitIdentity {
            permit_id: 9,
            session_generation: 1,
            config_revision: 1,
        };
        let current = PermitIdentity {
            permit_id: 10,
            session_generation: 2,
            config_revision: 3,
        };
        {
            let mut state = state.lock().unwrap();
            state.pending_results.push_back(completed(stale));
            state.pending_results.push_back(completed(current));
        }
        assert!(core.tick(now).is_empty());
        assert!(matches!(
            core.handle(permit(12, 2, 3), now).as_slice(),
            [AgentEvent::Media(Message::NoFrame { permit_id: 12, .. })]
        ));
        assert!(matches!(
            core.tick(now).as_slice(),
            [AgentEvent::Media(Message::NoFrame { permit_id: 10, .. })]
        ));
        assert!(core.handle(permit(12, 2, 3), now).is_empty());

        let state = state.lock().unwrap();
        assert_eq!(
            state.started,
            vec![
                current,
                PermitIdentity {
                    permit_id: 12,
                    session_generation: 2,
                    config_revision: 3,
                }
            ]
        );
    }

    #[test]
    fn stale_sessions_revisions_and_duplicate_permits_are_rejected() {
        let backend = FakeBackend::default();
        let state = backend.0.clone();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        core.observe_desktop(DesktopKind::Default, now);
        core.handle(
            Message::RenewLease {
                lease_id: 1,
                ttl_ms: 2_000,
            },
            now,
        );
        assert!(matches!(
            core.handle(Message::ConfigureMedia(config(5, 8)), now)
                .as_slice(),
            [AgentEvent::Control(Message::ConfigAck {
                accepted: true,
                ..
            })]
        ));

        for stale in [permit(1, 4, 8), permit(2, 5, 7)] {
            assert!(matches!(
                core.handle(stale, now).as_slice(),
                [AgentEvent::Media(Message::NoFrame {
                    reason: NoFrameReason::Backpressured,
                    ..
                })]
            ));
        }
        assert!(core.handle(permit(3, 5, 8), now).is_empty());
        let identity = PermitIdentity {
            permit_id: 3,
            session_generation: 5,
            config_revision: 8,
        };
        state
            .lock()
            .unwrap()
            .pending_results
            .push_back(completed(identity));
        core.tick(now);
        assert!(matches!(
            core.handle(permit(3, 5, 8), now).as_slice(),
            [AgentEvent::Media(Message::NoFrame { permit_id: 3, .. })]
        ));
        assert_eq!(state.lock().unwrap().started, vec![identity]);
    }

    #[test]
    fn replacing_a_lease_requires_a_new_session_generation() {
        let backend = FakeBackend::default();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        core.observe_desktop(DesktopKind::Default, now);
        core.handle(
            Message::RenewLease {
                lease_id: 10,
                ttl_ms: 2_000,
            },
            now,
        );
        core.handle(Message::ConfigureMedia(config(7, 1)), now);
        core.handle(
            Message::RenewLease {
                lease_id: 11,
                ttl_ms: 2_000,
            },
            now,
        );

        assert!(matches!(
            core.handle(permit(1, 7, 1), now).as_slice(),
            [AgentEvent::Media(Message::NoFrame { .. })]
        ));
        assert!(core.handle(permit(1, 8, 1), now).is_empty());
    }

    #[test]
    fn producer_lease_replacement_resets_input_once_and_preserves_ordinary_barriers() {
        let backend = FakeBackend::default();
        let state = backend.0.clone();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        core.observe_desktop(DesktopKind::Default, now);
        let renewal = |lease_id| Message::RenewLease {
            lease_id,
            ttl_ms: 2_000,
        };
        core.handle(renewal(10), now);
        core.handle(renewal(10), now);
        core.handle(Message::ReleaseAll, now);
        core.handle(Message::ConfigureMedia(config(1, 1)), now);
        core.handle(Message::ConfigureMedia(config(1, 2)), now);
        core.handle(Message::SelectMonitor { monitor_id: 0 }, now);
        core.suspend();
        core.resume_if_allowed(now);
        assert_eq!(state.lock().unwrap().input_session_resets, 0);

        core.handle(renewal(11), now);
        core.handle(renewal(11), now);
        core.handle(Message::ReleaseAll, now);
        assert_eq!(state.lock().unwrap().input_session_resets, 1);
        assert!(core.require_new_session);
        assert!(!core.input_ready);
    }

    #[test]
    fn producer_identity_survives_lease_expiry_for_input_session_reset() {
        let backend = FakeBackend::default();
        let state = backend.0.clone();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        core.observe_desktop(DesktopKind::Default, now);
        let renewal = |lease_id| Message::RenewLease {
            lease_id,
            ttl_ms: 2_000,
        };
        core.handle(renewal(10), now);
        let expired = now + LEASE_TTL + Duration::from_millis(1);
        core.tick(expired);
        assert!(core.lease.is_none());
        core.handle(renewal(10), expired);
        assert_eq!(state.lock().unwrap().input_session_resets, 0);
        core.tick(expired + LEASE_TTL + Duration::from_millis(1));
        core.handle(renewal(11), expired + LEASE_TTL + Duration::from_millis(1));
        core.handle(renewal(11), expired + LEASE_TTL + Duration::from_millis(2));
        assert_eq!(state.lock().unwrap().input_session_resets, 1);
    }

    #[test]
    fn failed_input_session_reset_stays_suspended_on_same_lease_heartbeat() {
        let backend = FakeBackend::default();
        let state = backend.0.clone();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        core.observe_desktop(DesktopKind::Default, now);
        let renewal = |lease_id| Message::RenewLease {
            lease_id,
            ttl_ms: 2_000,
        };
        core.handle(renewal(10), now);
        state.lock().unwrap().fail_input_session_reset = true;
        core.handle(renewal(11), now);
        core.handle(renewal(11), now);
        core.handle(permit(1, 1, 1), now);
        state
            .lock()
            .unwrap()
            .pending_results
            .push_back(Message::CaptureHealth {
                state: CaptureState::Active,
                error_code: 0,
            });
        let events = core.tick(now);
        assert!(!events.iter().any(|event| matches!(
            event,
            AgentEvent::Control(Message::CaptureHealth {
                state: CaptureState::Active,
                ..
            })
        )));
        core.handle(Message::Input(vec![1]), now);
        assert!(core.suspended);
        assert!(!core.input_ready);
        assert!(core.input_session_reset_failed);
        assert_eq!(state.lock().unwrap().inputs, 0);
        assert_eq!(state.lock().unwrap().input_session_resets, 1);

        state.lock().unwrap().fail_input_session_reset = false;
        core.handle(renewal(12), now);
        assert!(!core.input_session_reset_failed);
        assert!(!core.suspended);
        assert_eq!(state.lock().unwrap().input_session_resets, 2);
    }

    #[test]
    fn new_producer_reopens_input_only_after_new_generation_capture_is_verified() {
        let backend = FakeBackend::default();
        let state = backend.0.clone();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        core.observe_desktop(DesktopKind::Default, now);
        let renewal = |lease_id| Message::RenewLease {
            lease_id,
            ttl_ms: 2_000,
        };
        let active = || Message::CaptureHealth {
            state: CaptureState::Active,
            error_code: 0,
        };
        core.handle(renewal(10), now);
        core.handle(permit(1, 7, 1), now);
        state.lock().unwrap().pending_results.push_back(active());
        core.tick(now);
        core.handle(Message::Input(vec![1]), now);
        assert_eq!(state.lock().unwrap().inputs, 1);

        core.handle(renewal(11), now);
        core.handle(Message::Input(vec![1]), now);
        assert_eq!(state.lock().unwrap().inputs, 1);
        assert!(matches!(
            core.handle(permit(2, 7, 1), now).as_slice(),
            [AgentEvent::Media(Message::NoFrame { .. })]
        ));
        // A delayed health notification from the cancelled producer has no
        // outstanding new permit and cannot publish readiness.
        state.lock().unwrap().pending_results.push_back(active());
        assert!(core.tick(now).is_empty());
        core.handle(Message::ConfigureMedia(config(8, 1)), now);
        core.handle(Message::Input(vec![1]), now);
        assert_eq!(state.lock().unwrap().inputs, 1);
        core.handle(permit(2, 8, 1), now);
        state.lock().unwrap().pending_results.push_back(active());
        core.tick(now);
        core.handle(Message::Input(vec![1]), now);
        assert_eq!(state.lock().unwrap().inputs, 2);
    }

    #[test]
    fn desktop_switch_releases_input_cancels_work_and_rebinds_once() {
        let backend = FakeBackend::default();
        let state = backend.0.clone();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        let startup = core.observe_desktop(DesktopKind::Default, now);
        assert!(matches!(
            startup.as_slice(),
            [AgentEvent::Control(Message::DesktopTransition {
                desktop: Desktop::Default,
                generation: 1,
                input_enabled: false,
                reason: DesktopTransitionReason::Startup,
            })]
        ));
        core.handle(
            Message::RenewLease {
                lease_id: 1,
                ttl_ms: 2_000,
            },
            now,
        );
        core.handle(permit(20, 1, 1), now);

        let switched = core.observe_desktop(DesktopKind::Secure, now);
        assert!(switched.iter().any(|event| matches!(
            event,
            AgentEvent::Media(Message::NoFrame {
                permit_id: 20,
                reason: NoFrameReason::DesktopUnavailable,
                ..
            })
        )));
        assert!(switched.iter().any(|event| matches!(
            event,
            AgentEvent::Control(Message::DesktopTransition {
                desktop: Desktop::Winlogon,
                generation: 2,
                input_enabled: false,
                reason: DesktopTransitionReason::DesktopSwitched,
            })
        )));
        assert!(core.handle(permit(21, 1, 1), now).is_empty());
        state
            .lock()
            .unwrap()
            .pending_results
            .push_back(Message::CaptureHealth {
                state: CaptureState::Active,
                error_code: 0,
            });
        let verified = core.tick(now);
        assert!(verified.iter().any(|event| matches!(
            event,
            AgentEvent::Control(Message::DesktopTransition {
                desktop: Desktop::Winlogon,
                generation: 2,
                input_enabled: true,
                reason: DesktopTransitionReason::SessionChanged,
            })
        )));
        assert!(core.observe_desktop(DesktopKind::Secure, now).is_empty());

        let state = state.lock().unwrap();
        assert_eq!(
            state.transitions,
            vec![(DesktopKind::Default, 1), (DesktopKind::Secure, 2)]
        );
        assert_eq!(state.releases, 1);
        assert_eq!(state.resumes, 2);
    }

    #[test]
    fn explicit_release_all_reaches_backend_without_ending_the_lease() {
        let backend = FakeBackend::default();
        let state = backend.0.clone();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        core.observe_desktop(DesktopKind::Default, now);
        core.handle(
            Message::RenewLease {
                lease_id: 1,
                ttl_ms: 2_000,
            },
            now,
        );
        assert!(core.handle(permit(1, 1, 1), now).is_empty());
        state
            .lock()
            .unwrap()
            .pending_results
            .push_back(Message::CaptureHealth {
                state: CaptureState::Active,
                error_code: 0,
            });
        core.tick(now);
        core.handle(Message::ReleaseAll, now);
        core.handle(Message::Input(vec![1, 2, 3]), now);

        let state = state.lock().unwrap();
        assert_eq!(state.releases, 1);
        assert_eq!(state.inputs, 1);
    }

    struct StaticDesktop(DesktopKind);

    impl DesktopSource for StaticDesktop {
        fn current(&mut self) -> Result<DesktopKind> {
            Ok(self.0)
        }
    }

    #[test]
    fn runtime_delivers_control_and_correlated_media_on_separate_bounded_channels() {
        let runtime = spawn_agent_runtime(
            BoundaryBackend::default(),
            StaticDesktop(DesktopKind::Default),
        );
        assert!(matches!(
            runtime
                .control_events
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            Message::DesktopTransition {
                desktop: Desktop::Default,
                ..
            }
        ));
        runtime
            .control
            .send(Message::RenewLease {
                lease_id: 1,
                ttl_ms: u32::MAX,
            })
            .unwrap();
        runtime.control.send(permit(77, 1, 1)).unwrap();
        assert!(matches!(
            runtime
                .media_events
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            Message::NoFrame {
                permit_id: 77,
                session_generation: 1,
                config_revision: 1,
                reason: NoFrameReason::NoChange,
            }
        ));
        assert!(matches!(
            runtime.control_events.try_recv(),
            Err(TryRecvError::Empty)
        ));
        runtime.control.send(Message::Shutdown).unwrap();
        runtime.join().unwrap();
    }

    #[test]
    fn lease_activation_reannounces_ready_input_after_startup() {
        let backend = FakeBackend::default();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        let startup = core.observe_desktop(DesktopKind::Default, now);
        assert!(matches!(
            startup.as_slice(),
            [AgentEvent::Control(Message::DesktopTransition {
                input_enabled: false,
                ..
            })]
        ));
        let activated = core.handle(
            Message::RenewLease {
                lease_id: 1,
                ttl_ms: u32::MAX,
            },
            now,
        );
        assert!(activated.is_empty());
        assert!(core.handle(permit(1, 1, 1), now).is_empty());
        let state = core.backend.0.clone();
        state
            .lock()
            .unwrap()
            .pending_results
            .push_back(Message::CaptureHealth {
                state: CaptureState::Active,
                error_code: 0,
            });
        let verified = core.tick(now);
        assert!(verified.iter().any(|event| matches!(
            event,
            AgentEvent::Control(Message::DesktopTransition {
                desktop: Desktop::Default,
                generation: 1,
                input_enabled: true,
                reason: DesktopTransitionReason::SessionChanged,
            })
        )));
        assert!(core
            .handle(
                Message::RenewLease {
                    lease_id: 1,
                    ttl_ms: u32::MAX,
                },
                now + Duration::from_millis(500),
            )
            .is_empty());
    }

    #[test]
    fn unsolicited_backend_telemetry_is_forwarded_on_the_control_channel() {
        let backend = FakeBackend::default();
        let state = backend.0.clone();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        state
            .lock()
            .unwrap()
            .pending_results
            .push_back(Message::CaptureHealth {
                state: CaptureState::Recovering,
                error_code: 123,
            });

        assert!(matches!(
            core.tick(now).as_slice(),
            [AgentEvent::Control(Message::CaptureHealth {
                state: CaptureState::Recovering,
                error_code: 123,
            })]
        ));
    }

    #[test]
    fn cursor_kind_telemetry_requires_a_live_unsuspended_lease() {
        let backend = FakeBackend::default();
        let state = backend.0.clone();
        let mut core = AgentCore::new(backend);
        let now = Instant::now();
        let kind = Message::CursorKind(crate::input::CursorKind::Text);
        state
            .lock()
            .unwrap()
            .pending_results
            .push_back(kind.clone());
        assert!(
            core.tick(now).is_empty(),
            "no cursor updates without a lease"
        );
        core.observe_desktop(DesktopKind::Default, now);
        core.handle(
            Message::RenewLease {
                lease_id: 1,
                ttl_ms: 2_000,
            },
            now,
        );
        state
            .lock()
            .unwrap()
            .pending_results
            .push_back(kind.clone());
        assert!(core
            .tick(now)
            .iter()
            .any(|event| event == &AgentEvent::Control(kind.clone())));
        let expired = now + LEASE_TTL + Duration::from_millis(1);
        state.lock().unwrap().pending_results.push_back(kind);
        assert!(!core
            .tick(expired)
            .iter()
            .any(|event| matches!(event, AgentEvent::Control(Message::CursorKind(_)))));
    }

    #[test]
    fn cursor_kind_output_backpressure_keeps_only_the_latest_pending_kind() {
        use crate::input::CursorKind;
        let mut outputs = VecDeque::from([
            AgentEvent::Control(Message::CursorKind(CursorKind::Text)),
            AgentEvent::Control(Message::ReleaseAll),
            AgentEvent::Control(Message::CursorKind(CursorKind::Pointer)),
        ]);
        coalesce_pending_cursor_kinds(&mut outputs);
        coalesce_pending_clipboards(&mut outputs);
        assert_eq!(
            outputs,
            VecDeque::from([
                AgentEvent::Control(Message::ReleaseAll),
                AgentEvent::Control(Message::CursorKind(CursorKind::Pointer)),
            ])
        );
    }
}
