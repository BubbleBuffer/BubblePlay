//! Native capture/encode/input backend for the privileged per-session Agent.
//!
//! Every desktop generation owns a fresh worker thread. That thread binds to the current input
//! desktop before constructing USER, D3D, duplication, GDI capture, scaler, encoder, or input
//! objects. The public backend is only a synchronous command proxy plus a bounded result queue.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tracing::{info, warn};
use windows::core::Interface;
use windows::Win32::Foundation::RECT;

use crate::agent_runtime::{AgentBackend, DesktopBinding};
use crate::capture::{
    enumerate_monitors, texture_ptr, Capture, CaptureAvailability, CapturedFrame,
    MonitorInfo as CaptureMonitor, SecureCapture,
};
use crate::desktop_session::{BoundInputDesktop, DesktopKind, InputDesktop};
use crate::encode::{
    AqMode, EncodedFrame, EncoderOptions, Multipass, Nvenc, Preset, Tuning, VideoCodec,
};
use crate::input::{CursorTracker, DesktopRegion, InputApplication, InputSink};
use crate::privileged_ipc::{
    CaptureState, Codec, CropRect, EncodedAccessUnit, EncoderAqMode, EncoderMultipass,
    EncoderPreset, EncoderTuning, InputStatistics, MediaConfig, Message, MonitorInfo,
    MonitorSwitchResult, NoFrameReason, PermitIdentity, StreamMode,
};
use crate::quality::Profile;
use crate::scale::Scaler;

const WORKER_QUEUE_CAPACITY: usize = 8;
const CAPTURE_TIMEOUT: Duration = Duration::from_millis(4);
const MEDIA_IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const MEDIA_IDLE_RELEASE_AFTER: Duration = Duration::from_secs(5);
const SECURE_MAX_FPS: u32 = 15;
const SECURE_MAX_BITRATE_BPS: u32 = 3_000_000;
const CAPTURE_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CaptureRecovery {
    state: CaptureState,
    next_retry_at: Instant,
    backoff_step: usize,
}

impl CaptureRecovery {
    fn new(resource_ready: bool, now: Instant) -> Self {
        Self {
            state: CaptureState::Unavailable,
            next_retry_at: if resource_ready {
                now
            } else {
                now + CAPTURE_RETRY_DELAYS[0]
            },
            backoff_step: usize::from(!resource_ready),
        }
    }

    fn state(self) -> CaptureState {
        self.state
    }

    fn begin_attempt(&mut self, now: Instant) -> Option<CaptureState> {
        if self.state != CaptureState::Unavailable || now < self.next_retry_at {
            return None;
        }
        self.state = CaptureState::Recovering;
        Some(CaptureState::Recovering)
    }

    fn can_attempt(self, now: Instant) -> bool {
        self.state != CaptureState::Unavailable || now >= self.next_retry_at
    }

    fn failed(&mut self, now: Instant) -> Option<CaptureState> {
        let transition =
            (self.state != CaptureState::Unavailable).then_some(CaptureState::Unavailable);
        self.state = CaptureState::Unavailable;
        let delay = CAPTURE_RETRY_DELAYS[self
            .backoff_step
            .min(CAPTURE_RETRY_DELAYS.len().saturating_sub(1))];
        self.backoff_step = self.backoff_step.saturating_add(1);
        self.next_retry_at = now + delay;
        transition
    }

    fn verified(&mut self) -> Option<CaptureState> {
        if self.state == CaptureState::Active {
            return None;
        }
        self.state = CaptureState::Active;
        self.backoff_step = 0;
        Some(CaptureState::Active)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MediaPlan {
    profile: Profile,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
    codec: Codec,
    preset: EncoderPreset,
    tuning: EncoderTuning,
    multipass: EncoderMultipass,
    aq_mode: EncoderAqMode,
    aq_strength: u8,
    vbv_frames: u8,
    lookahead_depth: u8,
    weighted_prediction: bool,
    gop_seconds: u8,
    crop: Option<CropRect>,
    force_idr: bool,
    allow_idle_snapshot: bool,
}

impl MediaPlan {
    fn from_config(config: &MediaConfig, desktop: DesktopKind) -> Result<Self> {
        Self::new(
            config.width,
            config.height,
            config.fps,
            config.bitrate_bps,
            config.codec,
            config.stream_mode,
            EncoderPreset::P3,
            EncoderTuning::UltraLowLatency,
            EncoderMultipass::Disabled,
            EncoderAqMode::Spatial,
            8,
            1,
            0,
            false,
            2,
            None,
            false,
            true,
            desktop,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        width: u16,
        height: u16,
        fps: u16,
        bitrate_bps: u32,
        codec: Codec,
        stream_mode: StreamMode,
        preset: EncoderPreset,
        tuning: EncoderTuning,
        multipass: EncoderMultipass,
        aq_mode: EncoderAqMode,
        aq_strength: u8,
        vbv_frames: u8,
        lookahead_depth: u8,
        weighted_prediction: bool,
        gop_seconds: u8,
        crop: Option<CropRect>,
        force_idr: bool,
        allow_idle_snapshot: bool,
        desktop: DesktopKind,
    ) -> Result<Self> {
        if width == 0 || height == 0 || fps == 0 || bitrate_bps == 0 {
            bail!("media configuration dimensions, fps, and bitrate must be non-zero");
        }
        if !(1..=15).contains(&aq_strength) || !(1..=4).contains(&vbv_frames) {
            bail!("encoder AQ strength or VBV depth is outside the privileged policy");
        }
        let max_lookahead = match stream_mode {
            StreamMode::Work => 2,
            StreamMode::Gaming => 1,
        };
        if lookahead_depth > max_lookahead || !matches!(gop_seconds, 1 | 2 | 4) {
            bail!("encoder lookahead or GOP is outside the privileged policy");
        }
        let profile = profile_for_size(width as u32, height as u32);
        let (width, height, _) = profile.dimensions();
        let (fps, bitrate_bps) = if desktop == DesktopKind::Secure {
            (
                (fps as u32).min(SECURE_MAX_FPS),
                bitrate_bps.min(SECURE_MAX_BITRATE_BPS),
            )
        } else {
            ((fps as u32).min(60), bitrate_bps)
        };
        Ok(Self {
            profile,
            width,
            height,
            fps,
            bitrate_bps,
            codec,
            preset,
            tuning,
            multipass,
            aq_mode,
            aq_strength,
            vbv_frames,
            lookahead_depth,
            weighted_prediction,
            gop_seconds,
            crop,
            force_idr,
            allow_idle_snapshot,
        })
    }
}

fn profile_for_size(width: u32, height: u32) -> Profile {
    if width <= 854 && height <= 480 {
        Profile::Mobile
    } else if width <= 960 && height <= 540 {
        Profile::Low
    } else if width <= 1280 && height <= 720 {
        Profile::Balanced
    } else {
        Profile::Full
    }
}

fn crop_rect(crop: Option<CropRect>, source_width: u32, source_height: u32) -> Result<RECT> {
    if source_width < 2 || source_height < 2 {
        bail!("capture source is too small to crop");
    }
    let (x, y, width, height) = match crop {
        None => (0, 0, source_width, source_height),
        Some(CropRect::Pixels {
            x,
            y,
            width,
            height,
        }) => (x, y, width, height),
        Some(CropRect::Q16 {
            x,
            y,
            width,
            height,
        }) => {
            let px = |value: u16, extent: u32| -> u32 {
                (value as u64 * extent as u64 / u16::MAX as u64) as u32
            };
            (
                px(x, source_width),
                px(y, source_height),
                px(width, source_width),
                px(height, source_height),
            )
        }
    };
    if width == 0 || height == 0 || x >= source_width || y >= source_height {
        bail!("crop is empty or starts outside the capture source");
    }
    let left = x.min(source_width - 2) & !1;
    let top = y.min(source_height - 2) & !1;
    let right = (left.saturating_add(width).min(source_width)) & !1;
    let bottom = (top.saturating_add(height).min(source_height)) & !1;
    if right <= left || bottom <= top {
        bail!("crop collapsed after bounds and chroma alignment");
    }
    Ok(RECT {
        left: left as i32,
        top: top as i32,
        right: right as i32,
        bottom: bottom as i32,
    })
}

fn secure_input_allowed(packet: &[u8]) -> bool {
    if packet.first() != Some(&11) {
        return true;
    }
    std::str::from_utf8(&packet[1..])
        .ok()
        .is_some_and(|text| text.chars().count() == 1)
}

fn input_is_enabled(suspended: bool, capture_active: bool) -> bool {
    !suspended && capture_active
}

#[derive(Debug, Clone)]
enum WorkerCommand {
    Configure(MediaConfig),
    SetBitrate(u32),
    ForceIdr,
    SelectMonitor(u32),
    Suspend,
    Resume,
    Work(Message),
}

#[derive(Debug, Clone)]
enum InputWorkerCommand {
    Input(Vec<u8>),
    SetRegion(DesktopRegion),
    ListMonitors,
    ReleaseAll,
    Suspend,
    Resume,
    Shutdown,
}

struct InputWorkerRequest {
    command: InputWorkerCommand,
    reply: SyncSender<Result<Vec<Message>>>,
}

struct InputWorkerHandle {
    commands: SyncSender<InputWorkerRequest>,
    join: Option<JoinHandle<()>>,
}

impl InputWorkerHandle {
    fn spawn(
        expected: DesktopKind,
        generation: u32,
        region: DesktopRegion,
        capture_active: Arc<AtomicBool>,
    ) -> Result<Self> {
        let (commands, command_rx) = mpsc::sync_channel(WORKER_QUEUE_CAPACITY);
        let (startup_tx, startup_rx) = mpsc::sync_channel(0);
        let join = thread::Builder::new()
            .name(format!("bubbleplay-input-{generation}"))
            .spawn(move || {
                input_worker_main(
                    expected,
                    generation,
                    region,
                    capture_active,
                    command_rx,
                    startup_tx,
                )
            })
            .context("spawn desktop-bound input worker")?;
        match startup_rx
            .recv()
            .context("input worker exited before reporting readiness")?
        {
            Ok(()) => Ok(Self {
                commands,
                join: Some(join),
            }),
            Err(error) => {
                let _ = join.join();
                Err(error)
            }
        }
    }

    fn call(&self, command: InputWorkerCommand) -> Result<Vec<Message>> {
        let (reply, response) = mpsc::sync_channel(0);
        self.commands
            .send(InputWorkerRequest { command, reply })
            .context("input worker command channel closed")?;
        response
            .recv()
            .context("input worker response channel closed")?
    }

    fn stop(mut self) {
        let _ = self.call(InputWorkerCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct WorkerRequest {
    command: WorkerCommand,
    reply: SyncSender<Result<Vec<Message>>>,
}

struct WorkerHandle {
    commands: SyncSender<WorkerRequest>,
    join: Option<JoinHandle<()>>,
    cancelled: Arc<AtomicBool>,
    desktop: DesktopKind,
}

impl WorkerHandle {
    fn spawn(
        expected: DesktopKind,
        generation: u32,
        capture_active: Arc<AtomicBool>,
    ) -> Result<(Self, DesktopBinding, DesktopRegion)> {
        let (commands, command_rx) = mpsc::sync_channel(WORKER_QUEUE_CAPACITY);
        let (startup_tx, startup_rx) = mpsc::sync_channel(0);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let join = thread::Builder::new()
            .name(format!("bubbleplay-desktop-{generation}"))
            .spawn(move || {
                worker_main(
                    expected,
                    generation,
                    command_rx,
                    startup_tx,
                    worker_cancelled,
                    capture_active,
                )
            })
            .context("spawn desktop-bound Agent worker")?;
        let (binding, region) = match startup_rx
            .recv()
            .context("desktop worker exited before reporting readiness")?
        {
            Ok(binding) => binding,
            Err(error) => {
                let _ = join.join();
                return Err(error);
            }
        };
        Ok((
            Self {
                commands,
                join: Some(join),
                cancelled,
                desktop: expected,
            },
            binding,
            region,
        ))
    }

    fn call(&self, command: WorkerCommand) -> Result<Vec<Message>> {
        let (reply, response) = mpsc::sync_channel(0);
        self.commands
            .send(WorkerRequest { command, reply })
            .context("desktop worker command channel closed")?;
        response
            .recv()
            .context("desktop worker response channel closed")?
    }

    fn try_dispatch(&self, command: WorkerCommand) -> Result<Receiver<Result<Vec<Message>>>> {
        let (reply, response) = mpsc::sync_channel(1);
        self.commands
            .try_send(WorkerRequest { command, reply })
            .map_err(|error| {
                anyhow::anyhow!("desktop worker command queue unavailable: {error}")
            })?;
        Ok(response)
    }

    fn retire(mut self) {
        // A stuck GPU driver call must not freeze desktop tracking, lease expiry,
        // or the service watchdog. The cancellation flag prevents any queued
        // input from running if the media call eventually returns. Dropping a
        // JoinHandle detaches the wedged generation; the Broker can still
        // replace the Agent process if the driver never unwinds.
        self.cancelled.store(true, Ordering::Release);
        self.join.take();
    }
}

pub struct RealAgentBackend {
    worker: Option<WorkerHandle>,
    input_worker: Option<InputWorkerHandle>,
    capture_active: Arc<AtomicBool>,
    config: Option<MediaConfig>,
    selected_monitor: Option<u32>,
    suspended: bool,
    pending: VecDeque<Message>,
    pending_work: Option<Receiver<Result<Vec<Message>>>>,
    pending_aux: VecDeque<Receiver<Result<Vec<Message>>>>,
}

impl Default for RealAgentBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl RealAgentBackend {
    pub fn new() -> Self {
        Self {
            worker: None,
            input_worker: None,
            capture_active: Arc::new(AtomicBool::new(false)),
            config: None,
            selected_monitor: None,
            suspended: true,
            pending: VecDeque::new(),
            pending_work: None,
            pending_aux: VecDeque::new(),
        }
    }

    fn call(&self, command: WorkerCommand) -> Result<Vec<Message>> {
        self.worker
            .as_ref()
            .context("desktop worker is unavailable")?
            .call(command)
    }

    fn dispatch_aux(&mut self, command: WorkerCommand) -> Result<()> {
        let response = self
            .worker
            .as_ref()
            .context("desktop worker is unavailable")?
            .try_dispatch(command)?;
        self.pending_aux.push_back(response);
        Ok(())
    }

    fn stop_worker(&mut self) {
        self.capture_active.store(false, Ordering::Release);
        self.pending_work = None;
        self.pending_aux.clear();
        if let Some(worker) = self.worker.take() {
            worker.retire();
        }
        if let Some(worker) = self.input_worker.take() {
            worker.stop();
        }
    }
}

impl Drop for RealAgentBackend {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl AgentBackend for RealAgentBackend {
    fn configure(&mut self, config: &MediaConfig) -> Result<()> {
        let desktop = self
            .worker
            .as_ref()
            .map(|worker| worker.desktop)
            .unwrap_or(DesktopKind::Default);
        MediaPlan::from_config(config, desktop)?;
        if self.worker.is_some() {
            if self.pending_work.is_some() {
                bail!("cannot synchronously configure a busy desktop worker");
            }
            self.call(WorkerCommand::Configure(config.clone()))?;
        }
        self.config = Some(config.clone());
        Ok(())
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        if bitrate_bps == 0 {
            bail!("bitrate must be non-zero");
        }
        if let Some(config) = self.config.as_mut() {
            config.bitrate_bps = bitrate_bps;
        }
        self.dispatch_aux(WorkerCommand::SetBitrate(bitrate_bps))
    }

    fn force_idr(&mut self, _generation: u32) -> Result<()> {
        self.dispatch_aux(WorkerCommand::ForceIdr)
    }

    fn input(&mut self, packet: &[u8]) -> Result<()> {
        let events = self
            .input_worker
            .as_ref()
            .context("desktop input worker is unavailable")?
            .call(InputWorkerCommand::Input(packet.to_vec()))?;
        self.pending.extend(events);
        Ok(())
    }

    fn select_monitor(&mut self, monitor_id: u32) -> Result<()> {
        if self.pending_work.is_some() {
            bail!("cannot synchronously switch a busy desktop worker");
        }
        let events = self.call(WorkerCommand::SelectMonitor(monitor_id))?;
        if events
            .iter()
            .any(|event| matches!(event, Message::MonitorSwitchResult(result) if result.accepted))
        {
            self.selected_monitor = Some(monitor_id);
            if events.iter().any(
                |event| matches!(event, Message::MonitorSwitchResult(result) if result.accepted),
            ) {
                self.input_worker
                    .as_ref()
                    .context("desktop input worker is unavailable")?
                    .call(InputWorkerCommand::SetRegion(monitor_region_by_id(
                        monitor_id,
                    )?))?;
            }
        }
        self.pending.extend(events);
        Ok(())
    }

    fn list_monitors(&mut self) -> Result<Option<Message>> {
        Ok(self
            .input_worker
            .as_ref()
            .context("desktop input worker is unavailable")?
            .call(InputWorkerCommand::ListMonitors)?
            .into_iter()
            .next())
    }

    fn release_all(&mut self) -> Result<()> {
        if let Some(worker) = self.input_worker.as_ref() {
            self.pending
                .extend(worker.call(InputWorkerCommand::ReleaseAll)?);
        }
        Ok(())
    }

    fn suspend(&mut self) -> Result<()> {
        self.suspended = true;
        // Fail closed synchronously; this atomic is observed by the
        // independent input worker even when capture/NVENC is wedged.
        self.capture_active.store(false, Ordering::Release);
        if let Some(worker) = self.input_worker.as_ref() {
            self.pending
                .extend(worker.call(InputWorkerCommand::Suspend)?);
        }
        if self.worker.is_some() {
            self.dispatch_aux(WorkerCommand::Suspend)?;
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        self.input_worker
            .as_ref()
            .context("desktop input worker is unavailable")?
            .call(InputWorkerCommand::Resume)?;
        self.dispatch_aux(WorkerCommand::Resume)?;
        self.suspended = false;
        Ok(())
    }

    fn transition_desktop(
        &mut self,
        desktop: DesktopKind,
        generation: u32,
    ) -> Result<DesktopBinding> {
        self.stop_worker();
        self.pending.clear();
        self.capture_active = Arc::new(AtomicBool::new(false));
        let (worker, media_binding, region) =
            WorkerHandle::spawn(desktop, generation, Arc::clone(&self.capture_active))?;
        let input_worker = match InputWorkerHandle::spawn(
            desktop,
            generation,
            region,
            Arc::clone(&self.capture_active),
        ) {
            Ok(worker) => worker,
            Err(error) => {
                worker.retire();
                return Err(error);
            }
        };
        let binding = DesktopBinding {
            work_ready: media_binding.work_ready,
            input_ready: desktop != DesktopKind::Other,
        };
        let setup = (|| -> Result<Vec<Message>> {
            let mut events = Vec::new();
            if let Some(config) = self.config.clone() {
                worker.call(WorkerCommand::Configure(config))?;
            }
            if desktop == DesktopKind::Default {
                if let Some(monitor_id) = self.selected_monitor {
                    events.extend(worker.call(WorkerCommand::SelectMonitor(monitor_id))?);
                    input_worker.call(InputWorkerCommand::SetRegion(monitor_region_by_id(
                        monitor_id,
                    )?))?;
                }
            }
            if self.suspended {
                worker.call(WorkerCommand::Suspend)?;
                input_worker.call(InputWorkerCommand::Suspend)?;
            }
            Ok(events)
        })();
        match setup {
            Ok(events) => {
                let announced_unavailable = events.iter().any(|event| {
                    matches!(
                        event,
                        Message::CaptureHealth {
                            state: CaptureState::Unavailable,
                            ..
                        }
                    )
                });
                self.pending.extend(events);
                self.worker = Some(worker);
                self.input_worker = Some(input_worker);
                if !announced_unavailable {
                    self.pending
                        .push_back(capture_health(CaptureState::Unavailable));
                }
            }
            Err(error) => {
                worker.retire();
                input_worker.stop();
                return Err(error);
            }
        }
        Ok(binding)
    }

    fn start_work(&mut self, request: Message) -> Result<()> {
        if self.pending_work.is_some() {
            bail!("desktop worker already has an outstanding media permit");
        }
        self.pending_work = Some(
            self.worker
                .as_ref()
                .context("desktop worker is unavailable")?
                .try_dispatch(WorkerCommand::Work(request))?,
        );
        Ok(())
    }

    fn poll_work(&mut self) -> Result<Option<Message>> {
        if let Some(message) = self.pending.pop_front() {
            return Ok(Some(message));
        }

        let aux_count = self.pending_aux.len();
        for _ in 0..aux_count {
            let response = self.pending_aux.pop_front().expect("aux count is stable");
            match response.try_recv() {
                Ok(Ok(messages)) => self.pending.extend(messages),
                Ok(Err(error)) => {
                    warn!(?error, "desktop worker rejected an auxiliary command");
                }
                Err(mpsc::TryRecvError::Empty) => self.pending_aux.push_back(response),
                Err(mpsc::TryRecvError::Disconnected) => {
                    bail!("desktop worker auxiliary response channel closed")
                }
            }
        }

        if let Some(response) = self.pending_work.take() {
            match response.try_recv() {
                Ok(result) => self.pending.extend(result?),
                Err(mpsc::TryRecvError::Empty) => self.pending_work = Some(response),
                Err(mpsc::TryRecvError::Disconnected) => {
                    bail!("desktop worker media response channel closed")
                }
            }
        }
        Ok(self.pending.pop_front())
    }
}

enum FrameSource {
    Default(Option<Capture>),
    Secure(Option<SecureCapture>),
    Other,
}

impl FrameSource {
    fn new(desktop: DesktopKind) -> Self {
        match desktop {
            DesktopKind::Default => Self::Default(Capture::new(None).ok()),
            DesktopKind::Secure => Self::Secure(SecureCapture::primary().ok()),
            DesktopKind::Other => Self::Other,
        }
    }

    fn resource_ready(&self) -> bool {
        matches!(self, Self::Default(Some(_)) | Self::Secure(Some(_)))
    }

    fn take_availability_transition(&mut self) -> Option<CaptureAvailability> {
        match self {
            Self::Default(Some(capture)) => capture.take_availability_transition(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EncoderKey {
    device: usize,
    codec: Codec,
    preset: EncoderPreset,
    tuning: EncoderTuning,
    multipass: EncoderMultipass,
    aq_mode: EncoderAqMode,
    aq_strength: u8,
    vbv_frames: u8,
    lookahead_depth: u8,
    weighted_prediction: bool,
    gop_seconds: u8,
    width: u32,
    height: u32,
    fps: u32,
}

struct WorkerState {
    desktop: DesktopKind,
    generation: u32,
    source: FrameSource,
    capture_recovery: CaptureRecovery,
    config: Option<MediaConfig>,
    scaler: Option<Scaler>,
    encoder: Option<Nvenc>,
    encoder_key: Option<EncoderKey>,
    capture_active: Arc<AtomicBool>,
    suspended: bool,
    force_idr: bool,
    clock: Instant,
    last_media_work: Instant,
}

fn media_idle_release_due(last_media_work: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_media_work) >= MEDIA_IDLE_RELEASE_AFTER
}

fn worker_main(
    expected: DesktopKind,
    generation: u32,
    commands: Receiver<WorkerRequest>,
    startup: SyncSender<Result<(DesktopBinding, DesktopRegion)>>,
    cancelled: Arc<AtomicBool>,
    capture_active: Arc<AtomicBool>,
) {
    let start = (|| -> Result<(BoundInputDesktop, WorkerState)> {
        let bound = InputDesktop::open()?.bind_thread()?;
        let desktop = bound.kind();
        if desktop != expected {
            bail!("input desktop changed while binding: expected {expected:?}, got {desktop:?}");
        }
        let source = FrameSource::new(desktop);
        let capture_recovery = CaptureRecovery::new(source.resource_ready(), Instant::now());
        Ok((
            bound,
            WorkerState {
                desktop,
                generation,
                source,
                capture_recovery,
                config: None,
                scaler: None,
                encoder: None,
                encoder_key: None,
                capture_active,
                suspended: true,
                force_idr: true,
                clock: Instant::now(),
                last_media_work: Instant::now(),
            },
        ))
    })();
    let (bound_desktop, mut state) = match start {
        Ok((bound_desktop, state)) => {
            let usable = state.desktop != DesktopKind::Other;
            let region =
                source_region(&state.source).unwrap_or(DesktopRegion::new(0, 0, 1920, 1080));
            if startup
                .send(Ok((
                    DesktopBinding {
                        work_ready: usable,
                        input_ready: false,
                    },
                    region,
                )))
                .is_err()
            {
                return;
            }
            (bound_desktop, state)
        }
        Err(error) => {
            let _ = startup.send(Err(error));
            return;
        }
    };

    loop {
        match commands.recv_timeout(MEDIA_IDLE_CHECK_INTERVAL) {
            Ok(request) => {
                if cancelled.load(Ordering::Acquire) {
                    break;
                }
                let result = state.handle(request.command);
                let _ = request.reply.send(result);
                if cancelled.load(Ordering::Acquire) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => state.release_media_if_idle(Instant::now()),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(state);
    drop(bound_desktop);
}

struct InputWorkerState {
    desktop: DesktopKind,
    generation: u32,
    capture_active: Arc<AtomicBool>,
    input: InputSink,
    cursor: CursorTracker,
    suspended: bool,
    delivery: InputDeliveryCounters,
    dropped_inputs: u64,
    released_inputs: u64,
    last_input_sequence: Option<u16>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct InputDeliveryCounters {
    received_packets: u64,
    accepted_packets: u64,
    failed_packets: u64,
    requested_events: u64,
    applied_events: u64,
    last_error_code: Option<i32>,
}

impl InputDeliveryCounters {
    fn observe(&mut self, application: InputApplication) {
        self.received_packets = self.received_packets.saturating_add(1);
        self.requested_events = self
            .requested_events
            .saturating_add(u64::from(application.requested));
        self.applied_events = self
            .applied_events
            .saturating_add(u64::from(application.applied));
        if application.succeeded() {
            self.accepted_packets = self.accepted_packets.saturating_add(1);
        } else if application.requested > 0 {
            self.failed_packets = self.failed_packets.saturating_add(1);
            self.last_error_code = application.last_error_code;
        }
    }
}

fn input_worker_main(
    expected: DesktopKind,
    generation: u32,
    region: DesktopRegion,
    capture_active: Arc<AtomicBool>,
    commands: Receiver<InputWorkerRequest>,
    startup: SyncSender<Result<()>>,
) {
    let start = (|| -> Result<(BoundInputDesktop, InputWorkerState)> {
        let bound = InputDesktop::open()?.bind_thread()?;
        let desktop = bound.kind();
        if desktop != expected {
            bail!("input desktop changed while binding input worker: expected {expected:?}, got {desktop:?}");
        }
        match bound.security_context() {
            Ok(context) => info!(
                generation,
                desktop = ?desktop,
                token_is_local_system = context.token_is_local_system,
                token_session_id = context.token_session_id,
                integrity_rid = context.integrity_rid,
                integrity = context.integrity,
                elevation_type = context.elevation_type,
                ui_access = context.ui_access,
                process_window_station = context.process_window_station,
                thread_desktop = context.thread_desktop,
                winsta_input_access = context.winsta_input_access,
                desktop_input_access = context.desktop_input_access,
                "bound privileged input security context"
            ),
            Err(error) => {
                warn!(generation, desktop = ?desktop, ?error, "failed to inspect privileged input security context")
            }
        }
        let mut input = InputSink::new();
        input.set_absolute_region(region);
        Ok((
            bound,
            InputWorkerState {
                desktop,
                generation,
                capture_active,
                input,
                cursor: CursorTracker::for_region(region, Instant::now()),
                suspended: true,
                delivery: InputDeliveryCounters::default(),
                dropped_inputs: 0,
                released_inputs: 0,
                last_input_sequence: None,
            },
        ))
    })();
    let (bound_desktop, mut state) = match start {
        Ok((bound_desktop, state)) => {
            if startup.send(Ok(())).is_err() {
                return;
            }
            (bound_desktop, state)
        }
        Err(error) => {
            let _ = startup.send(Err(error));
            return;
        }
    };

    while let Ok(request) = commands.recv() {
        let stopping = matches!(request.command, InputWorkerCommand::Shutdown);
        let result = state.handle(request.command);
        let _ = request.reply.send(result);
        if stopping {
            break;
        }
    }
    state.input.release_all();
    drop(state);
    drop(bound_desktop);
}

impl InputWorkerState {
    fn handle(&mut self, command: InputWorkerCommand) -> Result<Vec<Message>> {
        match command {
            InputWorkerCommand::Input(packet) => self.handle_input(&packet),
            InputWorkerCommand::SetRegion(region) => {
                if region.width == 0 || region.height == 0 {
                    bail!("cursor dimensions must be non-zero");
                }
                self.input.set_absolute_region(region);
                self.cursor = CursorTracker::for_region(region, Instant::now());
                Ok(Vec::new())
            }
            InputWorkerCommand::ListMonitors => Ok(vec![monitor_list_for_desktop(self.desktop)?]),
            InputWorkerCommand::ReleaseAll => {
                self.release_all();
                Ok(vec![self.input_stats()])
            }
            InputWorkerCommand::Suspend => {
                self.release_all();
                self.suspended = true;
                Ok(Vec::new())
            }
            InputWorkerCommand::Resume => {
                self.suspended = false;
                Ok(Vec::new())
            }
            InputWorkerCommand::Shutdown => {
                self.release_all();
                self.suspended = true;
                Ok(Vec::new())
            }
        }
    }

    fn handle_input(&mut self, packet: &[u8]) -> Result<Vec<Message>> {
        let capture_active = self.capture_active.load(Ordering::Acquire);
        if !input_is_enabled(self.suspended, capture_active) {
            bail!("input is suspended until capture produces a verified encoded frame");
        }
        if self.desktop == DesktopKind::Secure && !secure_input_allowed(packet) {
            self.dropped_inputs = self.dropped_inputs.saturating_add(1);
            bail!("secure desktop text packets must contain exactly one Unicode scalar");
        }
        let outcome = self.input.handle(packet);
        self.delivery.observe(outcome.application);
        if outcome.application.requested > 0 && !outcome.application.succeeded() {
            self.dropped_inputs = self.dropped_inputs.saturating_add(1);
        }
        self.last_input_sequence = outcome.accepted_sequence.or(self.last_input_sequence);
        let stats = self.input.take_stats();
        self.dropped_inputs = self
            .dropped_inputs
            .saturating_add(stats.stale_motion_dropped);
        let mut events = Vec::new();
        if outcome.cursor_changed || outcome.sync_requested || outcome.feedback_reliable {
            if let Some(cursor) = self
                .cursor
                .sample(outcome.accepted_sequence, Instant::now())
            {
                events.push(Message::Cursor(crate::privileged_ipc::CursorState {
                    acknowledged_sequence: cursor.acknowledged_sequence,
                    x: cursor.x,
                    y: cursor.y,
                    in_bounds: cursor.in_bounds,
                    visible: cursor.visible,
                }));
            }
        }
        if outcome.feedback_reliable || outcome.application.failed_calls > 0 {
            events.push(self.input_stats());
        }
        Ok(events)
    }

    fn release_all(&mut self) {
        self.input.release_all();
        self.released_inputs = self.released_inputs.saturating_add(1);
    }

    fn input_stats(&self) -> Message {
        Message::InputStats(InputStatistics {
            session_generation: self.generation,
            received_packets: self.delivery.received_packets,
            accepted_packets: self.delivery.accepted_packets,
            failed_packets: self.delivery.failed_packets,
            dropped_packets: self.dropped_inputs,
            released_inputs: self.released_inputs,
            requested_events: self.delivery.requested_events,
            applied_events: self.delivery.applied_events,
            last_error_code: self.delivery.last_error_code,
            last_sequence: self.last_input_sequence,
        })
    }
}

impl WorkerState {
    fn handle(&mut self, command: WorkerCommand) -> Result<Vec<Message>> {
        match command {
            WorkerCommand::Configure(config) => {
                let plan = MediaPlan::from_config(&config, self.desktop)?;
                if let Some(encoder) = self.encoder.as_mut() {
                    encoder.set_bitrate(plan.bitrate_bps)?;
                }
                self.config = Some(config);
                Ok(Vec::new())
            }
            WorkerCommand::SetBitrate(bitrate_bps) => {
                let bitrate_bps = if self.desktop == DesktopKind::Secure {
                    bitrate_bps.min(SECURE_MAX_BITRATE_BPS)
                } else {
                    bitrate_bps
                };
                if let Some(config) = self.config.as_mut() {
                    config.bitrate_bps = bitrate_bps;
                }
                if let Some(encoder) = self.encoder.as_mut() {
                    encoder.set_bitrate(bitrate_bps)?;
                }
                Ok(Vec::new())
            }
            WorkerCommand::ForceIdr => {
                self.force_idr = true;
                if let Some(encoder) = self.encoder.as_mut() {
                    encoder.force_idr();
                }
                if let FrameSource::Default(Some(capture)) = &mut self.source {
                    capture.request_idle_snapshot();
                }
                Ok(Vec::new())
            }
            WorkerCommand::SelectMonitor(monitor_id) => self.select_monitor(monitor_id),
            WorkerCommand::Suspend => {
                self.suspended = true;
                self.capture_active.store(false, Ordering::Release);
                self.capture_recovery =
                    CaptureRecovery::new(self.source.resource_ready(), Instant::now());
                Ok(Vec::new())
            }
            WorkerCommand::Resume => {
                self.suspended = false;
                Ok(Vec::new())
            }
            WorkerCommand::Work(request) => {
                self.last_media_work = Instant::now();
                self.handle_work(request)
            }
        }
    }

    fn handle_work(&mut self, request: Message) -> Result<Vec<Message>> {
        let identity = request
            .permit_identity()
            .context("worker received a command without permit identity")?;
        let now = Instant::now();
        if self.suspended || self.desktop == DesktopKind::Other {
            return Ok(vec![no_frame(identity, NoFrameReason::Backpressured)]);
        }
        if !self.capture_recovery.can_attempt(now) {
            return Ok(vec![no_frame(identity, NoFrameReason::DesktopUnavailable)]);
        }

        let mut events = Vec::with_capacity(3);
        if let Some(state) = self.capture_recovery.begin_attempt(now) {
            events.push(capture_health(state));
        }

        let result = self.process_work(request);
        let capture_lost =
            self.source.take_availability_transition() == Some(CaptureAvailability::Unavailable);
        match result {
            Ok(Message::EncodedAccessUnit(access_unit)) => {
                self.capture_active.store(true, Ordering::Release);
                if let Some(state) = self.capture_recovery.verified() {
                    events.push(capture_health(state));
                }
                events.push(Message::EncodedAccessUnit(access_unit));
            }
            Ok(message) if capture_lost => {
                self.capture_active.store(false, Ordering::Release);
                if let Some(state) = self.capture_recovery.failed(now) {
                    events.push(capture_health(state));
                }
                events.push(no_frame(identity, NoFrameReason::DesktopUnavailable));
                let _ = message;
            }
            Ok(message) => events.push(message),
            Err(error) => {
                self.capture_active.store(false, Ordering::Release);
                warn!(
                    ?error,
                    ?identity,
                    "privileged capture/encode attempt failed"
                );
                self.invalidate_media_resources();
                if let Some(state) = self.capture_recovery.failed(now) {
                    events.push(capture_health(state));
                }
                events.push(no_frame(identity, NoFrameReason::DesktopUnavailable));
            }
        }
        Ok(events)
    }

    fn process_work(&mut self, request: Message) -> Result<Message> {
        let identity = request
            .permit_identity()
            .context("worker received a command without permit identity")?;
        let plan = match request {
            Message::EncodePermit {
                width,
                height,
                fps,
                bitrate_bps,
                codec,
                stream_mode,
                preset,
                tuning,
                multipass,
                aq_mode,
                aq_strength,
                vbv_frames,
                lookahead_depth,
                weighted_prediction,
                gop_seconds,
                crop,
                force_idr,
                allow_idle_snapshot,
                ..
            } => MediaPlan::new(
                width,
                height,
                fps,
                bitrate_bps,
                codec,
                stream_mode,
                preset,
                tuning,
                multipass,
                aq_mode,
                aq_strength,
                vbv_frames,
                lookahead_depth,
                weighted_prediction,
                gop_seconds,
                crop,
                force_idr,
                allow_idle_snapshot,
                self.desktop,
            )?,
            Message::RequestIdleSnapshot { .. } => {
                let config = self
                    .config
                    .as_ref()
                    .context("idle snapshot requested before media configuration")?;
                let mut plan = MediaPlan::from_config(config, self.desktop)?;
                plan.force_idr = true;
                plan.allow_idle_snapshot = true;
                plan
            }
            _ => bail!("worker received a non-work command on its work path"),
        };
        let mut source = std::mem::replace(&mut self.source, FrameSource::Other);
        let result = self.process_source(&mut source, identity, plan);
        self.source = source;
        match result {
            Ok(message) => Ok(message),
            Err(error) => Err(error),
        }
    }

    fn invalidate_media_resources(&mut self) {
        self.capture_active.store(false, Ordering::Release);
        self.source = match self.desktop {
            DesktopKind::Default => FrameSource::Default(None),
            DesktopKind::Secure => FrameSource::Secure(None),
            DesktopKind::Other => FrameSource::Other,
        };
        self.scaler = None;
        self.encoder = None;
        self.encoder_key = None;
        self.force_idr = true;
    }

    fn release_media_if_idle(&mut self, now: Instant) {
        if !media_idle_release_due(self.last_media_work, now)
            || (!self.source.resource_ready() && self.scaler.is_none() && self.encoder.is_none())
        {
            return;
        }
        self.invalidate_media_resources();
        // This is an intentional cold start, not a failed capture recovery;
        // the next real permit may recreate resources immediately.
        self.capture_recovery = CaptureRecovery::new(true, now);
    }

    fn process_source(
        &mut self,
        source: &mut FrameSource,
        identity: PermitIdentity,
        plan: MediaPlan,
    ) -> Result<Message> {
        match source {
            FrameSource::Default(capture) => {
                if capture.is_none() {
                    *capture = Some(Capture::new(None)?);
                }
                let capture = capture.as_mut().expect("default capture initialized");
                if plan.force_idr || self.force_idr {
                    capture.request_idle_snapshot();
                }
                let captured = match capture.acquire(CAPTURE_TIMEOUT)? {
                    Some(frame) => Some(frame),
                    None if plan.allow_idle_snapshot => capture
                        .take_idle_snapshot_frame()
                        .map(|frame| (frame, true, false)),
                    None => None,
                };
                let Some((frame, is_new, _recovered)) = captured else {
                    return Ok(no_frame(identity, NoFrameReason::Timeout));
                };
                if !is_new && !plan.force_idr && !self.force_idr {
                    return Ok(no_frame(identity, NoFrameReason::NoChange));
                }
                let idle = frame.is_idle_snapshot();
                let encoded = self.encode_frame(&frame, plan)?;
                drop(frame);
                if idle && encoded.is_idr {
                    capture.acknowledge_idle_snapshot_idr();
                }
                Ok(self.encoded_message(identity, plan, encoded))
            }
            FrameSource::Secure(capture) => {
                if capture.is_none() {
                    *capture = Some(SecureCapture::primary()?);
                }
                let frame = capture
                    .as_ref()
                    .expect("secure capture initialized")
                    .capture()?;
                let encoded = self.encode_frame(&frame, plan)?;
                Ok(self.encoded_message(identity, plan, encoded))
            }
            FrameSource::Other => Ok(no_frame(identity, NoFrameReason::DesktopUnavailable)),
        }
    }

    fn encode_frame(&mut self, frame: &CapturedFrame<'_>, plan: MediaPlan) -> Result<EncodedFrame> {
        let source_rect = crop_rect(plan.crop, frame.width(), frame.height())?;
        let full_source = source_rect.left == 0
            && source_rect.top == 0
            && source_rect.right == frame.width() as i32
            && source_rect.bottom == frame.height() as i32;
        let needs_scale =
            !full_source || frame.width() != plan.width || frame.height() != plan.height;
        let texture = if needs_scale {
            let stale = self
                .scaler
                .as_ref()
                .is_none_or(|scaler| !scaler.is_for_device(frame.device()));
            if stale {
                self.scaler = Some(
                    Scaler::new(frame.device(), frame.width(), frame.height())
                        .context("create privileged GPU scaler")?,
                );
            }
            self.scaler
                .as_mut()
                .expect("scaler initialized")
                .scale_with_source(frame.texture(), plan.profile, source_rect)
                .context("scale privileged capture texture")?
                .texture
        } else {
            texture_ptr(frame)
        };

        let device = frame.device().as_raw() as usize;
        let key = EncoderKey {
            device,
            codec: plan.codec,
            preset: plan.preset,
            tuning: plan.tuning,
            multipass: plan.multipass,
            aq_mode: plan.aq_mode,
            aq_strength: plan.aq_strength,
            vbv_frames: plan.vbv_frames,
            lookahead_depth: plan.lookahead_depth,
            weighted_prediction: plan.weighted_prediction,
            gop_seconds: plan.gop_seconds,
            width: plan.width,
            height: plan.height,
            fps: plan.fps,
        };
        let incompatible = self.encoder_key.is_none_or(|current| {
            current.device != key.device
                || current.codec != key.codec
                || current.preset != key.preset
                || current.tuning != key.tuning
                || current.multipass != key.multipass
                || current.aq_mode != key.aq_mode
                || current.aq_strength != key.aq_strength
                || current.vbv_frames != key.vbv_frames
                || current.lookahead_depth != key.lookahead_depth
                || current.weighted_prediction != key.weighted_prediction
                || current.gop_seconds != key.gop_seconds
        });
        if incompatible {
            self.encoder = Some(Nvenc::new_with_options_native_codec(
                device as *mut core::ffi::c_void,
                plan.width,
                plan.height,
                frame.width().max(plan.width),
                frame.height().max(plan.height),
                plan.fps,
                plan.bitrate_bps,
                encoder_options(plan),
                encode_codec(plan.codec),
            )?);
        } else if self.encoder_key != Some(key) {
            self.encoder
                .as_mut()
                .context("encoder missing for profile reconfiguration")?
                .reconfigure_profile(plan.width, plan.height, plan.fps)?;
        }
        let encoder = self.encoder.as_mut().context("encoder unavailable")?;
        encoder.set_bitrate(plan.bitrate_bps)?;
        if plan.force_idr || self.force_idr {
            encoder.force_idr();
        }
        self.force_idr = false;
        self.encoder_key = Some(key);
        encoder.encode_texture(texture)
    }

    fn encoded_message(
        &self,
        identity: PermitIdentity,
        plan: MediaPlan,
        encoded: EncodedFrame,
    ) -> Message {
        Message::EncodedAccessUnit(EncodedAccessUnit {
            permit_id: identity.permit_id,
            session_generation: identity.session_generation,
            config_revision: identity.config_revision,
            timestamp_90khz: (self.clock.elapsed().as_secs_f64() * 90_000.0) as u64,
            keyframe: encoded.is_idr,
            codec: plan.codec,
            width: plan.width as u16,
            height: plan.height as u16,
            fps: plan.fps as u16,
            encode_us: encoded.encode_us.min(u32::MAX as u64) as u32,
            data: encoded.data,
        })
    }

    fn select_monitor(&mut self, monitor_id: u32) -> Result<Vec<Message>> {
        let mut result = MonitorSwitchResult {
            session_generation: self.generation,
            monitor_id,
            accepted: false,
            width: 0,
            height: 0,
            error_code: -1,
        };
        if self.desktop != DesktopKind::Default || monitor_id == 0 {
            return Ok(vec![Message::MonitorSwitchResult(result)]);
        }
        let monitors = enumerate_monitors(None)?;
        let Some(monitor) = monitors.get(monitor_id.saturating_sub(1) as usize) else {
            return Ok(vec![Message::MonitorSwitchResult(result)]);
        };
        let capture = Capture::new(Some(&monitor.selector))?;
        result.accepted = true;
        result.width = capture.width().min(u16::MAX as u32) as u16;
        result.height = capture.height().min(u16::MAX as u32) as u16;
        result.error_code = 0;
        self.source = FrameSource::Default(Some(capture));
        self.capture_active.store(false, Ordering::Release);
        self.capture_recovery = CaptureRecovery::new(true, Instant::now());
        self.scaler = None;
        self.encoder = None;
        self.encoder_key = None;
        self.force_idr = true;
        Ok(vec![
            Message::MonitorSwitchResult(result),
            capture_health(CaptureState::Unavailable),
        ])
    }
}

fn source_dimensions(source: &FrameSource) -> Option<(u32, u32)> {
    match source {
        FrameSource::Default(Some(capture)) => Some((capture.width(), capture.height())),
        FrameSource::Secure(Some(capture)) => {
            let frame = capture.capture().ok()?;
            Some((frame.width(), frame.height()))
        }
        _ => None,
    }
}

fn source_region(source: &FrameSource) -> Option<DesktopRegion> {
    match source {
        FrameSource::Default(Some(capture)) => Some(capture.region()),
        FrameSource::Secure(Some(_capture)) => {
            source_dimensions(source).map(|(width, height)| DesktopRegion::new(0, 0, width, height))
        }
        _ => None,
    }
}

fn monitor_region_by_id(monitor_id: u32) -> Result<DesktopRegion> {
    let monitors = enumerate_monitors(None)?;
    let monitor = monitors
        .get(monitor_id.saturating_sub(1) as usize)
        .context("selected monitor disappeared before input remap")?;
    Ok(DesktopRegion::new(
        monitor.origin_x,
        monitor.origin_y,
        monitor.width,
        monitor.height,
    ))
}

fn monitor_list_for_desktop(desktop: DesktopKind) -> Result<Message> {
    monitor_list_for_desktop_with(desktop, enumerate_monitors)
}

fn monitor_list_for_desktop_with<F>(desktop: DesktopKind, mut enumerate: F) -> Result<Message>
where
    F: FnMut(Option<&str>) -> Result<Vec<CaptureMonitor>>,
{
    let monitors = match desktop {
        DesktopKind::Default => enumerate(None)?,
        DesktopKind::Secure => enumerate(Some("primary"))?,
        DesktopKind::Other => Vec::new(),
    };
    Ok(Message::MonitorList(map_monitors(monitors)))
}

fn map_monitors(monitors: Vec<CaptureMonitor>) -> Vec<MonitorInfo> {
    monitors
        .into_iter()
        .enumerate()
        .map(|(index, monitor)| MonitorInfo {
            id: index as u32 + 1,
            width: monitor.width.min(u16::MAX as u32) as u16,
            height: monitor.height.min(u16::MAX as u32) as u16,
            primary: monitor.selected,
            name: monitor.name,
        })
        .collect()
}

fn encode_codec(codec: Codec) -> VideoCodec {
    match codec {
        Codec::H264 => VideoCodec::H264,
        Codec::Hevc => VideoCodec::Hevc,
    }
}

fn encode_preset(preset: EncoderPreset) -> Preset {
    match preset {
        EncoderPreset::P1 => Preset::P1,
        EncoderPreset::P2 => Preset::P2,
        EncoderPreset::P3 => Preset::P3,
        EncoderPreset::P4 => Preset::P4,
        EncoderPreset::P5 => Preset::P5,
        EncoderPreset::P6 => Preset::P6,
        EncoderPreset::P7 => Preset::P7,
    }
}

fn encoder_options(plan: MediaPlan) -> EncoderOptions {
    EncoderOptions {
        preset: encode_preset(plan.preset),
        tuning: match plan.tuning {
            EncoderTuning::UltraLowLatency => Tuning::UltraLowLatency,
            EncoderTuning::LowLatency => Tuning::LowLatency,
        },
        rate_control: crate::encode::RateControlMode::Cbr,
        multipass: match plan.multipass {
            EncoderMultipass::Disabled => Multipass::Disabled,
            EncoderMultipass::Quarter => Multipass::Quarter,
            EncoderMultipass::Full => Multipass::Full,
        },
        aq_mode: match plan.aq_mode {
            EncoderAqMode::None => AqMode::None,
            EncoderAqMode::Spatial => AqMode::Spatial,
            EncoderAqMode::Temporal => AqMode::Temporal,
        },
        aq_strength: plan.aq_strength,
        vbv_frames: plan.vbv_frames,
        lookahead_depth: plan.lookahead_depth,
        weighted_prediction: plan.weighted_prediction,
        gop_seconds: plan.gop_seconds,
        roi_mode: crate::encode::RoiMode::Off,
        resource_mode: crate::encode::ResourceMode::PerFrame,
        async_depth: 0,
        main10: false,
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

fn capture_health(state: CaptureState) -> Message {
    Message::CaptureHealth {
        state,
        error_code: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_windows_application_is_received_but_never_accepted() {
        let mut counters = InputDeliveryCounters::default();
        counters.observe(crate::input::InputApplication {
            requested: 1,
            applied: 0,
            failed_calls: 1,
            last_error_code: Some(5),
        });

        assert_eq!(counters.received_packets, 1);
        assert_eq!(counters.accepted_packets, 0);
        assert_eq!(counters.failed_packets, 1);
        assert_eq!(counters.requested_events, 1);
        assert_eq!(counters.applied_events, 0);
        assert_eq!(counters.last_error_code, Some(5));
    }

    #[test]
    fn privileged_media_resources_expire_after_five_idle_seconds() {
        let start = Instant::now();
        assert!(!media_idle_release_due(
            start,
            start + MEDIA_IDLE_RELEASE_AFTER - Duration::from_millis(1)
        ));
        assert!(media_idle_release_due(
            start,
            start + MEDIA_IDLE_RELEASE_AFTER
        ));
    }

    #[test]
    fn standard_profile_mapping_is_monotonic_and_secure_policy_is_capped() {
        assert_eq!(profile_for_size(854, 480), Profile::Mobile);
        assert_eq!(profile_for_size(900, 500), Profile::Low);
        assert_eq!(profile_for_size(1280, 720), Profile::Balanced);
        assert_eq!(profile_for_size(1600, 900), Profile::Full);

        let secure = MediaPlan::new(
            1920,
            1080,
            60,
            8_000_000,
            Codec::Hevc,
            StreamMode::Gaming,
            EncoderPreset::P3,
            EncoderTuning::UltraLowLatency,
            EncoderMultipass::Disabled,
            EncoderAqMode::Spatial,
            8,
            1,
            0,
            false,
            2,
            None,
            false,
            true,
            DesktopKind::Secure,
        )
        .unwrap();
        assert_eq!((secure.fps, secure.bitrate_bps), (15, 3_000_000));
    }

    #[test]
    fn privileged_policy_rejects_gaming_lookahead_above_one() {
        let result = MediaPlan::new(
            1920,
            1080,
            60,
            8_000_000,
            Codec::Hevc,
            StreamMode::Gaming,
            EncoderPreset::P7,
            EncoderTuning::LowLatency,
            EncoderMultipass::Full,
            EncoderAqMode::Temporal,
            15,
            4,
            2,
            true,
            4,
            None,
            false,
            true,
            DesktopKind::Default,
        );
        assert!(result.is_err());
    }

    #[test]
    fn crop_mapping_clamps_and_even_aligns_without_leaving_the_source() {
        let crop = crop_rect(
            Some(CropRect::Pixels {
                x: 1919,
                y: 1079,
                width: 500,
                height: 500,
            }),
            1920,
            1080,
        )
        .unwrap();
        assert_eq!((crop.left, crop.top), (1918, 1078));
        assert_eq!((crop.right, crop.bottom), (1920, 1080));
        assert!(crop_rect(
            Some(CropRect::Pixels {
                x: 1920,
                y: 0,
                width: 10,
                height: 10,
            }),
            1920,
            1080,
        )
        .is_err());
    }

    #[test]
    fn secure_text_accepts_one_scalar_and_rejects_bulk_or_invalid_utf8() {
        assert!(secure_input_allowed(&[11, b'a']));
        assert!(secure_input_allowed(&[11, 0xC3, 0xA9]));
        assert!(!secure_input_allowed(&[11, b'a', b'b']));
        assert!(!secure_input_allowed(&[11, 0xFF]));
        assert!(secure_input_allowed(&[1, 0xFF]));
    }

    #[test]
    fn input_gate_requires_both_resume_and_verified_capture() {
        assert!(!input_is_enabled(true, false));
        assert!(!input_is_enabled(true, true));
        assert!(!input_is_enabled(false, false));
        assert!(input_is_enabled(false, true));
    }

    #[test]
    fn monitor_ids_are_stable_one_based_indices_for_a_snapshot() {
        let monitors = map_monitors(vec![CaptureMonitor {
            selector: "DISPLAY1".into(),
            name: "Primary".into(),
            width: 1920,
            height: 1080,
            origin_x: 0,
            origin_y: 0,
            selected: true,
        }]);
        assert_eq!(monitors[0].id, 1);
        assert!(monitors[0].primary);
    }

    #[test]
    fn monitor_discovery_uses_the_desktop_bound_non_gpu_enumerator() {
        let mut selectors = Vec::new();
        let message = monitor_list_for_desktop_with(DesktopKind::Default, |selector| {
            selectors.push(selector.map(str::to_owned));
            Ok(vec![CaptureMonitor {
                selector: "DISPLAY1".into(),
                name: "Primary".into(),
                width: 1920,
                height: 1080,
                origin_x: 0,
                origin_y: 0,
                selected: true,
            }])
        })
        .unwrap();

        assert_eq!(selectors, vec![None]);
        assert!(matches!(
            message,
            Message::MonitorList(monitors) if monitors.len() == 1 && monitors[0].id == 1
        ));

        let mut secure_selector = None;
        monitor_list_for_desktop_with(DesktopKind::Secure, |selector| {
            secure_selector = selector.map(str::to_owned);
            Ok(Vec::new())
        })
        .unwrap();
        assert_eq!(secure_selector.as_deref(), Some("primary"));

        let mut enumerated_other = false;
        monitor_list_for_desktop_with(DesktopKind::Other, |_| {
            enumerated_other = true;
            Ok(Vec::new())
        })
        .unwrap();
        assert!(!enumerated_other);
    }

    #[test]
    fn capture_is_not_active_until_an_encoded_frame_verifies_the_pipeline() {
        let now = Instant::now();
        let mut recovery = CaptureRecovery::new(true, now);
        assert_eq!(recovery.state(), CaptureState::Unavailable);
        assert_eq!(recovery.begin_attempt(now), Some(CaptureState::Recovering));
        assert_eq!(recovery.state(), CaptureState::Recovering);
        assert_eq!(recovery.verified(), Some(CaptureState::Active));
        assert_eq!(recovery.verified(), None);
        assert_eq!(recovery.state(), CaptureState::Active);
    }

    #[test]
    fn capture_retry_backoff_is_250_500_1000_then_2000_ms() {
        let start = Instant::now();
        let mut recovery = CaptureRecovery::new(true, start);
        recovery.begin_attempt(start);

        let expected = [250_u64, 500, 1_000, 2_000, 2_000];
        let mut attempt_at = start;
        for delay_ms in expected {
            assert_eq!(recovery.failed(attempt_at), Some(CaptureState::Unavailable));
            assert!(!recovery.can_attempt(attempt_at + Duration::from_millis(delay_ms - 1)));
            attempt_at += Duration::from_millis(delay_ms);
            assert!(recovery.can_attempt(attempt_at));
            assert_eq!(
                recovery.begin_attempt(attempt_at),
                Some(CaptureState::Recovering)
            );
        }
    }

    #[test]
    fn failed_initial_resource_creation_waits_before_first_retry() {
        let now = Instant::now();
        let mut recovery = CaptureRecovery::new(false, now);
        assert!(!recovery.can_attempt(now + Duration::from_millis(249)));
        assert!(recovery.can_attempt(now + Duration::from_millis(250)));
        assert_eq!(
            recovery.begin_attempt(now + Duration::from_millis(250)),
            Some(CaptureState::Recovering)
        );
        assert_eq!(
            recovery.failed(now + Duration::from_millis(250)),
            Some(CaptureState::Unavailable)
        );
        assert!(!recovery.can_attempt(now + Duration::from_millis(749)));
        assert!(recovery.can_attempt(now + Duration::from_millis(750)));
    }

    #[test]
    fn media_dispatch_does_not_wait_for_a_worker_reply() {
        let (commands, queued) = mpsc::sync_channel(1);
        let worker = WorkerHandle {
            commands,
            join: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            desktop: DesktopKind::Default,
        };

        let response = worker.try_dispatch(WorkerCommand::ForceIdr).unwrap();
        let request = queued.try_recv().expect("command was queued immediately");
        assert!(matches!(request.command, WorkerCommand::ForceIdr));
        assert!(matches!(
            response.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(request);
    }

    #[test]
    fn input_release_channel_progresses_while_media_reply_is_blocked() {
        let (media_commands, queued_media) = mpsc::sync_channel(1);
        let media = WorkerHandle {
            commands: media_commands,
            join: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            desktop: DesktopKind::Default,
        };
        let blocked_response = media.try_dispatch(WorkerCommand::ForceIdr).unwrap();
        let _blocked_request = queued_media
            .try_recv()
            .expect("media work is held without a reply");

        let (input_commands, input_rx) = mpsc::sync_channel::<InputWorkerRequest>(1);
        let input_join = thread::spawn(move || {
            while let Ok(request) = input_rx.recv() {
                let stopping = matches!(request.command, InputWorkerCommand::Shutdown);
                request.reply.send(Ok(Vec::new())).unwrap();
                if stopping {
                    break;
                }
            }
        });
        let input = InputWorkerHandle {
            commands: input_commands,
            join: Some(input_join),
        };
        input.call(InputWorkerCommand::ReleaseAll).unwrap();
        assert!(matches!(
            blocked_response.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        input.stop();
    }
}
