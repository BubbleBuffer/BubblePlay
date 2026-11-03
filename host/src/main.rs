//! BubblePlay host: DXGI capture -> NVENC / Media Foundation -> WebRTC / WSS.
//!
//! Modes:
//! - `spike`: M1 verification. Capture primary monitor, encode with NVENC,
//!   write Annex B to `test.h264`, report encode latency. No network.
//! - `serve` (default): full pipeline. Connect the signaling worker, wait for
//!   a browser offer, answer it, trickle srflx, stream video, apply input.

mod host_capture;
use host_capture::*;
mod host_encode;
use host_encode::*;
mod host_session;
use host_session::*;
mod host_benchmark;
use host_benchmark::*;
mod producer_wait;
mod session_watchdog;
mod viewer_display;
use viewer_display::{DisplayWakePolicy, ViewerDisplayHold};

use bubbleplay_host::activity::Activity;
use bubbleplay_host::benchmark;
use bubbleplay_host::capture;
use bubbleplay_host::encode;
use bubbleplay_host::feedback_roi;
use bubbleplay_host::file_service::{FileService, FileServiceConfig};
use bubbleplay_host::file_transfer::file_rate_cap_bps;
use bubbleplay_host::follow_window::{self, FollowWindow};
use bubbleplay_host::frame_damage;
use bubbleplay_host::hevc_tiles;
use bubbleplay_host::hybrid;
use bubbleplay_host::input;
use bubbleplay_host::quality;
use bubbleplay_host::reconstruction_feedback;
use bubbleplay_host::rtc;
use bubbleplay_host::scale;
use bubbleplay_host::scroll_detect::{extend_to_runs, sample_tiles, ScrollDetector};
use bubbleplay_host::signaling;
use bubbleplay_host::stats_window;
use bubbleplay_host::stun;
use bubbleplay_host::temporal_diff;
use bubbleplay_host::tile_hash::{TileHasher, MAX_HASHED_PER_MILLE};
use bubbleplay_host::tile_quality::{refine_qp_map, RefinePlan, TileQualityTracker};
use bubbleplay_host::tile_schedule::{pointer_pixels, PaceDecision, V4Pacer};
use bubbleplay_host::transport;
use bubbleplay_host::vdd;
use bubbleplay_host::video_encoder::cpu_policy::{CpuDamage, CpuPacing, CpuTarget};
use bubbleplay_host::video_encoder::{
    device_encoder_adapter, encoder_adapter, EncoderBackend, EncoderCapabilities,
    EncoderDiscoveryCache, EncoderOverride, EncoderPreference, VideoEncoder,
};
use bubbleplay_host::wss_media;

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use tracing::{error, info, warn};
use windows::core::Interface;
use windows::Win32::Foundation::{POINT, RECT};

use benchmark::{
    BenchmarkAqMode, BenchmarkHybridCacheMode, BenchmarkHybridMode, BenchmarkMultipass,
    BenchmarkPreset, BenchmarkResourceMode, BenchmarkRoiMode, BenchmarkStreamMode,
    BenchmarkTransport, BenchmarkTuning,
};
use bubbleplay_host::input_diagnostics::InputDropReason;
use capture::{
    encode_texture_png_crop, pick_monitor, texture_ptr, Capture, CaptureAvailability, CapturedFrame,
};
use encode::{
    AqMode, EncodeStageTimings, EncodedFrame, EncoderOptions, FrameDamage, Multipass, Nvenc,
    Preset, RateControlMode, ResourceMode, RoiMapReason, RoiMapTelemetry, RoiMode, Tuning,
    VideoCodec,
};
use feedback_roi::{
    changed_ctb_mask, FeedbackBudgetPolicy, FeedbackPlan, FeedbackPlanReason, FeedbackRoiPlanner,
    FeedbackWireBudget, FeedbackWireDecision,
};
use hevc_tiles::{is_supported_output_rect, HevcTileEncoder};
use hybrid::{
    encode_hybrid_frame_v1, hybrid_operation_wire_bytes, HybridFrame, HybridTileOperation,
    HybridWireBudget, QuiescentTilePlan, QuiescentTilePlanner, StaticTilePlanner, ViewportGeometry,
    MAX_HYBRID_SIDECAR_BYTES,
};
use input::{CursorKindTracker, CursorTracker, DesktopRegion, InputPacketCoalescer, InputSink};
use quality::{Controller, Decision, Profile, Qos, StreamMode};
use reconstruction_feedback::{
    is_exact_previous_feedback, ReconstructionFeedbackAnalyzer, ReconstructionFeedbackSubmission,
};
use rtc::RtcAction;
use scale::{ScaleOutput, Scaler};
use signaling::{
    BenchEncodeStages, BenchHybridActivity, BenchRoiActivity, BenchStageTiming, SignalEvent,
    Signaling,
};
use temporal_diff::{
    DamageCoverageAudit, DeferredReadbackActivity, QuiescentValidationStatus, TemporalDiffAnalyzer,
    TemporalDiffResult, TemporalMaskDecision,
};
use transport::{
    AccessUnit, RtcPeerDecision, RtcPeerLifecycle, TransportCommand, TransportEvent, TransportPump,
};
use wss_media::{
    AccessUnitMeta as WssAccessUnitMeta, MediaCodec, ReserveOutcome as WssReserveOutcome,
    SubmitOutcome as WssSubmitOutcome, WssMediaEgress,
};

const STUN_SERVER: &str = "stun.l.google.com:19302";

/// CPU conversion reads capture's native surface and owns the output size.
/// Use the same source contract for initial preparation and error recovery.
fn cpu_mode_texture(frame: &CapturedFrame<'_>, mode: VideoMode) -> ScaleOutput {
    ScaleOutput {
        texture: texture_ptr(frame),
        scale_us: 0,
        width: mode.width,
        height: mode.height,
    }
}
const DEFAULT_BITRATE_BPS: u32 = 1_500_000;
const MAX_BITRATE_BPS: u32 = 40_000_000;
const IDR_INTERVAL: Duration = Duration::from_secs(2);
const KEYFRAME_REQUEST_COOLDOWN: Duration = Duration::from_secs(2);
const STATIC_REPLAY_DELAY: Duration = Duration::from_millis(250);
const WSS_HIGH_BITRATE_BPS: u32 = 6_000_000;
const WSS_BALANCED_BITRATE_BPS: u32 = 3_000_000;
const WSS_SAVER_BITRATE_BPS: u32 = 1_500_000;
/// Idle heartbeat on the signaling socket (the browser uses the same period).
const SIGNALING_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// v4 refine frames and keyframes are encoded at this multiple of the live
/// bitrate (NVENC grows the one-frame VBV with it), so a keyframe is not
/// starved into a blocky whole-frame dip.
const WSS_V4_REFINE_BITRATE_FACTOR: u32 = 4;

/// v4 has no periodic keyframe: only start, a new peer / reconnect, a
/// transition and an explicit receiver request produce one.
fn periodic_idr_due(now: Instant, next_idr: Instant, v4: bool) -> bool {
    !v4 && now >= next_idr
}

fn format_keyframe_reasons(reasons: &std::collections::BTreeMap<String, u64>) -> String {
    reasons
        .iter()
        .map(|(reason, count)| format!("{reason}:{count}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn wss_v4_refine_bitrate(current_bps: u32) -> u32 {
    current_bps
        .saturating_mul(WSS_V4_REFINE_BITRATE_FACTOR)
        .min(MAX_BITRATE_BPS)
}

#[derive(Debug)]
struct Args {
    mode: String, // "spike" | "serve"
    worker: Option<String>,
    /// The unlocked PC key from the tray. Relay registration requires it.
    device_identity: Option<bubbleplay_host::device_key::DeviceIdentity>,
    token: Option<String>,
    duration_secs: u32,
    bitrate_bps: u32,
    max_bitrate_bps: u32,
    fixed_bitrate_bps: Option<u32>,
    quality: String,
    preset: String,
    out: String,
    monitor: Option<String>,
    vdd: bool,
    audio: bool,
    benchmark_profile: Option<PathBuf>,
    cpu_mode: bool,
    encoder: EncoderOverride,
}

impl Args {
    fn encoder_preference(&self) -> EncoderPreference {
        EncoderPreference {
            cpu_mode: self.cpu_mode,
            encoder: self.encoder,
        }
    }
}

fn parse_args() -> Result<Args> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(mut it: impl Iterator<Item = String>) -> Result<Args> {
    let mut args = Args {
        mode: "serve".to_string(),
        worker: None,
        device_identity: None,
        token: std::env::var("BUBBLEPLAY_TOKEN").ok(),
        duration_secs: 10,
        bitrate_bps: DEFAULT_BITRATE_BPS,
        max_bitrate_bps: 12_000_000,
        fixed_bitrate_bps: None,
        quality: "auto".to_string(),
        preset: "p3".to_string(),
        out: "test.h264".to_string(),
        monitor: None,
        vdd: false,
        audio: true,
        benchmark_profile: None,
        cpu_mode: false,
        encoder: EncoderOverride::Auto,
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" => args.mode = it.next().context("--mode value")?,
            "--worker" => args.worker = Some(it.next().context("--worker value")?),
            "--token" => args.token = Some(it.next().context("--token value")?),
            "--duration" => args.duration_secs = it.next().context("--duration value")?.parse()?,
            "--bitrate" => args.bitrate_bps = it.next().context("--bitrate value")?.parse()?,
            "--max-bitrate" => {
                args.max_bitrate_bps = it.next().context("--max-bitrate value")?.parse()?
            }
            "--fixed-bitrate" => {
                args.fixed_bitrate_bps = Some(it.next().context("--fixed-bitrate value")?.parse()?)
            }
            "--quality" => args.quality = it.next().context("--quality value")?,
            "--preset" => args.preset = it.next().context("--preset value")?,
            "--out" => args.out = it.next().context("--out value")?,
            "--monitor" => args.monitor = Some(it.next().context("--monitor value")?),
            "--vdd" => args.vdd = true,
            "--no-audio" => args.audio = false,
            "--benchmark-profile" => {
                args.benchmark_profile = Some(PathBuf::from(
                    it.next().context("--benchmark-profile value")?,
                ))
            }
            "--cpu-mode" => args.cpu_mode = true,
            // Diagnostic override for the encoder backend; not in --help.
            "--encoder" => args.encoder = it.next().context("--encoder value")?.parse()?,
            "-h" | "--help" => {
                println!("bubbleplay-host --mode spike|serve [--worker wss://...] [--token ...] [--duration secs] [--bitrate bps] [--max-bitrate bps] [--fixed-bitrate bps] [--quality auto|480p24|540p30|720p30|1080p60] [--preset p1|p3] [--out file] [--monitor primary|<index>|<name-substr>] [--vdd] [--no-audio] [--benchmark-profile absolute-path] [--cpu-mode]");
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    if args.bitrate_bps == 0 || args.max_bitrate_bps == 0 {
        bail!("bitrate values must be positive");
    }
    if args.max_bitrate_bps > MAX_BITRATE_BPS {
        bail!("--max-bitrate must be <= {MAX_BITRATE_BPS}");
    }
    if args.max_bitrate_bps < args.bitrate_bps {
        bail!("--max-bitrate must be >= --bitrate");
    }
    if !matches!(
        args.quality.as_str(),
        "auto" | "480p24" | "540p30" | "720p30" | "1080p60"
    ) {
        bail!("--quality must be auto|480p24|540p30|720p30|1080p60");
    }
    if let Some(fixed) = args.fixed_bitrate_bps {
        if fixed == 0 || fixed > args.max_bitrate_bps {
            bail!("--fixed-bitrate must be positive and <= --max-bitrate");
        }
    }
    if args.fixed_bitrate_bps.is_some() && args.quality == "auto" {
        bail!("--fixed-bitrate requires a non-auto --quality");
    }
    if !matches!(args.preset.as_str(), "p1" | "p3") {
        bail!("--preset must be p1|p3");
    }
    if args
        .benchmark_profile
        .as_ref()
        .is_some_and(|path| !path.is_absolute())
    {
        bail!("--benchmark-profile must be an absolute path");
    }
    Ok(args)
}

fn local_recovery_probe_due(available: bool, now: Instant, deadline: Instant) -> bool {
    !available && now >= deadline
}

fn refresh_input_geometry(
    input: &mut InputSink,
    cursor: &mut CursorTracker,
    region: DesktopRegion,
    now: Instant,
) {
    input.set_absolute_region(region);
    *cursor = CursorTracker::for_region(region, now);
}

/// A failed switch still advances the generation and closes presentation.
/// Reannounce the retained source so the viewer can present a fresh frame and
/// complete the same readiness handshake as a successful switch.
fn retained_capture_after_failed_switch(state: DesktopState) -> bool {
    state.available && state.desktop == "default"
}

fn foreground_sample_live(
    viewer_attached: bool,
    available: bool,
    desktop: &str,
    transport_live: bool,
) -> bool {
    viewer_attached && available && desktop == "default" && transport_live
}

fn announce_capture_state(
    signaling: &mut Signaling,
    capture: &mut CaptureSource,
    generation: u32,
    reason: &str,
) {
    let state = capture.desktop_state();
    if let Some(secure) = capture.secure_state() {
        signaling.send_secure_desktop_state_status(
            state.available,
            secure.kind,
            if state.available { &secure.caps } else { &[] },
            generation,
        );
        return;
    }
    if !state.available {
        if let Some(kind) = bubbleplay_host::desktop_session::secure_desktop_kind() {
            signaling.send_secure_desktop_state(kind, &[], generation);
            return;
        }
    }
    if matches!(reason, "wss-session-start" | "offer-state-replay") {
        capture.request_idle_snapshot();
        if capture.defer_recoverable_join_state(Instant::now()) {
            return;
        }
    }
    signaling.send_desktop_state_kind(
        if state.available {
            "active"
        } else {
            "unavailable"
        },
        Some(state.desktop),
        Some(state.input),
        generation,
        reason,
    );
}

fn native_drop_pixel(x: u16, y: u16, region: DesktopRegion) -> POINT {
    let axis = |value: u16, origin: i32, extent: u32| {
        let offset = (u64::from(value) * u64::from(extent.saturating_sub(1))
            + u64::from(u16::MAX) / 2)
            / u64::from(u16::MAX);
        (i64::from(origin) + offset as i64).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    };
    POINT {
        x: axis(x, region.x, region.width),
        y: axis(y, region.y, region.height),
    }
}

struct NativeDropView<'a> {
    ready: bool,
    epoch: Option<&'a str>,
    generation: u32,
    revision: u32,
}
impl NativeDropView<'_> {
    fn accepts(&self, epoch: &str, point: bubbleplay_host::file_transfer::DropPoint) -> bool {
        self.ready
            && self.epoch == Some(epoch)
            && point.generation == self.generation
            && point.viewport_revision == self.revision
    }
}
struct ActiveFileDragPreview {
    drag_id: u32,
    epoch: String,
    point: bubbleplay_host::file_transfer::DropPoint,
    updated: Instant,
    handle: bubbleplay_host::native_file_drop::NativeDropPreview,
}
#[derive(Default)]
struct FileDragPreviewState {
    desired: Option<(signaling::FileDragPreviewControl, Instant)>,
    active: Option<ActiveFileDragPreview>,
    attempted: Option<(u32, String)>,
}
impl FileDragPreviewState {
    fn control(&mut self, control: signaling::FileDragPreviewControl, now: Instant) {
        if !control.valid() {
            return;
        }
        if control.action == "cancel" {
            if self.desired.as_ref().is_some_and(|(current, _)| {
                current.drag_id == control.drag_id && current.epoch == control.epoch
            }) {
                self.desired = None;
            }
            if let Some(active) = self
                .active
                .as_ref()
                .filter(|active| active.drag_id == control.drag_id && active.epoch == control.epoch)
            {
                active.handle.cancel();
            }
        } else {
            self.desired = Some((control, now));
        }
    }
    fn tick(&mut self, view: NativeDropView<'_>, region: DesktopRegion, now: Instant) {
        if self.desired.as_ref().is_some_and(|(control, received)| {
            now.saturating_duration_since(*received) >= Duration::from_secs(2)
                || !view.accepts(&control.epoch, control.drop.unwrap())
        }) {
            self.desired = None;
        }
        if let Some(active) = self.active.as_mut() {
            let next = self.desired.as_ref().filter(|(control, _)| {
                control.drag_id == active.drag_id
                    && control.epoch == active.epoch
                    && view.accepts(&active.epoch, active.point)
            });
            if let Some((control, received)) = next {
                if *received != active.updated {
                    active.point = control.drop.unwrap();
                    if active
                        .handle
                        .update(native_drop_pixel(active.point.x, active.point.y, region))
                        .is_err()
                    {
                        active.handle.cancel();
                    }
                    active.updated = *received;
                }
            } else {
                active.handle.cancel();
            }
            if active.handle.finished() {
                if let Some(error) = active.handle.error() {
                    tracing::debug!(%error, "native drag preview ended; final file drops remain available");
                }
                self.active = None;
            }
        }
        if self.active.is_none() {
            if let Some((control, received)) = self.desired.as_ref() {
                let token = (control.drag_id, control.epoch.clone());
                if self.attempted.as_ref() == Some(&token) {
                    return;
                }
                self.attempted = Some(token);
                let point = control.drop.unwrap();
                let types = control.types.as_ref().unwrap();
                match bubbleplay_host::native_file_drop::NativeDropPreview::start_with_types(
                    types.len() as u16,
                    types,
                    native_drop_pixel(point.x, point.y, region),
                ) {
                    Ok(handle) => {
                        self.active = Some(ActiveFileDragPreview {
                            drag_id: control.drag_id,
                            epoch: control.epoch.clone(),
                            point,
                            updated: *received,
                            handle,
                        })
                    }
                    Err(error) => {
                        tracing::debug!(%error, "native drag preview unavailable; using local preview")
                    }
                }
            }
        }
    }
    fn cancel(&mut self) {
        self.desired = None;
        if let Some(active) = self.active.as_ref() {
            active.handle.cancel();
        }
    }
    fn finished(&self) -> bool {
        self.active
            .as_ref()
            .is_none_or(|active| active.handle.finished())
    }
}

fn native_drag_blocks_motion(data: &[u8], input_sink: &mut InputSink) -> bool {
    if !bubbleplay_host::native_file_drop::drag_active() {
        return false;
    }
    // OLE owns the pointer and shares the destination's input queue. Preserve
    // releases while preventing new presses or text from changing that queue.
    input_sink.handle_release_only_counted(data);
    input_sink.note_packet_drop(InputDropReason::NativeDrag);
    true
}

fn reconcile_carried_input(
    carried_input: &mut input::CarriedInput,
    capture_available: bool,
    remote_input_ready: bool,
    input_sink: &mut InputSink,
) {
    if carried_input.take_resume_release(capture_available, remote_input_ready) {
        input_sink.release_all();
    }
}

struct PendingNativeDrop {
    request: bubbleplay_host::native_file_drop::NativeDropRequest,
    since: Instant,
    target: Option<bubbleplay_host::native_file_drop::NativeDropTarget>,
    input: Option<bubbleplay_host::native_file_drop::NativeDropInputGuard>,
}
impl PendingNativeDrop {
    fn note_input(&mut self, data: &[u8]) -> bool {
        if self.target.is_none() {
            return false;
        }
        prepared_drop_input_invalidates(data)
    }
}
fn prepared_drop_input_invalidates(data: &[u8]) -> bool {
    match data.first().copied() {
        Some(0) => data.len() == 3, // key press
        Some(4) => data.len() == 2 && data[1] != 0,
        Some(5) => data.len() == 3 && (data[1] != 0 || data[2] != 0),
        Some(7) => data.len() == 11 && data[7..11].iter().any(|value| *value != 0),
        Some(9) => data.len() == 8 && data[7] != 0,
        Some(11) => data.len() > 1,
        Some(12) => data.len() >= 8 && data[1] & 1 != 0, // paste, not clipboard synchronization
        Some(13) => data.len() == 6 && data[1] & 1 == 0,
        _ => false, // motion, releases and cursor feedback cannot change the binding
    }
}
fn invalidate_pending_drop_input(pending: &mut Option<PendingNativeDrop>, data: &[u8]) {
    if pending
        .as_mut()
        .is_some_and(|pending| pending.note_input(data))
    {
        pending.take().unwrap().request.reject("drop-interrupted");
        info!(
            reason = "drop-interrupted",
            "prepared native file drop cancelled"
        );
    }
}
fn native_drop_view_rejection(
    view: &NativeDropView<'_>,
    desktop: bool,
    request: &bubbleplay_host::native_file_drop::NativeDropRequest,
) -> Option<&'static str> {
    if !desktop {
        Some("drop-desktop-unavailable")
    } else if !view.ready || view.epoch != Some(request.epoch.as_str()) {
        Some("drop-not-ready")
    } else if request.point.generation != view.generation
        || request.point.viewport_revision != view.revision
    {
        Some("drop-stale-view")
    } else {
        None
    }
}

fn main() -> Result<()> {
    // Read and remove the tray's key handoff before anything else can spawn a child process.
    let device_key_handoff = bubbleplay_host::device_key::take_handoff_env();
    // Stderr keeps the human-readable lines the tray reads; the JSONL file is the record.
    let mut log_config = bubbleplay_host::logging::LogConfig::new(
        "host",
        bubbleplay_host::logging::default_log_dir(),
    );
    log_config.stderr = true;
    let _log_guard = bubbleplay_host::logging::init(log_config);

    let mut args = parse_args()?;
    if let Some(token) = args.token.as_deref() {
        bubbleplay_host::logging::redact::register_secret(token);
    }
    let handoff = device_key_handoff.as_deref().map(String::as_str);
    args.device_identity = match bubbleplay_host::device_key::identity_from_handoff(handoff) {
        Ok(identity) => {
            match identity.as_ref() {
                Some(identity) => {
                    bubbleplay_host::logging::set_device(Some(&identity.id().to_string()));
                }
                None => info!("no PC device key supplied; starting without a device ID"),
            }
            identity
        }
        Err(error) => {
            // The error names the failure class (format, passphrase, corrupt); it never holds key text.
            warn!(%error, "PC device key is unusable; starting without a device ID");
            None
        }
    };
    drop(device_key_handoff);
    if args.mode == "serve" {
        bubbleplay_host::input::recover_headless_cursor_support();
    }
    info!(
        build = env!("CARGO_PKG_VERSION"),
        "BubblePlay host starting"
    );
    match args.mode.as_str() {
        "spike" => run_spike(&args),
        "serve" => run_serve_forever(&args),
        other => bail!("unknown mode {other} (expected spike|serve)"),
    }
}

/// The display the client asked for, retained for the lifetime of the serve
/// loop so a session restart (peer left, ICE loss) reopens the same monitor.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CaptureSelection {
    selector: Option<String>,
    /// True once a live switch replaced the launch selector. Only a runtime
    /// selection is retried boundedly before the host refuses to serve; the
    /// launch selector keeps its original unbounded retry behavior.
    runtime: bool,
}

impl CaptureSelection {
    fn from_launch(launch: Option<&str>) -> Self {
        Self {
            selector: launch.map(str::to_string),
            runtime: false,
        }
    }

    fn selector(&self) -> Option<&str> {
        self.selector.as_deref()
    }

    fn is_runtime(&self) -> bool {
        self.runtime
    }

    /// Record a live switch. An empty selector leaves the selection untouched.
    fn remember(&mut self, selector: String) -> bool {
        if selector.is_empty() {
            return false;
        }
        self.selector = Some(selector);
        self.runtime = true;
        true
    }
}

/// Bounded retries before the host refuses to serve rather than capture an
/// unintended display after a remembered monitor disappears.
const REMEMBERED_MONITOR_MAX_ATTEMPTS: u32 = 3;

/// A failed serve run that had been registered with the relay this long was healthy; the next
/// retry starts over at the minimum backoff instead of the 30 s ceiling.
const SERVE_HEALTHY_RUN: Duration = Duration::from_secs(60);

fn next_serve_backoff(current: Duration, registered_for: Option<Duration>) -> Duration {
    if registered_for.is_some_and(|d| d > SERVE_HEALTHY_RUN) {
        Duration::from_secs(1)
    } else {
        (current * 2).min(Duration::from_secs(30))
    }
}

fn remembered_monitor_attempt_exhausted(attempts: u32) -> bool {
    attempts >= REMEMBERED_MONITOR_MAX_ATTEMPTS
}

/// Starts the per-user Devices pipe for the Flutter app, or logs why not.
fn start_devices_pipe(
    info: &bubbleplay_host::devices_window::DevicesWindowInfo,
    commands: std::sync::mpsc::Sender<bubbleplay_host::devices_ui_api::DevicesCommand>,
) -> Option<bubbleplay_host::devices_pipe::DevicesPipe> {
    use bubbleplay_host::devices_pipe as pipe;
    let sid = match pipe::current_user_sid() {
        Ok(sid) => sid,
        Err(error) => {
            warn!(%error, "Devices pipe not started: user SID unavailable");
            return None;
        }
    };
    let config = pipe::DevicesPipeConfig {
        pipe_name: pipe::pipe_name_for_sid(&sid),
        sid,
        pc_name: info.pc_name.clone(),
        pc_id: info.pc_id.clone(),
        relay_origin: info.relay_origin.clone(),
        relay_online: info.relay_online.clone(),
        relay_poll: Duration::from_secs(1),
    };
    match pipe::DevicesPipe::start(config, commands) {
        Ok(pipe) => Some(pipe),
        Err(error) => {
            warn!(%error, "Devices pipe not started; the app's Devices section stays offline");
            None
        }
    }
}

/// Serve mode wrapper: a session ends when the peer leaves or ICE dies, but
/// the host keeps running and accepts the next client. Fatal errors (bad
/// token, capture failure) still exit — only session teardown retries.
fn run_serve_forever(args: &Args) -> Result<()> {
    // The inline VDD keepalive outlives a single session: dropping it between
    // sessions would unplug a remembered virtual display.
    let _vdd = start_vdd_keepalive(args)?;
    let (devices_host, devices_ui) = bubbleplay_host::devices_ui_api::channel();
    let relay_online = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The real Devices window and the Devices pipe need the PC key; without
    // one the events are only logged.
    let devices_info = args.device_identity.as_ref().map(|identity| {
        bubbleplay_host::devices_window::DevicesWindowInfo {
            pc_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "This PC".into()),
            pc_id: identity.id().to_string(),
            relay_origin: args
                .worker
                .as_deref()
                .and_then(bubbleplay_host::device_key::relay_origin)
                .unwrap_or_default(),
            relay_online: relay_online.clone(),
        }
    });
    // The Flutter app's Devices section. Optional: the host runs on if the
    // pipe cannot be created, and nothing waits for a client.
    let devices_pipe = match &devices_info {
        Some(info) => start_devices_pipe(info, devices_ui.commands.clone()),
        None => {
            info!("no device identity; Devices pipe not started");
            None
        }
    };
    let pipe_handle = devices_pipe
        .as_ref()
        .map(bubbleplay_host::devices_pipe::DevicesPipe::handle);
    let devices_window = devices_info.map(|info| {
        bubbleplay_host::devices_window::DevicesWindow::new(info, devices_ui.commands.clone())
    });
    match devices_window.clone() {
        // The tray's "Devices..." line also raises an already-open window;
        // its "show-devices-app" line only reaches the pipe clients.
        Some(window) => {
            let (stdin_requests, stdin_lines) = std::sync::mpsc::channel();
            bubbleplay_host::devices_ui_api::spawn_stdin_request_reader(
                std::io::stdin(),
                stdin_requests,
            )?;
            let commands = devices_ui.commands.clone();
            let pipe = pipe_handle.clone();
            std::thread::Builder::new()
                .name("devices-open".into())
                .spawn(move || {
                    bubbleplay_host::devices_pipe::route_stdin_requests(
                        stdin_lines,
                        || window.open(),
                        commands,
                        pipe,
                    )
                })?;
        }
        None => {
            bubbleplay_host::devices_ui_api::spawn_stdin_reader(
                std::io::stdin(),
                devices_ui.commands.clone(),
            )?;
        }
    }
    std::thread::Builder::new()
        .name("devices-ui".into())
        .spawn(move || match devices_window {
            Some(window) => bubbleplay_host::devices_ui_api::run_ui(
                bubbleplay_host::devices_pipe::TeeDevicesUi {
                    ui: window,
                    pipe: pipe_handle,
                },
                devices_ui.events,
            ),
            None => bubbleplay_host::devices_ui_api::run_ui(
                bubbleplay_host::devices_ui_api::LogDevicesUi,
                devices_ui.events,
            ),
        })?;
    // Kept alive for the life of the host.
    let _devices_pipe = devices_pipe;
    let devices_host = std::sync::Arc::new(std::sync::Mutex::new(devices_host));
    let mut selection = CaptureSelection::from_launch(args.monitor.as_deref());
    let mut backoff = Duration::from_secs(1);
    let mut last_registered_at: Option<Instant> = None;
    let mut unavailable_attempts = 0u32;
    loop {
        if selection.is_runtime() {
            if let Some(selector) = selection.selector() {
                if let Err(error) = pick_monitor(Some(selector)) {
                    unavailable_attempts += 1;
                    error!(
                        monitor = selector,
                        attempt = unavailable_attempts,
                        ?error,
                        "remembered capture monitor is unavailable; refusing to capture a different display"
                    );
                    if remembered_monitor_attempt_exhausted(unavailable_attempts) {
                        return Err(error.context(format!(
                            "remembered capture monitor {selector} did not return"
                        )));
                    }
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
                unavailable_attempts = 0;
            }
        }
        let mut registered_at: Option<Instant> = None;
        let served = run_serve(
            args,
            &mut selection,
            devices_host.clone(),
            &relay_online,
            &mut registered_at,
        );
        relay_online.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(at) = registered_at {
            last_registered_at = Some(at);
        }
        match served {
            Ok(()) => {
                info!("session ended — waiting for next client");
                backoff = Duration::from_secs(1);
                std::thread::sleep(backoff);
            }
            Err(e) => {
                // Worker deploys reset the Durable Object, killing our WS
                // and briefly 500ing new connections. That's transient —
                // retry with backoff instead of taking the host down.
                // `{:#}` prints the whole anyhow chain on one line ("signaling socket read failed (io): <os or tls error>").
                let registered_for = registered_at.map(|at| at.elapsed());
                error!(
                    error = %format!("{e:#}"),
                    backoff_secs = backoff.as_secs(),
                    registered_for_secs = registered_for.map(|d| d.as_secs()),
                    since_last_registration_secs = last_registered_at.map(|at| at.elapsed().as_secs()),
                    "serve session failed — retrying"
                );
                std::thread::sleep(backoff);
                backoff = next_serve_backoff(backoff, registered_for);
            }
        }
    }
}

/// M1 spike: capture + encode to a local file, measure latency.
fn run_spike(args: &Args) -> Result<()> {
    info!(
        duration = args.duration_secs,
        monitor = args.monitor.as_deref().unwrap_or("primary"),
        "M1 spike: capture + NVENC"
    );
    let _vdd = start_vdd_keepalive(args)?;
    let mut capture = Capture::new(args.monitor.as_deref()).context("capture init")?;

    // First acquire pins the D3D11 device.
    let mut encoder: Option<VideoEncoder> = None;
    let mut out = std::io::BufWriter::new(
        std::fs::File::create(&args.out).with_context(|| format!("create {}", args.out))?,
    );
    use std::io::Write;

    let deadline = Instant::now() + Duration::from_secs(args.duration_secs as u64);
    let mut frames = 0u64;
    let mut lat_sum_us = 0u64;
    let mut lat_max_us = 0u64;
    let mut bytes = 0u64;

    while Instant::now() < deadline {
        let Some((frame, is_new, _recovered)) = capture.acquire(Duration::from_millis(100))? else {
            continue;
        };
        if encoder.is_none() {
            let dev = frame.device().as_raw();
            encoder = Some(VideoEncoder::select(
                dev,
                frame.width(),
                frame.height(),
                60,
                args.bitrate_bps,
                false,
                u32::from(EncoderOptions::production(Preset::P3).gop_seconds),
                args.encoder_preference(),
                None,
                &mut String::new(),
                || Nvenc::new(dev, frame.width(), frame.height(), 60, args.bitrate_bps),
            )?);
            let spspps = encoder.as_ref().unwrap().sequence_params()?;
            info!(len = spspps.len(), "SPS/PPS available");
        }
        if !is_new {
            continue; // duplicate desktop frame
        }
        let enc = encoder.as_mut().unwrap();
        let encoded = enc.encode_texture(texture_ptr(&frame))?;
        out.write_all(&encoded.data)?;
        frames += 1;
        bytes += encoded.data.len() as u64;
        lat_sum_us += encoded.encode_us;
        lat_max_us = lat_max_us.max(encoded.encode_us);
        if frames.is_multiple_of(60) {
            info!(
                frames,
                avg_ms = format!("{:.2}", lat_sum_us as f64 / frames as f64 / 1000.0),
                max_ms = format!("{:.2}", lat_max_us as f64 / 1000.0),
                kbps = bytes * 8 / (frames * 1000 / 60) / 1000,
                idr = encoded.is_idr,
                "encode stats"
            );
        }
    }
    out.flush()?;
    let (acq, new) = capture.stats();
    info!(
        frames_encoded = frames,
        acquired = acq,
        new_presents = new,
        avg_encode_ms = format!("{:.2}", lat_sum_us as f64 / frames.max(1) as f64 / 1000.0),
        max_encode_ms = format!("{:.2}", lat_max_us as f64 / 1000.0),
        file = args.out,
        bytes,
        "spike done"
    );
    Ok(())
}

/// Host-to-browser clipboard sync for the unprivileged local host. Text
/// copied while no viewer is live is never sent: each gap re-baselines.
#[derive(Default)]
struct LocalClipboardPoll {
    next_poll: Option<Instant>,
    live: bool,
    id: u32,
}

impl LocalClipboardPoll {
    const INTERVAL: Duration = Duration::from_millis(300);

    fn poll(
        &mut self,
        viewer_live: bool,
        now: Instant,
        input: &mut InputSink,
        signaling: &mut Signaling,
    ) {
        if !viewer_live {
            self.live = false;
            return;
        }
        if self.live && self.next_poll.is_some_and(|at| now < at) {
            return;
        }
        let reset = !std::mem::replace(&mut self.live, true);
        self.next_poll = Some(now + Self::INTERVAL);
        let Some(text) = input.poll_clipboard(reset) else {
            return;
        };
        self.id = self.id.wrapping_add(1);
        let (png, text) = text.wire();
        let chunks = bubbleplay_host::clipboard::chunk_text(
            &text,
            bubbleplay_host::clipboard::HOST_CHUNK_BYTES,
        );
        let count = chunks.len() as u16;
        for (index, chunk) in chunks.into_iter().enumerate() {
            signaling.send_clipboard_content_chunk(self.id, index as u16, count, chunk, png);
        }
    }
}

/// Clears the log's active session id when the serve loop ends, however it ends.
struct SessionLogScope;

impl Drop for SessionLogScope {
    fn drop(&mut self) {
        bubbleplay_host::logging::set_session(None);
    }
}

/// Logs the tray's viewer markers: "viewer session started" once a viewer's
/// media transport (RTC or WSS) is first up in this session, and "viewer
/// session ended" when the session ends by any path (peer left, error). The
/// tray uses these edges for lifecycle logging; current admin demand is
/// published independently so filtering INFO cannot suppress startup.
#[derive(Default)]
struct ViewerSessionLog {
    live: bool,
}

impl ViewerSessionLog {
    fn observe(&mut self, connected: bool) {
        if connected && !self.live {
            self.live = true;
            info!(
                "{}",
                bubbleplay_host::admin_agent::client::VIEWER_STARTED_MARKER
            );
        } else if !connected && self.live {
            self.live = false;
            info!(
                "{}",
                bubbleplay_host::admin_agent::client::VIEWER_ENDED_MARKER
            );
        }
    }
}

impl Drop for ViewerSessionLog {
    fn drop(&mut self) {
        if self.live {
            info!(
                "{}",
                bubbleplay_host::admin_agent::client::VIEWER_ENDED_MARKER
            );
        }
    }
}

/// Full pipeline: signaling + WebRTC + media + input.
/// WSS carries the session: stop the RTC transport thread and forget its
/// state, leaving signaling, input and the WSS media path untouched.
fn end_rtc_peer(
    pump: &TransportPump,
    ice_connected: &mut bool,
    pending_candidates: &mut VecDeque<String>,
    reason: &str,
) {
    *ice_connected = false;
    pending_candidates.clear();
    if !pump.command(TransportCommand::ResetPeer) {
        warn!(
            reason,
            "RTC transport did not accept the peer reset (queue full or stopped)"
        );
    }
    info!(reason, "RTC peer ended; the WSS session continues");
}

/// Live producer/session loop.  The transport pump exclusively owns str0m and
/// the media UDP socket; this thread owns capture, D3D scaling and encoding.
fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn run_serve(
    args: &Args,
    selection: &mut CaptureSelection,
    devices_ui: bubbleplay_host::dk_host::SharedDevicesUi,
    relay_online: &std::sync::atomic::AtomicBool,
    registered_at: &mut Option<Instant>,
) -> Result<()> {
    let watchdog = match session_watchdog::SessionWatchdog::start() {
        Ok(watchdog) => Some(watchdog),
        Err(error) => {
            warn!(%error, "session watchdog unavailable");
            None
        }
    };
    macro_rules! session_stage {
        ($stage:ident) => {
            if let Some(watchdog) = &watchdog {
                watchdog.mark(session_watchdog::Stage::$stage, line!());
            }
        };
    }
    macro_rules! session_call {
        ($stage:ident, $call:expr) => {{
            let _stage = watchdog
                .as_ref()
                .map(|watchdog| watchdog.enter(session_watchdog::Stage::$stage, line!()));
            $call
        }};
    }
    session_stage!(Startup);
    let worker = args
        .worker
        .clone()
        .context("--worker wss://... is required")?;
    bubbleplay_host::relay_url::require_secure_relay_url(&worker).context("--worker")?;
    // TOKEN is only needed for the PC's first registration; a known PC proves its key.
    let token = args.token.clone().unwrap_or_default();
    let reset_lab_encoder_on_diagnostic_keyframe =
        std::env::var("BUBBLEPLAY_LAB_RESET_ENCODER_ON_DIAGNOSTIC_KEYFRAME")
            .is_ok_and(|value| value == "1");
    let mut capture = session_call!(Capture, CaptureSource::open(args, selection.selector()))?;
    let mut encoder_discovery = EncoderDiscoveryCache::default();
    let mut discovered_adapter = {
        let selector = capture.monitor_selector();
        let adapter = capture::monitor_adapter(Some(&selector))?;
        Some(encoder_adapter(&adapter.cast()?)?)
    };
    let mut discovered_backend = match discovered_adapter {
        Some(adapter) => encoder_discovery.resolve(adapter, args.encoder_preference())?,
        None => EncoderBackend::Nvenc,
    };
    let mut discovery_reason = discovered_adapter
        .and_then(|adapter| encoder_discovery.reason(adapter))
        .unwrap_or_default()
        .to_owned();
    let socket = UdpSocket::bind("0.0.0.0:0").context("bind media socket")?;
    // Host-IP discovery only feeds RTC ICE candidates. WSS media and
    // signaling do not need it, so a failure (e.g. WSAEAFNOSUPPORT on a
    // machine without a usable default route) must not end `serve`.
    let host_addr = match pick_host_ip() {
        Ok(ip) => Some(SocketAddr::new(ip, socket.local_addr()?.port())),
        Err(error) => {
            warn!(
                ?error,
                "local host IP discovery failed; RTC host candidates disabled, WSS unaffected"
            );
            None
        }
    };
    socket.set_nonblocking(false)?;
    let mapped_addr = stun::discover_mapped_address(&socket, STUN_SERVER).ok();
    let mut signaling = session_call!(
        Signaling,
        Signaling::connect_phase2(
            &worker,
            &token,
            args.device_identity
                .as_ref()
                .context("PC device key required")?,
            devices_ui,
        )
    )?;
    relay_online.store(true, std::sync::atomic::Ordering::Relaxed);
    // Room and data-socket ticket for this signaling connection; the file and media sessions below
    // take copies and drop them when this run ends.
    let relay_route = signaling.relay_route();
    // The route's ticket expires 12 h after this hello-ok while the socket can
    // stay open; an idle session re-registers before then.
    let relay_registered_at = Instant::now();
    *registered_at = Some(relay_registered_at);
    // File transfer runs in the user-level host (Downloads, clipboard and Send To
    // belong to the interactive user); the authenticated signaling route supplies
    // the file socket ticket.
    let file_service = match FileService::spawn_routed(
        &worker,
        FileServiceConfig::from_env(),
        relay_route.clone(),
    ) {
        Ok(service) => Some(service),
        Err(error) => {
            warn!(error = %format!("{error:#}"), "file transfer unavailable");
            None
        }
    };
    signaling.set_file_transfer(file_service.is_some());
    signaling.set_file_drop(file_service.is_some());
    // Follow active window needs the interactive user's foreground window,
    // so only the local (tray) capture path advertises it.
    signaling.set_follow_window_supported(true);
    signaling.set_audio_enabled(args.audio);
    let mut next_file_rate_update = Instant::now();
    let mut file_drag_preview = FileDragPreviewState::default();
    let mut pending_native_drop: Option<PendingNativeDrop> = None;
    signaling.send_host_capabilities(false, false, false, false);
    let pinned = match args.quality.as_str() {
        "480p24" => Some(Profile::Mobile),
        "540p30" => Some(Profile::Low),
        "720p30" => Some(Profile::Balanced),
        "1080p60" => Some(Profile::Full),
        _ => None,
    };
    let mut stream_mode = StreamMode::Work;
    let mut quality = pinned
        .map(|p| {
            Controller::fixed_with_mode(
                p,
                args.max_bitrate_bps,
                args.fixed_bitrate_bps.unwrap_or(args.bitrate_bps),
                stream_mode,
            )
        })
        .unwrap_or_else(|| {
            Controller::adaptive(Profile::Full, args.max_bitrate_bps, 6_000_000, stream_mode)
        });
    let activity = Activity::new().context("producer activity wait")?;
    let cursor_wake = producer_wait::CursorWake::new(activity.clone());
    if let Err(error) = &cursor_wake {
        warn!(%error, "cursor notifications unavailable; retaining 16 ms cursor sampling");
    }
    capture.set_event_activity(activity.clone());
    if let Some(helper) = capture.secure_helper() {
        helper
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_activity(activity.clone());
    }
    let pump = TransportPump::spawn(socket, host_addr, mapped_addr, quality.target());
    pump.set_activity(activity.clone());
    pump.set_stun_server(STUN_SERVER)?;
    let rtc_audio_sink = pump.audio_sink();
    let mut wss_audio_sink = None;
    let mut encoder: Option<VideoEncoder> = None;
    let mut active_codec = VideoCodec::preferred();
    let mut wss_media: Option<WssMediaEgress> = None;
    let mut active_wss_epoch: Option<String> = None;
    let mut media_generation = 1u32;
    let mut media_sequence = 0u32;
    let media_clock_start = Instant::now();
    // Audio belongs to the viewer session, independently of desktop/video epochs.
    let mut audio_capture: Option<bubbleplay_host::audio::AudioCapture> = None;
    let mut rtc_audio_requested = false;
    let mut wss_audio_requested = false;
    let mut next_audio_start = Instant::now();
    let mut audio_start_failed = false;
    let mut scaler: Option<Scaler> = None;
    let preset = if args.preset == "p1" {
        Preset::P1
    } else {
        Preset::P3
    };
    let mut profiles =
        ProfileTransition::new_for_mode(Profile::Full, quality.profile(), stream_mode);
    let mut cpu_pacing = CpuPacing::from_env(Instant::now());
    let mut cpu_damage = CpuDamage::default();
    let mut cpu_requested_mode: Option<VideoMode> = None;
    let mut cpu_applied_mode: Option<VideoMode> = None;
    // Admin mode: the tray points the user-level host at the elevated input
    // agent; without it (or if it is unavailable) input stays user-level.
    let admin_agent = bubbleplay_host::admin_agent::client::AdminAgentLink::from_env();
    let mut input_sink = InputSink::new()
        .with_clipboard(true)
        .with_admin_agent(admin_agent)
        .with_secure_helper(capture.secure_helper());
    input_sink.set_absolute_region(capture.region());
    let mut local_clipboard = LocalClipboardPoll::default();
    let mut input_coalescer = InputPacketCoalescer::default();
    let mut cursor_tracker = CursorTracker::for_region(capture.region(), Instant::now());
    let mut cursor_kind_tracker = CursorKindTracker::new(Instant::now());
    let mut latest_cursor_kind: Option<[u8; 2]> = None;
    let mut next_cursor_poll = Instant::now();
    let mut next_cursor_event_sample = Instant::now();
    let mut next_input_log = Instant::now() + Duration::from_secs(1);
    let mut latest_idr: Option<CachedIdr> = None;
    let mut replay = CachedIdrReplay::default();
    let mut ice_connected = false;
    let mut viewer_attached = false;
    let mut display_hold = ViewerDisplayHold::default();
    let mut display_wake = DisplayWakePolicy::default();
    let mut rtc_reset_pending = false;
    let mut rtc_peer = RtcPeerLifecycle::default();
    let mut offer_accepted = false;
    let mut offer_in_flight = false;
    let mut negotiated_video_codec: Option<VideoCodec> = None;
    let mut pending_candidates = VecDeque::new();
    let mut next_admission = Instant::now();
    let mut next_idr = Instant::now();
    let mut next_ping = Instant::now() + SIGNALING_HEARTBEAT_INTERVAL;
    // v4: per-tile quality debt, no-op repaint skipping and tile refines.
    let mut tile_tracker: Option<TileQualityTracker> = None;
    let mut tile_hasher: Option<TileHasher> = None;
    // v4: scrolls found by shifted tile content (no DDA move rects).
    let mut scroll_detector: Option<ScrollDetector> = None;
    let mut scroll_detections: u64 = 0;
    let mut scroll_dy: i32 = 0;
    let mut tile_hashing_failed = false;
    let mut tile_hash_us: u64 = 0;
    // v4 on a constrained link: per-frame byte budget, content-aware pacing
    // and pointer-first refinement (inert while the link is unconstrained).
    let mut v4_pacer = V4Pacer::default();
    // Last sampled pointer, normalized to the capture region (0..=65535).
    let mut v4_pointer_norm: Option<(u16, u16)> = None;
    // NVENC frameAvgQP of v4 frames in the current 1 s window: (max, sum, count).
    let mut wss_v4_qp_window: (u32, u64, u64) = (0, 0, 0);
    let mut wss_v4_key_reason: Option<String> = None;
    let mut wss_v4_keyframes = std::collections::BTreeMap::<String, u64>::new();
    let mut next_wss_stats = Instant::now() + Duration::from_secs(1);
    let mut next_quality_tick = Instant::now() + Duration::from_secs(1);
    let mut benchmark_lease = args
        .benchmark_profile
        .clone()
        .map(benchmark::BenchmarkLease::at)
        .unwrap_or_else(benchmark::BenchmarkLease::program_data);
    let mut next_benchmark_refresh = Instant::now();
    let mut benchmark_profile: Option<benchmark::BenchmarkProfile> = None;
    let mut next_benchmark_status = Instant::now() + Duration::from_secs(1);
    let mut benchmark_window_started = Instant::now();
    let mut benchmark_sequence = 0u64;
    let mut benchmark_profile_revision = 0u64;
    let mut encoder_benchmark_profile_revision: Option<u64> = None;
    let mut benchmark_encoded_bytes = 0u64;
    let mut benchmark_wss_payload_bytes = 0u64;
    let mut benchmark_encode_count = 0u64;
    let mut benchmark_encode_us_total = 0u64;
    let mut benchmark_encode_us_max = 0u32;
    let mut benchmark_idr_count = 0u64;
    let mut benchmark_delta_count = 0u64;
    let mut benchmark_skips = 0u64;
    let mut benchmark_qp_map_frames = 0u64;
    let mut benchmark_qp_map_changed_ctbs = 0u64;
    let mut benchmark_qp_map_total_ctbs = 0u64;
    let mut benchmark_roi_activity: Option<BenchRoiActivity> = None;
    let mut benchmark_temporal_analysis_us = Vec::<u32>::new();
    let mut benchmark_hybrid_activity: Option<BenchHybridActivity> = None;
    let mut benchmark_hybrid_analysis_us = Vec::<u32>::new();
    let mut benchmark_hybrid_validation_submit_us = Vec::<u32>::new();
    let mut benchmark_hybrid_validation_collect_us = Vec::<u32>::new();
    let mut benchmark_hybrid_validation_age_frames = Vec::<u32>::new();
    let mut temporal_diff_analyzer: Option<TemporalDiffAnalyzer> = None;
    let mut temporal_diff_key: Option<(u64, u32, u32, VideoMode)> = None;
    let mut temporal_diff_failed_key: Option<(u64, u32, u32, VideoMode)> = None;
    let mut reconstruction_feedback_analyzer: Option<ReconstructionFeedbackAnalyzer> = None;
    let mut feedback_roi_planner: Option<FeedbackRoiPlanner> = None;
    let mut reconstruction_feedback_key: Option<(u64, u32, u32, VideoMode)> = None;
    let mut reconstruction_feedback_failed_key: Option<(u64, u32, u32, VideoMode)> = None;
    let mut benchmark_feedback_submit_us = Vec::<u32>::new();
    let mut benchmark_feedback_readback_us = Vec::<u32>::new();
    let mut benchmark_feedback_wire_occupancy_bps = Vec::<u64>::new();
    let mut feedback_wire_budget: Option<FeedbackWireBudget> = None;
    let mut hybrid_tile_planner = StaticTilePlanner::default();
    let mut quiescent_tile_planner = QuiescentTilePlanner::default();
    let mut quiescent_wire_budget = HybridWireBudget::default();
    let mut quiescent_diff_analyzer: Option<TemporalDiffAnalyzer> = None;
    let mut quiescent_diff_revision = 0u64;
    let mut hevc_tile_encoder: Option<HevcTileEncoder> = None;
    let mut hevc_tile_encoder_failed_key: Option<(u64, usize)> = None;
    let mut benchmark_encode_stages = BenchEncodeStages::default();
    let mut last_keyframe_request = Instant::now()
        .checked_sub(KEYFRAME_REQUEST_COOLDOWN)
        .unwrap_or_else(Instant::now);
    let mut skipped_before_encode = 0u64;
    let mut next_skip_log = Instant::now() + Duration::from_secs(1);
    let mut wss_preencode_skips = 0u64;
    let mut wss_preencode_skips_total = 0u64;
    let mut wss_submission_failures = 0u64;
    let mut wss_idr_count = 0u64;
    let mut wss_delta_count = 0u64;
    let mut wss_max_access_unit_bytes = 0usize;
    let mut next_au_id = 1u64;
    let mut capture_available = true;
    let mut next_local_recovery_probe = Instant::now();
    let mut remote_input_ready = true;
    let mut carried_input = input::CarriedInput::default();
    let mut last_secure_announcement: Option<(bool, &'static str, Vec<&'static str>)> = None;
    let mut last_secure_helper_ready = false;
    let mut next_secure_state_poll = Instant::now();
    let mut pending_transition_idr: Option<PendingSubmittedIdr> = None;
    let mut pending_wss_idr: Option<PendingWssIdr> = None;
    let mut pending_lab_quality_keyframe: Option<String> = None;
    let mut pending_first_frame: Option<PendingFirstFrame> = None;
    let mut capture_watchdog = CaptureStallWatchdog::default();
    // (generation, RTP timestamp) of the startup IDR last admitted on WebRTC.
    let mut startup_idr_rtp: Option<(u32, u32)> = None;
    let mut completed_first_frame: Option<(String, u32, String, Option<String>)> = None;
    let mut startup_idr_claimable = true;
    let mut startup_idr_admitted: Option<(u32, &'static str)> = None;
    let mut startup_idr_submitted: Option<(u32, &'static str, u64)> = None;
    let mut pending_viewport_idr: Option<PendingViewportIdr> = None;
    let mut viewport_crop: Option<ViewportCrop> = None;
    let mut active_viewport: Option<(u32, u32)> = None;
    let mut viewport_revision = 0u32;
    let mut last_logged_bwe: Option<u32> = None;
    let mut producer_loop_iterations = 0u64;
    let mut stats_rollup = stats_window::Rollup::new(Instant::now(), stats_window::INFO_INTERVAL);
    // Lines logged while a viewer session is up carry its epoch as `session`.
    let _session_log = SessionLogScope;
    let mut logged_session_epoch: Option<String> = None;
    let mut producer_window_started = Instant::now();
    let mut last_capture_stats = (0u64, 0u64);
    let mut viewer_session = ViewerSessionLog::default();
    // One WSS stats line: the producer window `$w` (one second, or the sum of a rollup
    // interval) with the link totals as they stand now. `$level` is `debug` or `info`.
    macro_rules! wss_stats_line {
        ($level:ident, $egress:expr, $w:expr) => {{
            let w: &stats_window::ProducerWindow = $w;
            let v4 = $egress.stats();
            let tiles = tile_tracker
                .as_ref()
                .map(TileQualityTracker::stats)
                .unwrap_or_default();
            let debt_tiles = tile_tracker
                .as_ref()
                .map_or(0, TileQualityTracker::debt_tiles);
            let pacing = v4_pacer.stats();
            let classes = v4_pacer.counts();
            tracing::$level!(
                wss_preencode_skips = w.preencode_skips,
                wss_submission_failures = w.submission_failures,
                wss_idr_count = w.idr_count,
                wss_delta_count = w.delta_count,
                idr_percent = w.idr_percent(),
                span_ms = w.span_ms,
                producer_loop_iterations = w.loop_iterations,
                capture_acquired = w.capture_acquired,
                capture_new = w.capture_new,
                wss_max_access_unit_bytes = w.max_access_unit_bytes,
                connected = v4.connected,
                peer_present = v4.peer_present,
                frames_sent = v4.frames_sent,
                bytes_sent = v4.bytes_sent,
                keyframes_sent = v4.keyframes_sent,
                refines_sent = v4.refines_sent,
                skipped_budget = v4.skipped_budget,
                acks = v4.acks,
                heartbeats = v4.heartbeats,
                reconnects = v4.reconnects,
                inflight_frames = v4.inflight_frames,
                inflight_bytes = v4.inflight_bytes,
                bitrate_bps = v4.bitrate_bps,
                min_rtt_ms = v4.min_rtt_ms,
                last_rtt_ms = v4.last_rtt_ms,
                ack_timeouts = v4.ack_timeouts,
                keyframes = %format_keyframe_reasons(&wss_v4_keyframes),
                noop_skipped = tiles.noop_repaints_skipped,
                debt_tiles,
                tiles_refined = tiles.tiles_refined,
                refine_frames = tiles.refine_frames,
                whole_frame_refines = tiles.whole_frame_refines,
                hashed_tiles = tiles.hashed_tiles,
                unhashed_tiles = tiles.unhashed_tiles,
                tile_hash_us = w.tile_hash_us,
                frame_qp_max = w.qp_max,
                frame_qp_mean = w.qp_mean(),
                capacity_bps = v4.capacity_bps,
                rate_cap_bps = v4.rate_cap_bps,
                constrained_ms = v4.constrained_ms,
                budget_tokens = v4_pacer.tokens(),
                content = classes.dominant().as_str(),
                text_tiles = classes.text,
                scroll_tiles = classes.scroll,
                video_tiles = classes.video,
                max_qp = encoder.as_ref().and_then(VideoEncoder::max_qp).unwrap_or(0),
                paced_skips = pacing.paced_skips,
                refines_deferred = pacing.refines_deferred,
                refines_trimmed = pacing.refines_trimmed,
                pointer_maps = pacing.pointer_maps,
                scroll_detections,
                scroll_dy,
                "WSS stats"
            );
        }};
    }
    // Current demand is control state, independent of filtered lifecycle logs.
    // The host holds this object so a new tray can discover an active viewer.
    let admin_demand = bubbleplay_host::admin_agent::demand::DemandSignal::from_env()?;
    let mut last_routing_status = None;
    let mut last_admin_failure = None;
    let mut next_admin_diagnostic = Instant::now();
    // Follow active window: per signaling session, off until the client asks.
    let mut follow_state: Option<FollowWindow> = None;
    let mut next_follow_sample = Instant::now();
    // Edge hold: sampled every 50 ms only while a move loop is active; the
    // monitor list is read once per loop.
    let mut edge_monitors: Option<Vec<capture::MonitorInfo>> = None;
    let mut next_edge_sample = Instant::now();
    // One live monitor switch, shared by the manual Monitors pick and
    // "Follow active window". A macro because the switch resets most of the
    // session locals below; it must stay defined after all of them.
    // `carry_input: true` (edge hold) keeps held buttons and keys so the
    // window being dragged stays in its move loop across the switch; the
    // viewer's release during the presentation gate is reconciled on resume.
    macro_rules! switch_capture_monitor {
        ($selector:expr) => {
            switch_capture_monitor!($selector, false)
        };
        ($selector:expr, $carry_input:expr) => {{
            let selector: &str = $selector;
            if !capture_available
                || !capture.desktop_state().available
                || capture.desktop_state().desktop == "secure"
            {
                signaling.send_monitor_error(
                    selector,
                    "desktop unavailable; monitor selection is suspended",
                );
            } else {
                carried_input.begin_switch($carry_input);
                if !$carry_input {
                    session_call!(Input, input_sink.release_all());
                }
                remote_input_ready = false;
                latest_cursor_kind = None;
                cursor_kind_tracker.reset();
                signaling.send_desktop_state("unavailable", media_generation, "monitor-switch");
                media_generation = media_generation.wrapping_add(1).max(1);
                media_sequence = 0;
                pending_first_frame = None;
                completed_first_frame = None;
                latest_idr = None;
                pending_transition_idr = None;
                pending_wss_idr = None;
                pending_viewport_idr = None;
                encoder = None;
                scaler = None;
                next_admission = Instant::now();
                next_idr = Instant::now();
                match capture.switch_monitor(selector) {
                    Ok(()) => {
                        // Local capture completes synchronously.
                        capture_available = true;
                        // The client owns the display choice for
                        // the lifetime of the host process: a
                        // later session reopens the same monitor.
                        selection.remember(capture.monitor_selector());
                        refresh_input_geometry(
                            &mut input_sink,
                            &mut cursor_tracker,
                            capture.region(),
                            Instant::now(),
                        );
                        signaling.send_desktop_state(
                            "active",
                            media_generation,
                            "monitor-switched",
                        );
                        if let Ok(monitors) = capture.monitors() {
                            signaling.send_monitor_list(monitors);
                        }
                        info!(
                            monitor = selector,
                            generation = media_generation,
                            "live monitor switch complete"
                        );
                    }
                    Err(error) => {
                        capture_available = capture.desktop_state().available;
                        signaling.send_monitor_error(selector, &error.to_string());
                        if retained_capture_after_failed_switch(capture.desktop_state()) {
                            refresh_input_geometry(
                                &mut input_sink,
                                &mut cursor_tracker,
                                capture.region(),
                                Instant::now(),
                            );
                            announce_capture_state(
                                &mut signaling,
                                &mut capture,
                                media_generation,
                                "monitor-switched",
                            );
                            capture.request_idle_snapshot();
                        }
                        if let Ok(monitors) = capture.monitors() {
                            signaling.send_monitor_list(monitors);
                        }
                        warn!(
                            monitor = selector,
                            ?error,
                            "live monitor switch failed; retaining previous capture"
                        );
                    }
                }
            }
        }};
    }
    macro_rules! park_viewer {
        ($reason:expr) => {{
            remote_input_ready = false;
            park_viewer!(@keep_input $reason);
        }};
        // For a caller that re-arms remote input itself right afterwards.
        (@keep_input $reason:expr) => {{
            if let Some(files) = &file_service {
                files.set_epoch(None);
            }
            file_drag_preview.cancel();
            if let Some(pending) = pending_native_drop.take() {
                pending.request.reject("viewer-disconnected");
            }
            session_call!(Input, input_sink.release_all());
            capture.set_capture_demand(false)?;
            capture.end_viewer_session();
            carried_input.reset();
            viewer_attached = false;
            // The viewer's media epoch ends with it. A kept egress would keep
            // reconnecting to a retired epoch that the relay refuses.
            if let Some(egress) = wss_media.take() {
                egress.retire_epoch();
                egress.shutdown();
            }
            active_wss_epoch = None;
            audio_capture = None;
            rtc_audio_requested = false;
            wss_audio_requested = false;
            wss_audio_sink = None;
            audio_start_failed = false;
            display_hold.set_attached(false);
            display_wake.reset();
            input_sink.reset_status_delivery();
            viewer_session.observe(false);
            admin_demand.observe(false)?;
            ice_connected = false;
            offer_accepted = false;
            offer_in_flight = false;
            negotiated_video_codec = None;
            pending_candidates.clear();
            ensure!(
                pump.command(TransportCommand::ResetPeer),
                "RTC transport stopped during peer reset"
            );
            rtc_reset_pending = true;
            rtc_peer = RtcPeerLifecycle::default();
            replay = CachedIdrReplay::default();
            latest_idr = None;
            pending_first_frame = None;
            completed_first_frame = None;
            startup_idr_claimable = true;
            startup_idr_admitted = None;
            startup_idr_submitted = None;
            pending_transition_idr = None;
            pending_wss_idr = None;
            pending_viewport_idr = None;
            pending_lab_quality_keyframe = None;
            follow_state = None;
            signaling.set_follow(false);
            viewport_crop = None;
            active_viewport = None;
            viewport_revision = 0;
            profiles.announced = None;
            tile_tracker = None;
            tile_hasher = None;
            scroll_detector = None;
            v4_pacer = V4Pacer::default();
            if let Some(encoder) = encoder.as_mut() {
                encoder.force_idr();
            }
            next_admission = Instant::now();
            info!(
                reason = $reason,
                encoder_warm = encoder.is_some(),
                "viewer left; host pipeline remains warm"
            );
        }};
    }
    macro_rules! begin_viewer_input {
        () => {{
            begin_viewer_input!(@keep_input);
            remote_input_ready = capture_available;
        }};
        // For a caller that re-arms remote input itself right afterwards.
        (@keep_input) => {{
            session_call!(Input, input_sink.reset_viewer_session());
            input_coalescer = InputPacketCoalescer::default();
            signaling.reset_viewer_cursor_state();
            carried_input.reset();
            cursor_tracker = CursorTracker::for_region(capture.region(), Instant::now());
            cursor_kind_tracker.reset();
            latest_cursor_kind = None;
            // A new viewer starts a fresh media identity.
            media_generation = media_generation.wrapping_add(1).max(1);
            media_sequence = 0;
            info!("viewer input sequence space reset");
        }};
    }
    let mut first_iteration = true;
    'session: loop {
        // Apply authentication changes before draining transport input or file work.
        let mut signal_events = signaling.poll()?;
        if signal_events
            .iter()
            .any(|event| matches!(event, SignalEvent::PeerLeft))
        {
            park_viewer!("device-session-ended");
        }
        session_stage!(CaptureDemand);
        let wait_now = Instant::now();
        let wss_peer_live = viewer_attached
            && wss_media.as_ref().is_some_and(|egress| {
                let stats = egress.stats();
                stats.connected && stats.peer_present
            });
        let capture_demand = signaling.application_ready()
            && viewer_attached
            && (offer_in_flight || offer_accepted || ice_connected || wss_peer_live);
        let waiting_mode = profiles
            .staged
            .map(|(mode, _)| mode)
            .unwrap_or(profiles.active);
        let waiting_interval = Duration::from_secs_f64(1.0 / (waiting_mode.fps as f64).max(1.0));
        let admission_wake = frame_admission_wake_at(next_admission, waiting_interval);
        let urgent_capture = latest_idr.is_none()
            || profiles.staged().is_some()
            || encoder.as_ref().is_some_and(VideoEncoder::idr_pending);
        let recovery_due = signaling.application_ready()
            && viewer_attached
            && local_recovery_probe_due(capture_available, wait_now, next_local_recovery_probe);
        capture.set_capture_demand(capture_demand || recovery_due)?;
        if capture_demand || recovery_due {
            capture.request_event_frame_at(if urgent_capture || recovery_due {
                wait_now
            } else {
                admission_wake.max(wait_now)
            })?;
        }
        if let Ok(wake) = &cursor_wake {
            wake.set_active(capture_demand);
        }
        if !first_iteration && signal_events.is_empty() {
            let mut deadlines = vec![
                next_ping,
                next_benchmark_refresh,
                next_input_log,
                wait_now + Duration::from_millis(250),
            ];
            if !viewer_attached {
                if let Some(deadline) = capture.parked_maintenance_deadline() {
                    deadlines.push(deadline);
                }
            }
            // An absent/incompatible lease intentionally leaves status retry
            // due immediately. Only schedule it while a sample can be sent.
            if benchmark_lease.active().is_some_and(|profile| {
                active_codec == VideoCodec::Hevc
                    && benchmark_mode_matches(profile.stream_mode, stream_mode)
            }) {
                deadlines.push(next_benchmark_status);
            }
            if capture_demand {
                if capture.desktop_state().desktop != "secure" {
                    deadlines.push(next_cursor_poll);
                    if cursor_wake
                        .as_ref()
                        .is_ok_and(|wake| wake.changed_pending())
                    {
                        deadlines.push(next_cursor_event_sample);
                    }
                }
                if wss_media.is_some() {
                    deadlines.push(next_wss_stats);
                }
                if let Some(deadline) = capture.next_event_deadline(should_snapshot_idle_desktop(
                    ice_connected || wss_peer_live,
                    latest_idr.is_none(),
                )) {
                    deadlines.push(deadline);
                }
                if foreground_sample_live(
                    viewer_attached,
                    capture_available,
                    capture.desktop_state().desktop,
                    ice_connected || wss_peer_live,
                ) {
                    deadlines.push(next_follow_sample);
                    if edge_monitors.is_some() {
                        deadlines.push(next_edge_sample);
                    }
                }
                if file_service.is_some() {
                    deadlines.push(next_file_rate_update);
                }
            }
            if viewer_attached && !capture_available {
                deadlines.push(next_local_recovery_probe);
            }
            if args.fixed_bitrate_bps.is_none() && wss_media.is_none() {
                deadlines.push(next_quality_tick);
            }
            let wait = producer_wait_duration(wait_now, deadlines);
            session_call!(
                ActivityWait,
                activity.wait(wait, &[(signaling.read_socket(), false)])
            )?;
        }
        first_iteration = false;
        // The socket may have woken the wait. Authenticate those frames before any
        // transport input, asynchronous file completions or capture work this tick.
        let awakened_events = signaling.poll()?;
        if awakened_events
            .iter()
            .any(|event| matches!(event, SignalEvent::PeerLeft))
        {
            signal_events.clear();
            park_viewer!("device-session-ended-after-wait");
        }
        signal_events.extend(awakened_events);
        session_stage!(Housekeeping);
        producer_loop_iterations = producer_loop_iterations.saturating_add(1);
        if signaling.viewer_epoch() != logged_session_epoch.as_deref() {
            logged_session_epoch = signaling.viewer_epoch().map(str::to_owned);
            bubbleplay_host::logging::set_session(logged_session_epoch.as_deref());
        }
        let loop_now = Instant::now();
        if loop_now >= next_benchmark_refresh {
            let next_profile = benchmark_lease.refresh(unix_time_ms()).cloned();
            if next_profile != benchmark_profile {
                benchmark_profile_revision =
                    advance_benchmark_profile_revision(benchmark_profile_revision, true);
                match (benchmark_profile.as_ref(), next_profile.as_ref()) {
                    (None, Some(profile)) => {
                        info!(run_id = %profile.run_id, "stream-lab benchmark lease activated")
                    }
                    (Some(previous), Some(profile)) => info!(
                        run_id = %profile.run_id,
                        previous_bitrate_bps = previous.bitrate_bps,
                        bitrate_bps = profile.bitrate_bps,
                        "stream-lab benchmark profile changed"
                    ),
                    (Some(_), None) => {
                        info!("stream-lab benchmark lease cleared; production policy restored")
                    }
                    (None, None) => {}
                }
                // The local candidate owns its NVENC session. Any effective
                // lease change must recreate that session so bench-status
                // cannot claim options that the encoder never applied.
                encoder = None;
                scaler = None;
                latest_idr = None;
                pending_transition_idr = None;
                pending_wss_idr = None;
                pending_viewport_idr = None;
                next_admission = Instant::now();
                next_idr = Instant::now();
                benchmark_profile = next_profile;
                benchmark_roi_activity = benchmark_profile
                    .as_ref()
                    .and_then(new_benchmark_roi_activity);
                benchmark_hybrid_activity = benchmark_profile
                    .as_ref()
                    .and_then(new_benchmark_hybrid_activity);
                benchmark_sequence = 0;
                benchmark_window_started = loop_now;
                benchmark_encoded_bytes = 0;
                benchmark_wss_payload_bytes = 0;
                benchmark_encode_count = 0;
                benchmark_encode_us_total = 0;
                benchmark_encode_us_max = 0;
                benchmark_idr_count = 0;
                benchmark_delta_count = 0;
                benchmark_skips = 0;
                benchmark_qp_map_frames = 0;
                benchmark_qp_map_changed_ctbs = 0;
                benchmark_qp_map_total_ctbs = 0;
                benchmark_temporal_analysis_us.clear();
                benchmark_hybrid_analysis_us.clear();
                benchmark_hybrid_validation_submit_us.clear();
                benchmark_hybrid_validation_collect_us.clear();
                benchmark_hybrid_validation_age_frames.clear();
                temporal_diff_analyzer = None;
                temporal_diff_key = None;
                temporal_diff_failed_key = None;
                reconstruction_feedback_analyzer = None;
                feedback_roi_planner = None;
                reconstruction_feedback_key = None;
                reconstruction_feedback_failed_key = None;
                benchmark_feedback_submit_us.clear();
                benchmark_feedback_readback_us.clear();
                benchmark_feedback_wire_occupancy_bps.clear();
                feedback_wire_budget = None;
                benchmark_encode_stages = BenchEncodeStages::default();
            }
            next_benchmark_refresh = loop_now + Duration::from_secs(1);
        }
        // Publish benchmark state before capture or encoding. Candidate encoder
        // failures deliberately continue the session for recovery; placing this
        // at the producer tail starves the only telemetry that lets Stream Lab
        // classify an unsupported NVENC combination and move on safely.
        let benchmark_status_now = Instant::now();
        if benchmark_status_now >= next_benchmark_status {
            let status_sent = if let Some(profile) =
                benchmark_lease.active().cloned().filter(|profile| {
                    active_codec == VideoCodec::Hevc
                        && benchmark_mode_matches(profile.stream_mode, stream_mode)
                }) {
                benchmark_sequence = benchmark_sequence.wrapping_add(1);
                signaling.send_bench_status(
                    profile.schema_version,
                    &profile.run_id,
                    benchmark_sequence,
                    benchmark_profile_revision,
                    "sample",
                    if profile.schema_version == 2 {
                        Some(match profile.implementation_variant {
                            benchmark::BenchmarkImplementationVariant::Production => "production",
                            benchmark::BenchmarkImplementationVariant::Candidate => "candidate",
                        })
                    } else {
                        None
                    },
                    profile.experiment_id.as_deref(),
                    match profile.transport {
                        BenchmarkTransport::Wss => "wss",
                        BenchmarkTransport::Webrtc => "webrtc",
                    },
                    "hevc",
                    match profile.stream_mode {
                        BenchmarkStreamMode::Work => "work",
                        BenchmarkStreamMode::Game => "game",
                    },
                    profile.width,
                    profile.height,
                    profile.fps,
                    profile.bitrate_bps,
                    profile.gop_seconds,
                    match profile.encoder.preset {
                        BenchmarkPreset::P1 => "p1",
                        BenchmarkPreset::P2 => "p2",
                        BenchmarkPreset::P3 => "p3",
                        BenchmarkPreset::P4 => "p4",
                        BenchmarkPreset::P5 => "p5",
                        BenchmarkPreset::P6 => "p6",
                        BenchmarkPreset::P7 => "p7",
                    },
                    match profile.encoder.tuning {
                        BenchmarkTuning::UltraLowLatency => "ultra-low-latency",
                        BenchmarkTuning::LowLatency => "low-latency",
                    },
                    match profile.encoder.multipass {
                        BenchmarkMultipass::Disabled => "disabled",
                        BenchmarkMultipass::Quarter => "quarter",
                        BenchmarkMultipass::Full => "full",
                    },
                    profile.encoder.aq_strength,
                    match profile.effective_aq_mode() {
                        BenchmarkAqMode::None => "none",
                        BenchmarkAqMode::Spatial => "spatial",
                        BenchmarkAqMode::Temporal => "temporal",
                    },
                    profile.encoder.vbv_frames,
                    match profile.encoder.rate_control_mode {
                        benchmark::BenchmarkRateControlMode::Cbr => "cbr",
                        benchmark::BenchmarkRateControlMode::ExternalBudgetVbr => {
                            "external-budget-vbr"
                        }
                    },
                    profile.encoder.target_quality,
                    profile.encoder.peak_bitrate_multiplier,
                    profile.encoder.lookahead_depth,
                    profile.encoder.weighted_prediction,
                    profile.encoder.dirty_qp_map,
                    profile.encoder.main10,
                    match profile.effective_roi_mode() {
                        BenchmarkRoiMode::Off => "off",
                        BenchmarkRoiMode::Damage => "damage",
                        BenchmarkRoiMode::TextEdge => "text-edge",
                        BenchmarkRoiMode::Feedback => "feedback",
                        BenchmarkRoiMode::FeedbackHeadroom => "feedback-headroom",
                    },
                    match profile.encoder.resource_mode {
                        BenchmarkResourceMode::PerFrame => "per-frame",
                        BenchmarkResourceMode::Persistent => "persistent",
                    },
                    profile.encoder.async_depth,
                    match profile.hybrid_mode {
                        BenchmarkHybridMode::Off => "off",
                        BenchmarkHybridMode::PngStaticRoi => "png-static-roi",
                        BenchmarkHybridMode::PngQuiescentDetail => "png-quiescent-detail",
                        BenchmarkHybridMode::HevcQuiescentTiles => "hevc-quiescent-tiles",
                    },
                    match profile.hybrid_cache_mode {
                        BenchmarkHybridCacheMode::DdaGated => "dda-gated",
                        BenchmarkHybridCacheMode::SuppressReactivate => "suppress-reactivate",
                    },
                    benchmark_window_started
                        .elapsed()
                        .as_millis()
                        .min(u64::MAX as u128) as u64,
                    benchmark_encoded_bytes,
                    benchmark_wss_payload_bytes,
                    benchmark_encode_count,
                    benchmark_encode_us_total,
                    benchmark_encode_us_max,
                    benchmark_idr_count,
                    benchmark_delta_count,
                    benchmark_skips,
                    benchmark_qp_map_frames,
                    benchmark_qp_map_changed_ctbs,
                    benchmark_qp_map_total_ctbs,
                    benchmark_roi_activity.clone(),
                    (profile.schema_version == 2).then(|| benchmark_encode_stages.clone()),
                    benchmark_hybrid_activity.clone(),
                );
                benchmark_encoded_bytes = 0;
                benchmark_wss_payload_bytes = 0;
                benchmark_encode_count = 0;
                benchmark_encode_us_total = 0;
                benchmark_encode_us_max = 0;
                benchmark_idr_count = 0;
                benchmark_delta_count = 0;
                benchmark_skips = 0;
                benchmark_qp_map_frames = 0;
                benchmark_qp_map_changed_ctbs = 0;
                benchmark_qp_map_total_ctbs = 0;
                benchmark_temporal_analysis_us.clear();
                benchmark_feedback_submit_us.clear();
                benchmark_feedback_readback_us.clear();
                benchmark_hybrid_analysis_us.clear();
                benchmark_hybrid_validation_submit_us.clear();
                benchmark_hybrid_validation_collect_us.clear();
                benchmark_hybrid_validation_age_frames.clear();
                benchmark_roi_activity = new_benchmark_roi_activity(&profile);
                benchmark_hybrid_activity = new_benchmark_hybrid_activity(&profile);
                benchmark_encode_stages = BenchEncodeStages::default();
                benchmark_window_started = Instant::now();
                true
            } else {
                false
            };
            next_benchmark_status =
                benchmark_status_now + benchmark_status_retry_delay(status_sent);
        }
        session_stage!(SecureHelper);
        capture.tick_secure(
            viewer_attached && (ice_connected || wss_peer_live),
            loop_now,
        );
        capture.maintain_parked(viewer_attached, loop_now)?;
        let secure_helper_ready = capture.secure_helper_ready();
        if secure_helper_ready != last_secure_helper_ready {
            signaling.send_host_capabilities(
                false,
                false,
                secure_helper_ready,
                secure_helper_ready,
            );
            last_secure_helper_ready = secure_helper_ready;
        }
        input_sink.set_secure_desktop(
            capture.secure_state().is_some(),
            capture.secure_input_allowed(),
        );
        if let Some(link) = capture.secure_helper() {
            while let Some(result) = link
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take_deny_result()
            {
                signaling.send_secure_action_result(result.ok, &result.reason);
            }
        }
        let CaptureSource::Local(source) = &mut capture;
        if let Some(state) = source.take_availability_transition() {
            match state {
                CaptureAvailability::Unavailable => {
                    capture_available = false;
                    next_local_recovery_probe = Instant::now();
                    // Keep startup alive through local capture recovery;
                    // its own watchdog retains a bounded deadline.
                    carried_input.reset();
                    session_call!(Input, input_sink.release_all());
                    latest_idr = None;
                    pending_transition_idr = None;
                    pending_wss_idr = None;
                    pending_viewport_idr = None;
                    encoder = None;
                    scaler = None;
                    if let Some(secure) = source.secure_state.as_ref().filter(|state| state.secure)
                    {
                        signaling.send_secure_desktop_state(secure.kind, &[], media_generation);
                    } else if let Some(kind) =
                        bubbleplay_host::desktop_session::secure_desktop_kind()
                    {
                        signaling.send_secure_desktop_state(kind, &[], media_generation);
                    } else if !source
                        .unavailable_announcement_deferred(viewer_attached, Instant::now())
                    {
                        signaling.send_desktop_state(
                            "unavailable",
                            media_generation,
                            "capture-unavailable",
                        );
                    }
                    // A powered-off display comes back by itself: expected, not a fault.
                    let reason = source.unavailable_reason();
                    if reason == "display off" {
                        info!(
                            generation = media_generation,
                            %reason,
                            "desktop capture unavailable; input suspended"
                        );
                    } else {
                        warn!(
                            generation = media_generation,
                            %reason,
                            "desktop capture unavailable; input suspended"
                        );
                    }
                }
                CaptureAvailability::Active => {
                    capture_available = true;
                    media_generation = media_generation.wrapping_add(1).max(1);
                    if let Some(pending) = pending_first_frame.as_mut() {
                        pending.rebind_after_recovery(media_generation);
                    }
                    if display_wake.recovered() {
                        info!("desktop capture recovered after display wake");
                    }
                    media_sequence = 0;
                    latest_idr = None;
                    pending_transition_idr = None;
                    pending_wss_idr = None;
                    pending_viewport_idr = None;
                    encoder = None;
                    scaler = None;
                    next_admission = Instant::now();
                    next_idr = Instant::now();
                    signaling.send_desktop_state("active", media_generation, "capture-recovered");
                    info!(
                        generation = media_generation,
                        "desktop capture recovered; waiting for fresh IDR"
                    );
                }
            }
        }
        let mut secure_announcement = capture.secure_state().map(|state| {
            let active = capture.desktop_state().available;
            (active, state.kind, if active { state.caps } else { vec![] })
        });
        if secure_announcement.is_none() && !capture_available {
            if loop_now >= next_secure_state_poll {
                secure_announcement = bubbleplay_host::desktop_session::secure_desktop_kind()
                    .map(|kind| (false, kind, vec![]));
                next_secure_state_poll = loop_now + capture::CAPTURE_RECOVERY_INTERVAL;
            } else if last_secure_announcement
                .as_ref()
                .is_some_and(|(active, _, _)| !active)
            {
                secure_announcement = last_secure_announcement.clone();
            }
        }
        if secure_announcement != last_secure_announcement {
            if let Some((active, kind, caps)) = &secure_announcement {
                signaling.send_secure_desktop_state_status(*active, kind, caps, media_generation);
                info!(
                    active,
                    kind,
                    ?caps,
                    generation = media_generation,
                    "secure desktop capabilities changed"
                );
            }
            last_secure_announcement = secure_announcement;
        }
        session_stage!(TransportEvents);
        while let Some(event) = pump.try_event() {
            if matches!(event, TransportEvent::PeerReset) {
                rtc_reset_pending = false;
                continue;
            }
            if rtc_reset_pending {
                continue;
            }
            // After an RTC-only teardown the pump's trailing events (Closed,
            // Stopped) belong to the dead peer, not to the WSS session.
            if rtc_peer.ended() {
                continue;
            }
            let rtc_decision = match &event {
                TransportEvent::Action(RtcAction::IceDisconnected) => {
                    ice_connected = false;
                    Some((
                        rtc_peer.on_disconnected(Instant::now(), wss_media.is_some()),
                        "ice-disconnected",
                    ))
                }
                TransportEvent::Action(RtcAction::Closed) => {
                    Some((rtc_peer.on_closed(wss_media.is_some()), "rtc-closed"))
                }
                TransportEvent::Stopped => Some((
                    rtc_peer.on_closed(wss_media.is_some()),
                    "rtc-transport-stopped",
                )),
                _ => None,
            };
            if let Some((decision, reason)) = rtc_decision {
                match decision {
                    RtcPeerDecision::Continue => {
                        warn!(reason, "RTC peer disconnected; waiting for ICE to recover");
                    }
                    RtcPeerDecision::EndPeer => {
                        end_rtc_peer(&pump, &mut ice_connected, &mut pending_candidates, reason);
                    }
                    RtcPeerDecision::EndSession => {
                        park_viewer!(reason);
                    }
                }
                continue;
            }
            match event {
                TransportEvent::Answer(answer) => {
                    signaling.send_answer(&answer)?;
                    offer_in_flight = false;
                    offer_accepted = true;
                }
                TransportEvent::LocalCandidate(candidate) => {
                    signaling.send_candidate(&candidate)?;
                }
                TransportEvent::OfferRejected => {
                    park_viewer!("offer-rejected");
                }
                TransportEvent::Action(RtcAction::InputControlReady) => {
                    if let Some(kind) = latest_cursor_kind {
                        pump.publish_cursor_kind(kind);
                    }
                }
                TransportEvent::Action(RtcAction::AudioReady) => {
                    rtc_audio_requested = true;
                }
                TransportEvent::Action(RtcAction::IceConnected) => {
                    signaling.dtls_connected()?;
                    rtc_peer.on_connected();
                    ice_connected = signaling.application_ready();
                    if ice_connected {
                        ensure!(
                            pump.command(TransportCommand::AuthorizePeer),
                            "RTC transport stopped"
                        );
                    }
                    replay.on_ice_connected();
                    next_idr = Instant::now() + IDR_INTERVAL;
                    if let Some(kind) = latest_cursor_kind {
                        pump.publish_cursor_kind(kind);
                    }
                }
                TransportEvent::Action(RtcAction::VideoCodecSelected(codec)) => {
                    negotiated_video_codec = Some(codec);
                    if codec != active_codec && accept_session_codec(active_codec, codec) {
                        info!(
                            from = active_codec.name(),
                            to = codec.name(),
                            "negotiated codec changed; recreating encoder on next frame"
                        );
                        active_codec = codec;
                        encoder = None;
                        latest_idr = None;
                        startup_idr_admitted = None;
                        startup_idr_submitted = None;
                        if let Some(pending) = pending_first_frame
                            .as_mut()
                            .filter(|pending| pending.transport == "webrtc" && !pending.admitted)
                        {
                            pending.rtc_idr_id = None;
                        }
                        pending_transition_idr = None;
                        next_idr = Instant::now();
                    }
                }
                TransportEvent::Action(RtcAction::KeyframeRequested) => {
                    let now = Instant::now();
                    if now.duration_since(last_keyframe_request) >= KEYFRAME_REQUEST_COOLDOWN {
                        replay.on_keyframe_requested(now);
                        if let Some(enc) = encoder.as_mut() {
                            enc.force_idr();
                        }
                        last_keyframe_request = now;
                    }
                }
                TransportEvent::Action(RtcAction::Bwe(bps))
                    if args.fixed_bitrate_bps.is_none() && wss_media.is_none() =>
                {
                    let clamped = bps.min(args.max_bitrate_bps as u64) as u32;
                    if last_logged_bwe
                        .is_none_or(|previous| previous.abs_diff(clamped) >= 1_000_000)
                    {
                        info!(bwe_bps = clamped, raw_bwe_bps = bps, "RTC BWE estimate");
                        last_logged_bwe = Some(clamped);
                    }
                    if benchmark_lease.active().is_none_or(|profile| {
                        !benchmark_mode_matches(profile.stream_mode, stream_mode)
                    }) {
                        if let Some(d) = quality.bwe_sample(Instant::now(), clamped) {
                            apply_quality_decision(
                                &mut quality,
                                d,
                                &mut profiles,
                                &mut latest_idr,
                                &mut encoder,
                                &mut signaling,
                            );
                        }
                    }
                }
                TransportEvent::Action(RtcAction::InputData(data)) => {
                    if !signaling.application_ready() {
                        continue;
                    }
                    input_sink.note_packet_received(&data);
                    reconcile_carried_input(
                        &mut carried_input,
                        capture_available,
                        remote_input_ready,
                        &mut input_sink,
                    );
                    if !capture_available || !remote_input_ready || !capture.secure_input_allowed()
                    {
                        input_sink.note_packet_drop(if !capture.secure_input_allowed() {
                            InputDropReason::SecurePolicy
                        } else {
                            InputDropReason::NotReady
                        });
                        if let Some(state) = capture.secure_state() {
                            input_sink.note_secure_input_drop(
                                &data,
                                if state.caps.contains(&"input") {
                                    "not_ready"
                                } else {
                                    "no_input_capability"
                                },
                            );
                        }
                        carried_input.observe_gated(&data);
                        continue;
                    }
                    if native_drag_blocks_motion(&data, &mut input_sink) {
                        input_coalescer.observe_release_only(&data);
                        continue;
                    }
                    input_coalescer.observe(&data);
                    carried_input.observe_forwarded(&data);
                    invalidate_pending_drop_input(&mut pending_native_drop, &data);
                    let outcome = session_call!(Input, input_sink.handle_counted(&data));
                    if outcome.sync_requested && capture.desktop_state().desktop != "secure" {
                        if let Some(kind) = cursor_kind_tracker.sample(true, Instant::now()) {
                            let encoded = kind.encode();
                            latest_cursor_kind = Some(encoded);
                            signaling.send_binary(&encoded);
                            pump.publish_cursor_kind(encoded);
                        }
                    }
                    if outcome.cursor_feedback_requested()
                        && capture.desktop_state().desktop != "secure"
                    {
                        if let Some(state) =
                            cursor_tracker.sample(outcome.accepted_sequence, Instant::now())
                        {
                            let encoded = state.encode();
                            if outcome.feedback_reliable {
                                let _ =
                                    pump.command(TransportCommand::CursorStateReliable(encoded));
                            } else {
                                pump.publish_cursor_state(encoded);
                            }
                        }
                    }
                }
                TransportEvent::AccessUnitWritten {
                    id,
                    is_idr: true,
                    rtp_timestamp,
                } => {
                    replay.on_fresh_idr_admitted();
                    if startup_idr_submitted == Some((media_generation, "webrtc", id)) {
                        startup_idr_admitted = Some((media_generation, "webrtc"));
                        startup_idr_rtp = Some((media_generation, rtp_timestamp));
                        startup_idr_submitted = None;
                    }
                    if let Some(pending) = pending_first_frame.as_mut().and_then(|pending| {
                        pending
                            .admit_rtc_access_unit(media_generation, id)
                            .then_some(pending)
                    }) {
                        pending.rtc_rtp_timestamp = Some(rtp_timestamp);
                        // The RTP timestamp lets the browser prove that a
                        // presented frame belongs to this generation before it
                        // enables input (in-place desktop transitions).
                        signaling.send_first_frame_with_rtp(
                            "admitted",
                            &pending.request_id,
                            pending.generation,
                            Some("webrtc"),
                            pending.epoch.as_deref(),
                            Some(active_codec.name()),
                            Some(rtp_timestamp),
                        );
                    }
                    if let Some(pending) = take_matching_transition(&mut pending_transition_idr, id)
                    {
                        let _ = commit_matching_transition(&mut profiles, pending, id);
                    }
                    if let Some((mode, reason)) = profiles.pending_announcement() {
                        signaling.send_quality(
                            mode.label,
                            mode.width,
                            mode.height,
                            mode.fps,
                            encoder
                                .as_ref()
                                .map(VideoEncoder::bitrate)
                                .unwrap_or(quality.target()),
                            quality.bwe(),
                            reason,
                            stream_mode.label(),
                        );
                        profiles.on_idr_admitted();
                    }
                }
                TransportEvent::AccessUnitNotAdmitted { id, is_idr } => {
                    if let Some(pending) = pending_first_frame.as_mut() {
                        pending.reject_rtc_access_unit(id);
                    }
                    if startup_idr_submitted.is_some_and(|(_, _, submitted)| submitted == id) {
                        startup_idr_submitted = None;
                    }
                    let pending =
                        rejected_transition_requires_recovery(&pending_transition_idr, id);
                    if pending {
                        pending_transition_idr = None;
                        latest_idr = None;
                    }
                    let now = Instant::now();
                    // Any rejected AU can break the decoder's reference chain.
                    // Never continue with dependent P-frames: force a fresh IDR
                    // and let the bounded static-replay policy recover it.
                    if let Some(enc) = encoder.as_mut() {
                        enc.force_idr();
                    }
                    next_idr = now;
                    if is_idr || latest_idr.is_some() {
                        replay.on_not_admitted(now);
                    }
                }
                _ => {}
            }
        }
        session_stage!(Foreground);
        let cursor_now = Instant::now();
        // Keep event bursts coalesced until the existing 16 ms cursor cadence
        // allows sampling; a high-polling mouse must not recreate a 1 ms loop.
        let cursor_changed = cursor_now >= next_cursor_event_sample
            && cursor_wake.as_ref().is_ok_and(|wake| wake.take_changed());
        cursor_kind_tracker.set_active(
            capture_available
                && capture.desktop_state().desktop != "secure"
                && (ice_connected || wss_media.is_some()),
        );
        if viewer_attached
            && capture.desktop_state().desktop != "secure"
            && (cursor_changed || cursor_now >= next_cursor_poll)
        {
            if let Some(state) = cursor_tracker.sample(None, cursor_now) {
                v4_pointer_norm = state.in_bounds.then_some((state.x, state.y));
                pump.publish_cursor_state(state.encode());
                if wss_media.is_some() {
                    signaling.send_cursor_state(state.encode(), false);
                }
            }
            if capture_available && capture.desktop_state().desktop != "secure" {
                if let Some(kind) = cursor_kind_tracker.sample(cursor_changed, cursor_now) {
                    let encoded = kind.encode();
                    latest_cursor_kind = Some(encoded);
                    signaling.send_binary(&encoded);
                    pump.publish_cursor_kind(encoded);
                }
            } else {
                cursor_kind_tracker.reset();
            }
            next_cursor_poll = cursor_now
                + Duration::from_millis(
                    if cursor_wake.as_ref().is_ok_and(|wake| wake.available()) {
                        100
                    } else {
                        16
                    },
                );
            next_cursor_event_sample = cursor_now + Duration::from_millis(16);
        }
        viewer_session.observe(viewer_attached && (ice_connected || wss_peer_live));
        admin_demand.observe(viewer_attached)?;
        // Share follow-window's existing cadence even when following is off.
        // Token inspection is cached until foreground HWND/PID changes.
        let foreground_sample_now = Instant::now();
        let foreground_live = foreground_sample_live(
            viewer_attached,
            capture_available,
            capture.desktop_state().desktop,
            ice_connected || wss_peer_live,
        );
        let foreground_sample = if foreground_live && foreground_sample_now >= next_follow_sample {
            next_follow_sample = foreground_sample_now + follow_window::SAMPLE_INTERVAL;
            input_sink.refresh_foreground_status();
            follow_state
                .as_ref()
                .map(|_| follow_window::sample_foreground())
        } else {
            None
        };
        if let Some(follow) = follow_state.as_mut() {
            let now = Instant::now();
            let viewer_live = capture_available
                && capture.desktop_state().desktop != "secure"
                && (ice_connected || wss_media.is_some());
            if !viewer_live {
                edge_monitors = None;
            } else if edge_monitors.is_some() && now >= next_edge_sample {
                // Edge hold while a window is being moved (see
                // `docs/architecture/follow-active-window.md`).
                next_edge_sample = now + follow_window::EDGE_SAMPLE_INTERVAL;
                let moving = follow_window::sample_move();
                let monitors = edge_monitors.as_deref().unwrap_or_default();
                let rects: Vec<follow_window::Rect> =
                    monitors.iter().map(follow_window::Rect::of).collect();
                let current_selector = capture.monitor_selector();
                let current = monitors
                    .iter()
                    .find(|m| m.selector.eq_ignore_ascii_case(&current_selector))
                    .map(follow_window::Rect::of);
                let carry = current.and_then(|from| {
                    let to =
                        follow_window::observe_edge(Some(&mut *follow), moving, &rects, from, now)?;
                    let selector = monitors
                        .iter()
                        .find(|m| follow_window::Rect::of(m) == to)?
                        .selector
                        .clone();
                    Some((from, to, selector, moving?.cursor))
                });
                if moving.is_none() {
                    edge_monitors = None;
                }
                if let Some((from, to, selector, cursor)) = carry {
                    follow.switched(now);
                    info!(
                        monitor = %selector,
                        "follow active window: edge hold, switching monitor"
                    );
                    switch_capture_monitor!(&selector, true);
                    // Same relative position on the new monitor while the
                    // button is still held, so the dragged window comes along.
                    // A cursor already past the edge is on the new monitor: leave it there.
                    if from.contains(cursor)
                        && capture.monitor_selector().eq_ignore_ascii_case(&selector)
                    {
                        let (x, y) = follow_window::carry_cursor(cursor, from, to);
                        if !follow_window::set_cursor(x, y) {
                            warn!("follow active window: edge hold could not move the cursor");
                        }
                    }
                }
            }
            if let Some(sample) = foreground_sample {
                if sample == follow_window::Sample::Busy && edge_monitors.is_none() {
                    // A move loop may have started: sample the edge finely
                    // until it ends. The monitor list is read once per loop.
                    if follow_window::sample_move().is_some() {
                        edge_monitors = capture.monitors().ok();
                        next_edge_sample = now;
                    }
                }
                if let Some(target) = follow.observe(sample, now) {
                    match capture.monitors() {
                        Ok(monitors) => match follow_window::selector_for(&monitors, &target) {
                            Some(selector)
                                if !capture.monitor_selector().eq_ignore_ascii_case(&selector) =>
                            {
                                follow.switched(now);
                                let details = follow.confirmed_details();
                                info!(
                                    monitor = %selector,
                                    window_class = %details.class,
                                    window_exe = %details.exe,
                                    window_pid = details.pid,
                                    "follow active window: switching monitor"
                                );
                                switch_capture_monitor!(&selector);
                            }
                            Some(_) => {}
                            None => warn!(
                                device = %target.device,
                                "follow active window: no capture monitor matches"
                            ),
                        },
                        Err(error) => {
                            warn!(?error, "follow active window: monitor enumeration failed")
                        }
                    }
                }
            }
        }
        session_stage!(Files);
        if file_drag_preview.desired.is_some() || file_drag_preview.active.is_some() {
            file_drag_preview.tick(
                NativeDropView {
                    ready: signaling.application_ready()
                        && file_service.is_some()
                        && bubbleplay_host::clipboard::input_desktop_is_default()
                        && remote_input_ready
                        && capture_available
                        && wss_media.is_some()
                        && pending_first_frame.is_none()
                        && pending_transition_idr.is_none()
                        && pending_viewport_idr.is_none(),
                    epoch: active_wss_epoch.as_deref(),
                    generation: media_generation,
                    revision: viewport_revision,
                },
                capture.region(),
                cursor_now,
            );
        }
        if let Some(files) = file_service.as_ref() {
            // Files use the authenticated viewer epoch in either media mode.
            files.set_epoch(if signaling.application_ready() && viewer_attached {
                signaling.viewer_epoch()
            } else {
                None
            });
            if cursor_now >= next_file_rate_update {
                let cap = wss_media.as_ref().map_or(0, |egress| {
                    file_rate_cap_bps(
                        egress.capacity().map(|capacity| capacity.bps),
                        egress.stats().bitrate_bps,
                    )
                });
                files.set_rate_cap_bps(cap);
                next_file_rate_update = cursor_now + Duration::from_millis(250);
            }
            let outbox = files.take_outbox();
            for text in &outbox.signals {
                signaling.send_text(text);
            }
            let desktop = (!outbox.drops.is_empty() || pending_native_drop.is_some())
                && bubbleplay_host::clipboard::input_desktop_is_default();
            let view = NativeDropView {
                ready: signaling.application_ready()
                    && remote_input_ready
                    && capture_available
                    && wss_media.is_some()
                    && pending_first_frame.is_none()
                    && pending_transition_idr.is_none()
                    && pending_viewport_idr.is_none(),
                epoch: active_wss_epoch.as_deref(),
                generation: media_generation,
                revision: viewport_revision,
            };
            for request in outbox.drops {
                if pending_native_drop.is_some() {
                    request.reject("busy");
                    continue;
                }
                if let Some(reason) = native_drop_view_rejection(&view, desktop, &request) {
                    request.reject(reason);
                    info!(reason, "native file drop rejected");
                    continue;
                }
                let point = native_drop_pixel(request.point.x, request.point.y, capture.region());
                let target = if request.preparation_required() {
                    match bubbleplay_host::native_file_drop::NativeDropTarget::capture(point) {
                        Ok(target) => Some(target),
                        Err(error) => {
                            request.reject("drop-target-changed");
                            info!(%error, reason = "drop-target-changed", "prepared native file drop cancelled");
                            continue;
                        }
                    }
                } else {
                    None
                };
                file_drag_preview.cancel();
                if target.is_some() {
                    info!(
                        bytes = request.total_bytes(),
                        "native file drop preparing verified contents"
                    );
                }
                pending_native_drop = Some(PendingNativeDrop {
                    request,
                    since: Instant::now(),
                    target,
                    input: None,
                });
            }
            if let Some(mut pending) = pending_native_drop.take() {
                let mut reason =
                    native_drop_view_rejection(&view, desktop, &pending.request).map(str::to_owned);
                let preparation = pending.request.preparation_status();
                if let Err(error) = &preparation {
                    reason.get_or_insert_with(|| error.clone());
                }
                if pending
                    .target
                    .as_ref()
                    .is_some_and(|target| !target.matches_current())
                {
                    reason.get_or_insert("drop-target-changed".into());
                }
                let preview_finished = file_drag_preview.finished();
                if !preview_finished && pending.since.elapsed() >= Duration::from_secs(2) {
                    reason.get_or_insert("drop-unavailable".into());
                }
                let mut input_ready = pending.target.is_none();
                if preview_finished && pending.target.is_some() && reason.is_none() {
                    if let Some(input) = pending.input.as_ref() {
                        use bubbleplay_host::native_file_drop::NativeDropInputStatus;
                        match input.poll() {
                            NativeDropInputStatus::Starting => {}
                            NativeDropInputStatus::Clear => input_ready = true,
                            NativeDropInputStatus::Interrupted => {
                                reason = Some("drop-interrupted".into())
                            }
                            NativeDropInputStatus::Unavailable => {
                                reason = Some("drop-unavailable".into())
                            }
                        }
                    } else {
                        match bubbleplay_host::native_file_drop::NativeDropInputGuard::new() {
                            Ok(input) => pending.input = Some(input),
                            Err(_) => reason = Some("drop-unavailable".into()),
                        }
                    }
                }
                if let Some(reason) = reason {
                    pending.request.reject(&reason);
                    info!(%reason, "prepared native file drop cancelled");
                } else if preview_finished && input_ready && preparation == Ok(true) {
                    let point = native_drop_pixel(
                        pending.request.point.x,
                        pending.request.point.y,
                        capture.region(),
                    );
                    let started = if let Some(target) = pending.target {
                        info!(
                            elapsed_ms = pending.since.elapsed().as_millis() as u64,
                            "native file drop prepared; committing to original target"
                        );
                        // Stop category monitoring before our own OLE button bootstrap.
                        drop(pending.input.take());
                        pending.request.start_prepared(target)
                    } else {
                        pending.request.start(point)
                    };
                    if let Err(error) = started {
                        warn!(%error, "native file drop could not start");
                    }
                } else {
                    pending_native_drop = Some(pending);
                }
            }
            for _ in 0..outbox.pastes {
                if signaling.application_ready()
                    && viewer_attached
                    && remote_input_ready
                    && capture_available
                    && capture.desktop_state().desktop != "secure"
                {
                    input_sink.paste();
                } else {
                    info!("file paste skipped: remote input is not ready");
                }
            }
        }
        if cursor_now >= next_input_log {
            input_sink.log_diagnostics();
            let stats = input_sink.take_stats();
            if !stats.is_empty() {
                info!(
                    relative_packets = stats.relative_packets,
                    absolute_packets = stats.absolute_packets,
                    stale_motion_dropped = stats.stale_motion_dropped,
                    cursor_sync_requests = stats.cursor_sync_requests,
                    "remote input"
                );
            }
            next_input_log = cursor_now + Duration::from_secs(1);
        }
        if Instant::now() >= next_ping {
            signaling.send_ping();
            next_ping = Instant::now() + SIGNALING_HEARTBEAT_INTERVAL;
        }
        if args.fixed_bitrate_bps.is_none()
            && wss_media.is_none()
            && Instant::now() >= next_quality_tick
        {
            let now = Instant::now();
            if benchmark_lease
                .active()
                .is_none_or(|profile| !benchmark_mode_matches(profile.stream_mode, stream_mode))
            {
                if let Some(decision) = quality.tick(now) {
                    apply_quality_decision(
                        &mut quality,
                        decision,
                        &mut profiles,
                        &mut latest_idr,
                        &mut encoder,
                        &mut signaling,
                    );
                }
            }
            next_quality_tick = now + Duration::from_secs(1);
        }
        session_stage!(Signaling);
        let mut signal_events = signal_events.into_iter().peekable();
        while let Some(event) = signal_events.next() {
            if signaling.viewer_epoch().is_none() && !matches!(event, SignalEvent::PeerLeft) {
                continue;
            }
            match event {
                SignalEvent::Bound => {
                    signaling.send_host_capabilities(
                        false,
                        false,
                        capture.secure_helper_ready(),
                        capture.secure_helper_ready(),
                    );
                    if let Ok(monitors) = capture.monitors() {
                        signaling.send_monitor_list(monitors);
                    }
                    announce_capture_state(
                        &mut signaling,
                        &mut capture,
                        media_generation,
                        "dtls-bound",
                    );
                    // An endpoint write can end the authenticated session. Do not
                    // authorize transport after a failed state replay.
                    if !signaling.application_ready() {
                        continue;
                    }
                    ice_connected = true;
                    ensure!(
                        pump.command(TransportCommand::AuthorizePeer),
                        "RTC transport stopped"
                    );
                    replay.on_ice_connected();
                    next_idr = Instant::now();
                }
                SignalEvent::Offer(sdp) => {
                    input_sink.reset_status_delivery();
                    if !viewer_attached {
                        begin_viewer_input!();
                        if let Some(egress) = wss_media.take() {
                            egress.shutdown();
                        }
                        active_wss_epoch = None;
                    }
                    viewer_attached = true;
                    display_hold.set_attached(true);
                    // An offer that starts a new peer (including a client's
                    // fallback restart after a desktop transition) keeps
                    // input off until that peer's first-frame handshake
                    // presents.
                    let starts_peer = !rtc_peer.ended()
                        && should_forward_offer(ice_connected, offer_accepted, offer_in_flight);
                    // A repeated offer never turns input back on by itself.
                    remote_input_ready =
                        remote_input_ready && input_ready_on_offer(capture_available, starts_peer);
                    if let Some(kind) = latest_cursor_kind {
                        signaling.send_binary(&kind);
                        pump.publish_cursor_kind(kind);
                    }
                    if starts_peer {
                        carried_input.reset();
                    }
                    signaling.send_host_capabilities(
                        false,
                        false,
                        capture.secure_helper_ready(),
                        capture.secure_helper_ready(),
                    );
                    if let Ok(monitors) = capture.monitors() {
                        signaling.send_monitor_list(monitors);
                    }
                    announce_capture_state(
                        &mut signaling,
                        &mut capture,
                        media_generation,
                        "offer-state-replay",
                    );
                    if rtc_peer.ended() {
                        warn!("ignoring an RTC offer after the RTC peer ended under WSS");
                    } else if should_forward_offer(ice_connected, offer_accepted, offer_in_flight) {
                        let selector = capture.monitor_selector();
                        let adapter = capture::monitor_adapter(Some(&selector))?;
                        let adapter = encoder_adapter(&adapter.cast()?)?;
                        if discovered_adapter != Some(adapter) {
                            discovered_backend =
                                encoder_discovery.resolve(adapter, args.encoder_preference())?;
                            discovered_adapter = Some(adapter);
                            discovery_reason = encoder_discovery
                                .reason(adapter)
                                .unwrap_or_default()
                                .to_owned();
                        }
                        // Answer at master's offer site, without waiting for
                        // a DDA frame or changing NVENC's prewarm codec.
                        if !pump.command(startup_offer_command(sdp, discovered_backend)) {
                            break 'session;
                        }
                        offer_in_flight = true;
                    }
                }
                SignalEvent::FirstFrame {
                    protocol,
                    phase,
                    request_id,
                    transport,
                    epoch,
                    generation,
                    sequence: _,
                    codec,
                    reason: _,
                } => {
                    if protocol != 1
                        || request_id.len() < 8
                        || request_id.len() > 64
                        || !request_id.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
                        })
                    {
                        continue;
                    }
                    match phase.as_str() {
                        "request" => {
                            let Some(transport) =
                                transport.filter(|value| value == "webrtc" || value == "wss")
                            else {
                                continue;
                            };
                            if transport == "wss"
                                && epoch.as_deref().is_none_or(|value| value.len() != 32)
                            {
                                continue;
                            }
                            if (transport == "wss") != wss_media.is_some() {
                                continue;
                            }
                            if transport == "wss" && epoch.as_deref() != active_wss_epoch.as_deref()
                            {
                                continue;
                            }
                            // Requests may precede ICE. A retry of the same
                            // handshake joins/reports its existing IDR.
                            if let Some(pending) = pending_first_frame.as_ref().filter(|pending| {
                                pending.request_id == request_id
                                    && pending.transport == transport
                                    && pending.epoch == epoch
                                    && pending.generation == media_generation
                            }) {
                                if pending.admitted {
                                    signaling.send_first_frame_with_rtp(
                                        "admitted",
                                        &request_id,
                                        pending.generation,
                                        Some(&transport),
                                        epoch.as_deref(),
                                        Some(active_codec.name()),
                                        pending.rtc_rtp_timestamp,
                                    );
                                }
                                continue;
                            }
                            if completed_first_frame.as_ref().is_some_and(
                                |(id, generation, media, completed_epoch)| {
                                    id == &request_id
                                        && *generation == media_generation
                                        && media == &transport
                                        && completed_epoch == &epoch
                                },
                            ) {
                                signaling.send_first_frame(
                                    "ready",
                                    &request_id,
                                    media_generation,
                                    Some(&transport),
                                    epoch.as_deref(),
                                    Some(active_codec.name()),
                                );
                                continue;
                            }
                            let now = Instant::now();
                            viewer_attached = true;
                            display_hold.set_attached(true);
                            let submitted_id = startup_idr_submitted
                                .filter(|(generation, media, _)| {
                                    *generation == media_generation && *media == transport
                                })
                                .map(|(_, _, id)| id);
                            let startup_admitted =
                                startup_idr_admitted.is_some_and(|(generation, media)| {
                                    generation == media_generation && media == transport
                                });
                            let reuse_startup = startup_idr_claimable
                                && ((transport == "webrtc"
                                    && latest_idr.as_ref().is_some_and(|cached| {
                                        cached.belongs_to(media_generation)
                                    }))
                                    || submitted_id.is_some()
                                    || startup_admitted);
                            let admitted = reuse_startup && startup_admitted;
                            let admitted_rtp = startup_idr_rtp
                                .filter(|(generation, _)| {
                                    admitted
                                        && transport == "webrtc"
                                        && *generation == media_generation
                                })
                                .map(|(_, rtp)| rtp);
                            pending_first_frame = Some(PendingFirstFrame {
                                request_id,
                                transport,
                                epoch,
                                generation: media_generation,
                                admitted,
                                rtc_idr_id: submitted_id,
                                rtc_rtp_timestamp: admitted_rtp,
                                armed_at: now,
                                recovery: FirstFrameRecovery::default(),
                            });
                            if admitted {
                                let pending = pending_first_frame.as_ref().expect("request armed");
                                signaling.send_first_frame_with_rtp(
                                    "admitted",
                                    &pending.request_id,
                                    media_generation,
                                    Some(&pending.transport),
                                    pending.epoch.as_deref(),
                                    Some(active_codec.name()),
                                    pending.rtc_rtp_timestamp,
                                );
                            } else if !reuse_startup {
                                latest_idr = None;
                                capture.request_idle_snapshot();
                                if let Some(enc) = encoder.as_mut() {
                                    enc.force_idr();
                                }
                                next_idr = now;
                                next_admission = now;
                            }
                            startup_idr_claimable = false;
                            info!(
                                generation = media_generation,
                                ?codec,
                                "first-frame request armed"
                            );
                        }
                        "presented" => {
                            let Some(pending) = pending_first_frame.as_ref() else {
                                continue;
                            };
                            if pending.request_id != request_id
                                || generation != Some(pending.generation)
                                || !pending.admitted
                            {
                                continue;
                            }
                            let (transport, epoch, codec, generation) = (
                                pending.transport.clone(),
                                pending.epoch.clone(),
                                codec.or_else(|| Some(active_codec.name().to_string())),
                                pending.generation,
                            );
                            signaling.send_first_frame(
                                "ready",
                                &request_id,
                                generation,
                                Some(&transport),
                                epoch.as_deref(),
                                codec.as_deref(),
                            );
                            remote_input_ready = input_ready_after_presentation(
                                capture_available,
                                generation,
                                media_generation,
                            );
                            reconcile_carried_input(
                                &mut carried_input,
                                capture_available,
                                remote_input_ready,
                                &mut input_sink,
                            );
                            completed_first_frame =
                                Some((request_id, generation, transport, epoch));
                            pending_first_frame = None;
                            info!(generation, "first-frame handshake ready");
                        }
                        _ => {}
                    }
                }
                SignalEvent::ClientDiagnostics {
                    seq,
                    at_ms,
                    profile,
                } => info!(seq, at_ms, client_profile = %profile, "client diagnostics"),
                SignalEvent::ClientLog {
                    seq,
                    at_ms,
                    level,
                    event,
                    detail,
                } => match level.as_str() {
                    "error" => {
                        tracing::error!(seq, at_ms, client_event = %event, client_detail = %detail, "client log")
                    }
                    "warn" => {
                        tracing::warn!(seq, at_ms, client_event = %event, client_detail = %detail, "client log")
                    }
                    _ => {
                        info!(seq, at_ms, client_event = %event, client_detail = %detail, "client log")
                    }
                },
                SignalEvent::RemoteCandidate(_) if rtc_peer.ended() => {}
                SignalEvent::RemoteCandidate(c) if offer_accepted => {
                    if !pump.command(TransportCommand::RemoteCandidate(c)) {
                        warn!("RTC transport did not accept a remote candidate (queue full or stopped)");
                    }
                }
                SignalEvent::RemoteCandidate(c) => pending_candidates.push_back(c),
                SignalEvent::FileDragPreview(control) => {
                    if !signaling.application_ready() {
                        continue;
                    }
                    if signaling.viewer_epoch() == Some(control.epoch.as_str())
                        && (pending_native_drop.is_none() || control.action == "cancel")
                    {
                        file_drag_preview.control(control, Instant::now());
                    }
                }
                // Applied at the beginning of this tick, before transport and file work.
                SignalEvent::PeerLeft => {}
                SignalEvent::Qos {
                    seq: _,
                    at_ms: _,
                    rx,
                    presentation,
                } if args.fixed_bitrate_bps.is_none() && wss_media.is_none() => {
                    let backlog = rx
                        .frames_received_delta
                        .zip(rx.frames_decoded_delta)
                        .map(|(r, d)| r as i64 - d as i64);
                    if benchmark_lease.active().is_none_or(|profile| {
                        !benchmark_mode_matches(profile.stream_mode, stream_mode)
                    }) {
                        if let Some(d) = quality.qos_sample(
                            Instant::now(),
                            Qos {
                                loss_pct: rx.loss_pct,
                                packets: rx.packets_received_delta,
                                decode_backlog_growth: backlog,
                                freeze_delta: rx.freeze_delta,
                                admitted_fps: presentation.fps,
                            },
                        ) {
                            apply_quality_decision(
                                &mut quality,
                                d,
                                &mut profiles,
                                &mut latest_idr,
                                &mut encoder,
                                &mut signaling,
                            );
                        }
                    }
                }
                SignalEvent::InputData(data) => {
                    if !signaling.application_ready() {
                        continue;
                    }
                    input_sink.note_packet_received(&data);
                    let mut packets = vec![data];
                    while matches!(signal_events.peek(), Some(SignalEvent::InputData(_))) {
                        if let Some(SignalEvent::InputData(data)) = signal_events.next() {
                            input_sink.note_packet_received(&data);
                            packets.push(data);
                        }
                    }
                    // Coalescing cannot consume sequences or motion behind
                    // the presentation/native-drag gate. Preserve releases
                    // and every other signal event at their original boundary.
                    let packets = if capture_available
                        && remote_input_ready
                        && !bubbleplay_host::native_file_drop::drag_active()
                    {
                        let received = packets.len();
                        let packets = input_coalescer.coalesce(packets);
                        input_sink.note_packets_coalesced(received.saturating_sub(packets.len()));
                        packets
                    } else {
                        packets
                    };
                    for data in packets {
                        if !signaling.application_ready() {
                            break;
                        }
                        reconcile_carried_input(
                            &mut carried_input,
                            capture_available,
                            remote_input_ready,
                            &mut input_sink,
                        );
                        if !capture_available
                            || !remote_input_ready
                            || !capture.secure_input_allowed()
                        {
                            input_sink.note_packet_drop(if !capture.secure_input_allowed() {
                                InputDropReason::SecurePolicy
                            } else {
                                InputDropReason::NotReady
                            });
                            if let Some(state) = capture.secure_state() {
                                input_sink.note_secure_input_drop(
                                    &data,
                                    if state.caps.contains(&"input") {
                                        "not_ready"
                                    } else {
                                        "no_input_capability"
                                    },
                                );
                            }
                            carried_input.observe_gated(&data);
                            continue;
                        }
                        if native_drag_blocks_motion(&data, &mut input_sink) {
                            input_coalescer.observe_release_only(&data);
                            continue;
                        }
                        carried_input.observe_forwarded(&data);
                        invalidate_pending_drop_input(&mut pending_native_drop, &data);
                        let outcome = session_call!(Input, input_sink.handle_counted(&data));
                        if outcome.sync_requested && capture.desktop_state().desktop != "secure" {
                            if let Some(kind) = cursor_kind_tracker.sample(true, Instant::now()) {
                                let encoded = kind.encode();
                                latest_cursor_kind = Some(encoded);
                                signaling.send_binary(&encoded);
                                pump.publish_cursor_kind(encoded);
                            }
                        }
                        if outcome.cursor_feedback_requested()
                            && capture.desktop_state().desktop != "secure"
                        {
                            if let Some(state) =
                                cursor_tracker.sample(outcome.accepted_sequence, Instant::now())
                            {
                                // WSS cursor feedback uses the same ctl channel, with
                                // its original binary bytes sealed as kind cursor.
                                signaling
                                    .send_cursor_state(state.encode(), outcome.feedback_reliable);
                                if outcome.sync_requested {
                                    info!(ack = ?state.acknowledged_sequence, in_bounds = state.in_bounds, os_visible = state.visible, "WSS cursor feedback sent");
                                }
                            }
                        }
                    }
                }
                SignalEvent::SecureDeny => {
                    if !signaling.application_ready() {
                        continue;
                    }
                    let allowed = capture.desktop_state().available
                        && capture.secure_state().is_some_and(|state| {
                            state.available && state.kind == "uac" && state.caps.contains(&"deny")
                        });
                    let queued = allowed
                        && session_call!(
                            SecureHelper,
                            capture.secure_helper().is_some_and(|link| {
                                link.lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .deny_consent()
                            })
                        );
                    if !queued {
                        signaling.send_secure_action_result(
                            false,
                            if allowed {
                                "helper-unavailable"
                            } else {
                                "deny-unavailable"
                            },
                        );
                    }
                }
                SignalEvent::MediaControl {
                    action,
                    request_id,
                    stream_mode: requested_stream_mode,
                    codec,
                    reason,
                    protocol,
                    epoch,
                    quality: requested_quality,
                    revision,
                    enabled,
                    x_q16,
                    y_q16,
                    width_q16,
                    height_q16,
                    monitor,
                    follow,
                    audio,
                } => {
                    if action == "follow" {
                        let Some(enabled) = follow else {
                            warn!("follow control missing boolean follow");
                            continue;
                        };
                        let changed = follow_state.is_some() != enabled;
                        if enabled {
                            follow_state.get_or_insert_with(FollowWindow::default);
                        } else {
                            follow_state = None;
                        }
                        signaling.set_follow(enabled);
                        if changed {
                            next_follow_sample = Instant::now();
                            info!(enabled, "follow active window changed");
                            match capture.monitors() {
                                Ok(monitors) => signaling.send_monitor_list(monitors),
                                Err(error) => warn!(?error, "monitor enumeration failed"),
                            }
                        }
                        continue;
                    }
                    if action == "monitors" {
                        if !valid_monitor_refresh_control(
                            protocol,
                            epoch.as_deref(),
                            active_wss_epoch.as_deref(),
                        ) {
                            warn!(
                                ?protocol,
                                ?epoch,
                                "monitor refresh missing current v4 epoch"
                            );
                            continue;
                        }
                        match capture.monitors() {
                            Ok(monitors) => signaling.send_monitor_list(monitors),
                            Err(error) => warn!(?error, "monitor refresh enumeration failed"),
                        }
                        continue;
                    }
                    if action == "start"
                        && !media_start_epoch_allowed(epoch.as_deref(), signaling.viewer_epoch())
                    {
                        // Only the authenticated session's epoch may start or replace media.
                        warn!(
                            ?epoch,
                            session_epoch = signaling.viewer_epoch(),
                            "media start refused: epoch is not the authenticated session epoch"
                        );
                        continue;
                    }
                    if let Some(next_mode) = StreamMode::parse(requested_stream_mode.as_deref()) {
                        if next_mode != stream_mode {
                            stream_mode = next_mode;
                            quality.set_mode(next_mode);
                            let staged = profiles.set_stream_mode(next_mode);
                            if let Some(enc) = encoder.as_mut() {
                                let transport = if wss_media.is_some() {
                                    BenchmarkTransport::Wss
                                } else {
                                    BenchmarkTransport::Webrtc
                                };
                                let encoder_bitrate_bps = benchmark_owned_encoder_bitrate(
                                    quality.target(),
                                    benchmark_lease.active(),
                                    transport,
                                    stream_mode,
                                    active_codec,
                                );
                                if let Err(error) = enc.set_bitrate(encoder_bitrate_bps) {
                                    warn!(?error, "stream-mode bitrate update failed");
                                }
                                enc.force_idr();
                            }
                            if staged {
                                latest_idr = None;
                                pending_wss_idr = None;
                                pending_transition_idr = None;
                            }
                            next_idr = Instant::now();
                            info!(stream_mode = stream_mode.label(), "stream mode changed");
                        }
                    }
                    let requested = parse_media_codec(codec.as_deref());
                    if action == "monitor" {
                        let Some(selector) = monitor
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty() && value.len() <= 128)
                        else {
                            warn!("monitor control missing bounded selector");
                            continue;
                        };
                        switch_capture_monitor!(selector);
                        continue;
                    }
                    if action == "start" {
                        input_sink.reset_status_delivery();
                        if let Some(requested_epoch) =
                            epoch.as_deref().filter(|value| valid_media_epoch(value))
                        {
                            if viewer_attached
                                && active_wss_epoch
                                    .as_deref()
                                    .is_some_and(|active| active != requested_epoch)
                            {
                                // Replacement can arrive without PeerLeft.
                                // Fence queued/late input on the old RTC peer
                                // before clearing the producer's stale barrier.
                                park_viewer!(@keep_input "wss-epoch-replaced");
                                // Parking stopped the old egress, so the
                                // replacement below does not run; keep its
                                // immediate IDR for the new epoch.
                                next_idr = Instant::now();
                            }
                            if !viewer_attached {
                                begin_viewer_input!(@keep_input);
                            }
                        }
                        viewer_attached = true;
                        display_hold.set_attached(true);
                        remote_input_ready = capture_available;
                        if let Some(requested_epoch) =
                            epoch.as_deref().filter(|value| valid_media_epoch(value))
                        {
                            if should_replace_wss_media_session(
                                wss_media.is_some(),
                                active_wss_epoch.as_deref(),
                                requested_epoch,
                            ) {
                                info!(
                                    previous_epoch = active_wss_epoch.as_deref(),
                                    epoch = requested_epoch,
                                    "replacing stale WSS media egress for new browser epoch"
                                );
                                if let Some(egress) = wss_media.take() {
                                    egress.shutdown();
                                }
                                active_wss_epoch = None;
                                media_generation = media_generation.wrapping_add(1).max(1);
                                media_sequence = 0;
                                latest_idr = None;
                                completed_first_frame = None;
                                startup_idr_claimable = true;
                                startup_idr_admitted = None;
                                startup_idr_submitted = None;
                                pending_first_frame = None;
                                pending_transition_idr = None;
                                pending_wss_idr = None;
                                pending_viewport_idr = None;
                                next_admission = Instant::now();
                                next_idr = Instant::now();
                            }
                        }
                    }
                    if action == "start"
                        && active_wss_epoch.as_deref() == epoch.as_deref()
                        && wss_media.is_some()
                    {
                        // Remember opt-in for this media epoch across repeated starts.
                        wss_audio_requested |= audio && protocol == Some(4);
                        if let Some(egress) = &wss_media {
                            egress.set_audio_enabled(args.audio && wss_audio_requested);
                        }
                    }
                    if action == "start" && wss_media.is_none() {
                        let Some(epoch) = epoch.as_deref().filter(|value| valid_media_epoch(value))
                        else {
                            warn!(?protocol, "WSS media request missing session epoch");
                            continue;
                        };
                        let connect_result = if protocol == Some(4) {
                            // The preset is the controller's ceiling; the
                            // delay controller only ever lowers from it.
                            let preset = wss_preset(
                                requested_quality.as_deref(),
                                args.max_bitrate_bps,
                                stream_mode,
                            );
                            WssMediaEgress::connect_routed(
                                &worker,
                                epoch,
                                u64::from(preset.target_bps),
                                u64::from(preset.target_bps),
                                &relay_route,
                            )
                        } else {
                            Err(anyhow::anyhow!("unsupported WSS media protocol"))
                        };
                        match connect_result {
                            Ok(egress) => {
                                egress.set_activity(activity.clone());
                                wss_audio_requested = audio && protocol == Some(4);
                                egress.set_audio_enabled(args.audio && wss_audio_requested);
                                wss_audio_sink = Some(egress.audio_sink());
                                info!(codec = active_codec.name(), "WSS media egress connected");
                                wss_media = Some(egress);
                                active_wss_epoch = Some(epoch.to_owned());
                                // A browser can reconnect after a previous
                                // secure-desktop transition.  Announce the
                                // current capture state at every media start
                                // so the client cannot retain stale overlay
                                // state across WSS epochs.
                                announce_capture_state(
                                    &mut signaling,
                                    &mut capture,
                                    media_generation,
                                    "wss-session-start",
                                );
                                signaling.send_host_capabilities(
                                    false,
                                    false,
                                    capture.secure_helper_ready(),
                                    capture.secure_helper_ready(),
                                );
                                // WSS-only clients do not necessarily complete
                                // the WebRTC offer path, so announce monitors
                                // here as well as on a normal offer.
                                if let Ok(monitors) = capture.monitors() {
                                    signaling.send_monitor_list(monitors);
                                }
                                let adjustment = wss_preset(
                                    requested_quality.as_deref(),
                                    args.max_bitrate_bps,
                                    stream_mode,
                                );
                                let encoder_bitrate_bps = benchmark_owned_encoder_bitrate(
                                    adjustment.target_bps,
                                    benchmark_lease.active(),
                                    BenchmarkTransport::Wss,
                                    stream_mode,
                                    active_codec,
                                );
                                let staged = apply_wss_adjustment(
                                    &mut quality,
                                    adjustment,
                                    encoder_bitrate_bps,
                                    &mut profiles,
                                    &mut latest_idr,
                                    &mut encoder,
                                    &mut signaling,
                                );
                                if let Some(egress) = wss_media.as_ref() {
                                    egress.set_bitrate_bounds(
                                        u64::from(encoder_bitrate_bps),
                                        u64::from(adjustment.target_bps),
                                    );
                                }
                                if staged {
                                    // Announce intent immediately; a second quality
                                    // message after the IDR ACK confirms activation.
                                    signaling.send_quality(
                                        adjustment.mode.label,
                                        adjustment.mode.width,
                                        adjustment.mode.height,
                                        adjustment.mode.fps,
                                        adjustment.target_bps,
                                        quality.bwe(),
                                        "wss-preset-pending",
                                        stream_mode.label(),
                                    );
                                }
                                next_idr = Instant::now();
                                latest_idr = None;
                            }
                            Err(error) => {
                                warn!(?error, "WSS media egress connection failed");
                            }
                        }
                    }
                    if action == "quality" {
                        let adjustment = wss_preset(
                            requested_quality.as_deref(),
                            args.max_bitrate_bps,
                            stream_mode,
                        );
                        let transport = if wss_media.is_some() {
                            BenchmarkTransport::Wss
                        } else {
                            BenchmarkTransport::Webrtc
                        };
                        let encoder_bitrate_bps = benchmark_owned_encoder_bitrate(
                            adjustment.target_bps,
                            benchmark_lease.active(),
                            transport,
                            stream_mode,
                            active_codec,
                        );
                        let staged = apply_wss_adjustment(
                            &mut quality,
                            adjustment,
                            encoder_bitrate_bps,
                            &mut profiles,
                            &mut latest_idr,
                            &mut encoder,
                            &mut signaling,
                        );
                        if let Some(egress) = wss_media.as_ref() {
                            egress.set_bitrate_bounds(
                                u64::from(encoder_bitrate_bps),
                                u64::from(adjustment.target_bps),
                            );
                        }
                        if staged {
                            signaling.send_quality(
                                adjustment.mode.label,
                                adjustment.mode.width,
                                adjustment.mode.height,
                                adjustment.mode.fps,
                                adjustment.target_bps,
                                quality.bwe(),
                                if wss_media.is_some() {
                                    "wss-preset-pending"
                                } else {
                                    "rtc-preset-pending"
                                },
                                stream_mode.label(),
                            );
                        }
                        next_idr = Instant::now();
                    }
                    if action == "viewport" && wss_media.is_some() {
                        // v4 always streams the full desktop.
                        if !matches!(protocol, Some(2 | 3)) || epoch.is_none() {
                            warn!(?protocol, "WSS viewport request missing session metadata");
                            continue;
                        }
                        let Some(revision) = revision else {
                            continue;
                        };
                        if revision <= viewport_revision {
                            continue;
                        }
                        let crop = match viewport_from_q16(
                            revision,
                            enabled.unwrap_or(false),
                            x_q16.unwrap_or(0),
                            y_q16.unwrap_or(0),
                            width_q16.unwrap_or(u16::MAX),
                            height_q16.unwrap_or(u16::MAX),
                            capture.width(),
                            capture.height(),
                        ) {
                            Some(crop) => crop,
                            None => {
                                warn!(revision, "invalid WSS viewport request");
                                continue;
                            }
                        };
                        let implicit_full = ViewportCrop {
                            revision,
                            x: 0,
                            y: 0,
                            width: capture.width(),
                            height: capture.height(),
                        };
                        let current_crop = viewport_crop.unwrap_or(implicit_full);
                        if pending_viewport_idr.is_none()
                            && same_viewport_geometry(current_crop, crop)
                        {
                            viewport_revision = revision;
                            viewport_crop = Some(crop);
                            let state = if crop.x == 0
                                && crop.y == 0
                                && crop.width == capture.width()
                                && crop.height == capture.height()
                            {
                                "full"
                            } else {
                                "active"
                            };
                            signaling.send_media_view(
                                state,
                                crop.revision,
                                media_generation,
                                capture.width(),
                                capture.height(),
                                crop.x,
                                crop.y,
                                crop.width,
                                crop.height,
                                "viewport-unchanged",
                            );
                            active_viewport = Some((crop.revision, media_generation));
                            info!(
                                revision,
                                generation = media_generation,
                                crop_x = crop.x,
                                crop_y = crop.y,
                                crop_width = crop.width,
                                crop_height = crop.height,
                                "WSS viewport unchanged; acknowledged without IDR"
                            );
                            continue;
                        }
                        viewport_revision = revision;
                        viewport_crop = Some(crop);
                        info!(
                            revision,
                            enabled = enabled.unwrap_or(false),
                            crop_x = crop.x,
                            crop_y = crop.y,
                            crop_width = crop.width,
                            crop_height = crop.height,
                            "WSS viewport prepared"
                        );
                        media_generation = media_generation.wrapping_add(1).max(1);
                        media_sequence = 0;
                        pending_wss_idr = None;
                        pending_viewport_idr = None;
                        latest_idr = None;
                        capture.request_idle_snapshot();
                        if let Some(enc) = encoder.as_mut() {
                            enc.force_idr();
                        }
                        next_idr = Instant::now();
                        signaling.send_media_view(
                            "pending",
                            crop.revision,
                            media_generation,
                            capture.width(),
                            capture.height(),
                            crop.x,
                            crop.y,
                            crop.width,
                            crop.height,
                            reason.as_deref().unwrap_or("sharp-viewport"),
                        );
                    }
                    if matches!(action.as_str(), "start" | "keyframe") {
                        if let Some(requested) = requested {
                            if requested != active_codec
                                && accept_session_codec(active_codec, requested)
                            {
                                info!(
                                    from = active_codec.name(),
                                    to = requested.name(),
                                    ?reason,
                                    "browser requested one-way codec fallback"
                                );
                                active_codec = requested;
                                media_generation = media_generation.wrapping_add(1).max(1);
                                media_sequence = 0;
                                encoder = None;
                                latest_idr = None;
                                pending_transition_idr = None;
                                pending_wss_idr = None;
                                pending_viewport_idr = None;
                            }
                        }
                        // A keyframe request must wake a static desktop too.
                        // Clearing the cached IDR makes the next producer
                        // pass take an idle snapshot instead of waiting for a
                        // new desktop present that may never arrive.
                        if action == "keyframe" {
                            latest_idr = None;
                            if benchmark_lease.active().is_some_and(|profile| {
                                profile.schema_version == 2
                                    && profile.implementation_variant
                                        == benchmark::BenchmarkImplementationVariant::Candidate
                                    && profile.hybrid_mode == BenchmarkHybridMode::PngStaticRoi
                                    && profile.transport == BenchmarkTransport::Wss
                                    && benchmark_mode_matches(profile.stream_mode, stream_mode)
                            }) {
                                hybrid_tile_planner.reset();
                            } else if benchmark_lease.active().is_some_and(|profile| {
                                profile.schema_version == 2
                                    && profile.implementation_variant
                                        == benchmark::BenchmarkImplementationVariant::Candidate
                                    && matches!(
                                        profile.hybrid_mode,
                                        BenchmarkHybridMode::PngQuiescentDetail
                                            | BenchmarkHybridMode::HevcQuiescentTiles
                                    )
                                    && profile.transport == BenchmarkTransport::Wss
                                    && benchmark_mode_matches(profile.stream_mode, stream_mode)
                            }) {
                                quiescent_tile_planner.mark_output_stale();
                            }
                            capture.request_idle_snapshot();
                        }
                        let lab_quality_request = lab_quality_keyframe_request(
                            &action,
                            reason.as_deref(),
                            request_id.as_deref(),
                        );
                        if let Some(request_id) = lab_quality_request {
                            pending_lab_quality_keyframe = Some(request_id.to_owned());
                        }
                        if let Some(enc) = encoder.as_mut() {
                            if should_reset_lab_encoder_for_keyframe(
                                reset_lab_encoder_on_diagnostic_keyframe,
                                &action,
                                reason.as_deref(),
                            ) {
                                if lab_quality_request.is_some() {
                                    enc.reset_rate_control().context(
                                        "resetting Stream Lab encoder rate-control state",
                                    )?;
                                } else {
                                    warn!("ignoring Stream Lab encoder reset without a valid request id");
                                }
                            }
                            enc.force_idr();
                        }
                        next_idr = Instant::now();
                    }
                }
                _ => {}
            }
        }
        signaling.flush_cursor_state();
        let viewer_live = signaling.application_ready()
            && capture_available
            && remote_input_ready
            && (ice_connected || wss_media.is_some());
        // Apply queued RTC and WSS actions before a clipboard completion:
        // a newer press or disconnect must cancel a delayed paste first.
        if viewer_live {
            session_call!(Input, input_sink.finish_clipboard());
        }
        local_clipboard.poll(viewer_live, Instant::now(), &mut input_sink, &mut signaling);
        let audio_wanted = signaling.application_ready()
            && args.audio
            && viewer_attached
            && (wss_audio_requested || rtc_audio_requested);
        if !audio_wanted {
            audio_capture = None;
        }
        if audio_wanted && audio_capture.is_none() && Instant::now() >= next_audio_start {
            match bubbleplay_host::audio::AudioCapture::start(media_clock_start, activity.clone()) {
                Ok(capture) => audio_capture = Some(capture),
                Err(error) => {
                    if !audio_start_failed {
                        warn!(%error, "audio capture thread unavailable; video continues");
                        audio_start_failed = true;
                    }
                    next_audio_start = Instant::now() + Duration::from_secs(1);
                }
            }
        }
        if let Some(capture) = &audio_capture {
            // The capture thread submits directly into a transport's bounded
            // audio queue. Video capture/encoding cannot delay these submissions.
            let sink = if wss_media.is_some() {
                wss_audio_requested
                    .then(|| wss_audio_sink.clone())
                    .flatten()
            } else {
                rtc_audio_requested.then(|| rtc_audio_sink.clone())
            };
            capture.set_sink(sink);
        }
        if viewer_attached {
            let changed = input_sink.take_status_change();
            if let Some(status) = changed {
                last_routing_status = Some(status);
            }
            let diagnostic_due = Instant::now() >= next_admin_diagnostic;
            if changed.is_some() || diagnostic_due {
                next_admin_diagnostic = Instant::now() + Duration::from_millis(250);
                if let Some(status) = last_routing_status {
                    let detail =
                        if status.reason == Some(input::InputStatusReason::AdminUnavailable) {
                            bubbleplay_host::admin_agent::demand::failure_from_env()
                        } else {
                            None
                        };
                    if changed.is_some() || detail != last_admin_failure {
                        signaling.send_input_routing_status(
                            media_generation,
                            status,
                            detail.as_deref(),
                        );
                        last_admin_failure = detail;
                    }
                }
            }
        }
        session_stage!(Media);
        match rtc_peer.poll(Instant::now(), wss_media.is_some()) {
            RtcPeerDecision::Continue => {}
            RtcPeerDecision::EndPeer => {
                end_rtc_peer(
                    &pump,
                    &mut ice_connected,
                    &mut pending_candidates,
                    "wss-during-rtc-grace",
                );
            }
            RtcPeerDecision::EndSession => {
                warn!("RTC peer did not reconnect within the disconnect grace");
                park_viewer!("ice-grace-expired");
            }
        }
        if offer_accepted && !rtc_peer.ended() {
            // Retain an early candidate until the bounded control queue has
            // accepted it; a saturated pump must not silently lose ICE.
            while let Some(candidate) = pending_candidates.front() {
                if !pump.command(TransportCommand::RemoteCandidate(candidate.clone())) {
                    break;
                }
                pending_candidates.pop_front();
            }
        }
        let now = Instant::now();
        let recovering = !capture.desktop_state().available;
        let wake_default = signaling.application_ready()
            && viewer_attached
            && recovering
            && capture.secure_state().is_none()
            && bubbleplay_host::desktop_session::input_desktop_is_default();
        if display_wake.attempt_due(viewer_attached, recovering, wake_default, now) {
            let receipt = input_sink.wake_display();
            info!(applied = receipt.applied, error_code = ?receipt.last_error_code,
                "display wake attempted while desktop capture unavailable");
        }
        if pending_first_frame.as_mut().is_some_and(|pending| {
            pending
                .recovery
                .expired(pending.armed_at, viewer_attached, recovering, now)
        }) {
            if let Some(pending) = pending_first_frame.take() {
                let reason = if pending.admitted {
                    "presentation-timeout"
                } else {
                    "no-idr-admitted"
                };
                signaling.send_first_frame_failed(
                    &pending.request_id,
                    pending.generation,
                    Some(&pending.transport),
                    pending.epoch.as_deref(),
                    Some(active_codec.name()),
                    reason,
                );
                // Name the capture-demand inputs that are false (empty: demand
                // was true), so a failure without demand says why.
                let wss_stats = wss_media.as_ref().map(WssMediaEgress::stats);
                let demand_false_inputs = capture_demand_gaps(
                    viewer_attached,
                    offer_in_flight,
                    offer_accepted,
                    ice_connected,
                    wss_stats.is_some_and(|stats| stats.connected),
                    wss_stats.is_some_and(|stats| stats.peer_present),
                );
                warn!(generation = pending.generation, transport = %pending.transport, reason, demand_false_inputs, "first-frame handshake failed");
            }
        }
        let wss_ready = viewer_attached
            && wss_media.as_ref().is_some_and(|egress| {
                let stats = egress.stats();
                stats.connected && stats.peer_present
            });
        let media_ready = viewer_attached && (ice_connected || wss_ready);
        let wss_v4_events = wss_media
            .as_ref()
            .map(WssMediaEgress::take_events)
            .unwrap_or_default();
        if let Some(end) = &wss_v4_events.link_end {
            match wss_link_end_action(end) {
                WssLinkEndAction::Reregister => {
                    warn!(
                        ?end,
                        epoch = active_wss_epoch.as_deref(),
                        "relay keeps refusing WSS media for this signaling registration; ending the session to re-register for a fresh ticket"
                    );
                    break 'session;
                }
                WssLinkEndAction::AwaitNextStart => {
                    info!(
                        ?end,
                        epoch = active_wss_epoch.as_deref(),
                        "WSS media epoch retired by relay; awaiting the next media start"
                    );
                }
            }
        }
        if relay_ticket_refresh_due(relay_registered_at, viewer_attached, now) {
            info!(
                registered_for_secs = now.saturating_duration_since(relay_registered_at).as_secs(),
                "idle signaling registration nears its relay ticket expiry; re-registering"
            );
            break 'session;
        }
        if let Some(reason) = capture_watchdog.poll(
            capture_demand,
            capture_available,
            pending_first_frame.is_some(),
            latest_idr.is_none(),
            now,
        ) {
            if capture.recreate_stalled_duplication(now) {
                warn!(
                    reason,
                    stall_ms = CAPTURE_STALL_LIMIT.as_millis() as u64,
                    "capture watchdog: no captured frame while one is needed; re-creating the duplication"
                );
                latest_idr = None;
                if let Some(enc) = encoder.as_mut() {
                    enc.force_idr();
                }
                next_idr = now;
            }
        }
        if let Some(reason) = wss_v4_events
            .keyframe
            .as_deref()
            .filter(|_| viewer_attached)
        {
            // The receiver lost (or never had) its reference: encode a fresh
            // IDR even when the desktop is static. v4 never replays a cached
            // IDR, whose sequence would not match the receiver's chain.
            latest_idr = None;
            capture.request_idle_snapshot();
            if let Some(enc) = encoder.as_mut() {
                enc.force_idr();
            }
            next_idr = now;
            wss_v4_key_reason = Some(reason.to_string());
            info!(reason, "WSS v4 keyframe requested");
        }
        if let Some(bitrate_bps) = wss_v4_events.bitrate_bps {
            let bitrate_bps = u32::try_from(bitrate_bps).unwrap_or(u32::MAX);
            if args.fixed_bitrate_bps.is_none() && pending_wss_idr.is_none() {
                let _ = apply_wss_adjustment(
                    &mut quality,
                    WssAdjustment {
                        mode: profiles.active,
                        target_bps: bitrate_bps,
                        reason: "wss-v4-delay",
                    },
                    bitrate_bps,
                    &mut profiles,
                    &mut latest_idr,
                    &mut encoder,
                    &mut signaling,
                );
                info!(
                    bitrate_bps,
                    reason = wss_v4_events.bitrate_reason.unwrap_or("unknown"),
                    "WSS v4 delay controller adjusted bitrate"
                );
            }
        }
        if let Some((generation, sequence)) = wss_v4_events.latest_ack {
            if pending_wss_idr.as_ref().is_some_and(|pending| {
                pending.generation == generation && sequence_reached(sequence, pending.sequence)
            }) {
                let pending = pending_wss_idr.take().expect("checked above");
                if is_fallback_reason(pending.reason) {
                    profiles.pinned = true;
                }
                profiles.commit_mode(pending.mode, pending.reason);
                if let Some((mode, reason)) = profiles.pending_announcement() {
                    signaling.send_quality(
                        mode.label,
                        mode.width,
                        mode.height,
                        mode.fps,
                        encoder
                            .as_ref()
                            .map(VideoEncoder::bitrate)
                            .unwrap_or(quality.target()),
                        quality.bwe(),
                        reason,
                        stream_mode.label(),
                    );
                    profiles.on_idr_admitted();
                }
            }
        }
        if viewer_attached
            && ice_connected
            && wss_media.is_none()
            && replay.is_due(now, latest_idr.is_some())
        {
            if let Some(cached) = latest_idr.as_ref() {
                let expected = profiles
                    .staged
                    .map(|(mode, _)| mode)
                    .unwrap_or(profiles.active);
                if cached.mode != expected || !cached.belongs_to(media_generation) {
                    latest_idr = None;
                    continue 'session;
                }
                if let Some(reservation) = pump.reserve_access_unit() {
                    if !cached.replay_authorized(
                        bubbleplay_host::desktop_session::input_desktop_is_default,
                    ) {
                        latest_idr = None;
                        capture.request_idle_snapshot();
                        if let Some(encoder) = encoder.as_mut() {
                            encoder.force_idr();
                        }
                        next_idr = now;
                        next_admission = now;
                    } else {
                        let id = next_au_id;
                        next_au_id += 1;
                        if reservation.submit(AccessUnit {
                            id,
                            data: cached.data.clone(),
                            produced_at: now,
                            is_idr: true,
                        }) {
                            pending_transition_idr =
                                cached_replay_transition(&profiles, cached.mode, id);
                            replay.on_replayed();
                            startup_idr_submitted = Some((media_generation, "webrtc", id));
                            if let Some(pending) = pending_first_frame.as_mut().filter(|pending| {
                                pending.transport == "webrtc"
                                    && !pending.admitted
                                    && pending.generation == media_generation
                            }) {
                                pending.rtc_idr_id.get_or_insert(id);
                            }
                            next_idr = now + IDR_INTERVAL;
                        }
                    }
                }
            }
        }
        // Reserve the RTC queue only when RTC is active. WSS owns a separately
        // bounded media socket, so it is never gated by an unused RTC slot.
        let rtc_reservation = if viewer_attached && ice_connected && wss_media.is_none() {
            pump.reserve_access_unit()
        } else {
            None
        };
        let negotiation_active =
            signaling.application_ready() && viewer_attached && (offer_in_flight || offer_accepted);
        let prewarm_needed = negotiation_active && !ice_connected && latest_idr.is_none();
        let local_recovery_probe = signaling.application_ready()
            && viewer_attached
            && local_recovery_probe_due(capture_available, now, next_local_recovery_probe);
        if signaling.application_ready()
            && (local_recovery_probe
                || capture_admission_ready(
                    capture_available,
                    negotiation_active,
                    ice_connected,
                    wss_ready,
                    rtc_reservation.is_some(),
                    prewarm_needed,
                ))
        {
            if local_recovery_probe {
                next_local_recovery_probe = now + capture::CAPTURE_RECOVERY_INTERVAL;
            }
            let active_benchmark_profile = benchmark_lease.active().filter(|profile| {
                active_codec == VideoCodec::Hevc
                    && benchmark_mode_matches(profile.stream_mode, stream_mode)
                    && match profile.transport {
                        BenchmarkTransport::Wss => wss_media.is_some(),
                        BenchmarkTransport::Webrtc => wss_media.is_none() && ice_connected,
                    }
            });
            let desired_encoder_benchmark_profile_revision =
                active_benchmark_profile.map(|_| benchmark_profile_revision);
            if encoder_benchmark_contract_changed(
                encoder.is_some(),
                encoder_benchmark_profile_revision,
                desired_encoder_benchmark_profile_revision,
            ) {
                info!(
                    applied_profile_revision = ?encoder_benchmark_profile_revision,
                    desired_profile_revision = ?desired_encoder_benchmark_profile_revision,
                    "effective benchmark transport changed; rebuilding local encoder contract"
                );
                encoder = None;
                scaler = None;
                latest_idr = None;
                pending_transition_idr = None;
                pending_wss_idr = None;
                pending_viewport_idr = None;
                next_admission = Instant::now();
                next_idr = Instant::now();
            }
            session_stage!(Capture);
            let secure_frame_state = capture.secure_state();
            let captured = match capture.acquire(Duration::ZERO)? {
                Some(frame) => Some(frame),
                None if should_snapshot_idle_desktop(media_ready, latest_idr.is_none()) => capture
                    .take_idle_snapshot_frame_with_recovery()
                    .map(|(frame, recovered)| (frame, true, recovered)),
                None => None,
            };
            if let Some((mut frame, is_new, recovered)) = captured {
                if is_new {
                    capture_watchdog.frame_captured();
                }
                if recovered {
                    if let Some(region) = frame.desktop_region() {
                        refresh_input_geometry(
                            &mut input_sink,
                            &mut cursor_tracker,
                            region,
                            Instant::now(),
                        );
                    }
                    capture_available = true;
                    if display_wake.recovered() {
                        info!("desktop capture recovered after display wake");
                    }
                    media_generation = media_generation.wrapping_add(1).max(1);
                    if frame.is_secure_snapshot() {
                        pending_first_frame = None;
                    } else if let Some(pending) = pending_first_frame.as_mut() {
                        pending.rebind_after_recovery(media_generation);
                    }
                    media_sequence = 0;
                    latest_idr = None;
                    pending_transition_idr = None;
                    pending_wss_idr = None;
                    pending_viewport_idr = None;
                    encoder = None;
                    scaler = None;
                    next_admission = Instant::now();
                    next_idr = Instant::now();
                    if frame.is_secure_snapshot() {
                        if let Some(state) = &secure_frame_state {
                            signaling.send_secure_desktop_state_status(
                                true,
                                state.kind,
                                &state.caps,
                                media_generation,
                            );
                            info!(active = true, kind = state.kind, caps = ?state.caps,
                                generation = media_generation, "secure desktop capabilities changed");
                            last_secure_announcement = Some((true, state.kind, state.caps.clone()));
                        }
                    } else {
                        signaling.send_desktop_state_kind(
                            "active",
                            Some("default"),
                            Some("enabled"),
                            media_generation,
                            "capture-recovered",
                        );
                        last_secure_announcement = None;
                    }
                    info!(
                        generation = media_generation,
                        secure_frame = frame.is_secure_snapshot(),
                        desktop = if frame.is_secure_snapshot() {
                            "secure"
                        } else {
                            "default"
                        },
                        "desktop capture recovered from verified frame; waiting for fresh IDR"
                    );
                }
                // Recovery probes are independent of media queue admission.
                // A verified frame may close recovery while the peer has no
                // room for media; keep the fresh-IDR request armed for the
                // next admitted capture without encoding this probe.
                if local_recovery_probe
                    && !capture_admission_ready(
                        true,
                        negotiation_active,
                        ice_connected,
                        wss_media.is_some(),
                        rtc_reservation.is_some(),
                        prewarm_needed,
                    )
                {
                    continue 'session;
                }
                let is_idle_snapshot = frame.is_idle_snapshot();
                // v4 tile quality: accumulate every captured frame's damage
                // until a frame is encoded, so no change is ever missed.
                if wss_media.is_none() || active_benchmark_profile.is_some() {
                    tile_tracker = None;
                    tile_hasher = None;
                    scroll_detector = None;
                    v4_pacer = V4Pacer::default();
                } else {
                    if recovered
                        || tile_tracker
                            .as_ref()
                            .is_some_and(|tracker| !tracker.is_for(frame.width(), frame.height()))
                    {
                        tile_tracker = None;
                        tile_hasher = None;
                        scroll_detector = None;
                        tile_hashing_failed = false;
                    }
                    let tracker = tile_tracker.get_or_insert_with(|| {
                        TileQualityTracker::new(now, frame.width(), frame.height())
                    });
                    if is_new && !is_idle_snapshot {
                        frame.prepare_damage_union();
                        tracker.note_damage(now, frame.damage());
                        v4_pacer.on_capture(
                            now,
                            frame.width(),
                            frame.height(),
                            frame.damage(),
                            frame.moved(),
                        );
                    }
                }
                let dev = frame.device().as_raw();
                if discovered_backend == EncoderBackend::MfSoftware {
                    if recovered {
                        cpu_damage.invalidate();
                    }
                    if is_new {
                        frame.prepare_damage_union();
                        cpu_damage.note((frame.width(), frame.height()), frame.damage());
                    }
                }
                // WSS v4 owns keyframes (start / peer / request), so its
                // encoder runs without periodic IDRs. NVENC cannot change the
                // GOP in place: recreate the session when the mode differs.
                let transport_infinite_gop =
                    wss_media.is_some() && active_benchmark_profile.is_none();
                let mut want_infinite_gop = transport_infinite_gop
                    || encoder.as_ref().is_some_and(|e| {
                        e.is_for_device(dev) && e.backend() == EncoderBackend::MfSoftware
                    });
                if encoder
                    .as_ref()
                    .is_some_and(|e| e.is_for_device(dev) && e.infinite_gop() != want_infinite_gop)
                {
                    info!(
                        infinite_gop = want_infinite_gop,
                        "recreating encoder for the WSS v4 GOP mode"
                    );
                    encoder = None;
                    latest_idr = None;
                    pending_transition_idr = None;
                    next_idr = Instant::now();
                }
                if encoder.as_ref().is_none_or(|e| !e.is_for_device(dev)) {
                    let adapter = device_encoder_adapter(dev)?;
                    if discovered_adapter != Some(adapter) {
                        discovered_backend =
                            encoder_discovery.resolve(adapter, args.encoder_preference())?;
                        discovered_adapter = Some(adapter);
                        discovery_reason = encoder_discovery
                            .reason(adapter)
                            .unwrap_or_default()
                            .to_owned();
                    }
                    if encoder.is_some() {
                        warn!("capture D3D11 device changed; invalidating scaler and IDR state");
                        scaler = None;
                        latest_idr = None;
                        pending_transition_idr = None;
                        next_idr = Instant::now();
                    }
                    let mode =
                        encoder_mode_for_capture(&mut profiles, frame.width(), frame.height());
                    want_infinite_gop =
                        transport_infinite_gop || discovered_backend == EncoderBackend::MfSoftware;
                    let created = create_local_encoder(
                        args.encoder_preference(),
                        discovered_backend,
                        &mut discovery_reason,
                        dev,
                        mode.width,
                        mode.height,
                        frame.width(),
                        frame.height(),
                        mode.fps,
                        quality.target(),
                        preset,
                        active_codec,
                        active_benchmark_profile,
                        want_infinite_gop,
                    )?;
                    adopt_encoder_codec(&mut active_codec, Some(&created));
                    discovered_backend = created.backend();
                    encoder_discovery.record_actual(adapter, discovered_backend, &discovery_reason);
                    encoder = Some(created);
                    encoder_benchmark_profile_revision = desired_encoder_benchmark_profile_revision;
                }
                ensure_rtc_encoder_codec(
                    discovered_backend,
                    wss_media.is_some(),
                    negotiated_video_codec,
                    active_codec,
                )?;
                // An H.264-only backend disables HEVC benchmark leases before
                // preparing textures or reporting a benchmark contract.
                let active_benchmark_profile = active_benchmark_profile.filter(|_| {
                    encoder
                        .as_ref()
                        .is_some_and(|encoder| encoder.capabilities().hevc)
                });
                let main10 = main10_conversion(
                    active_benchmark_profile.is_some_and(|profile| profile.encoder.main10),
                    encoder
                        .as_ref()
                        .map_or(EncoderCapabilities::MF, VideoEncoder::capabilities),
                );
                let old_mode = profiles.active;
                let mut mode = profiles.staged.map(|(m, _)| m).unwrap_or(old_mode);
                let cpu_conversion = encoder.as_ref().is_some_and(VideoEncoder::cpu_conversion);
                if cpu_conversion && viewport_crop.is_none() {
                    if cpu_applied_mode != Some(mode) {
                        cpu_requested_mode = Some(mode);
                    }
                    let requested = cpu_requested_mode.unwrap_or(mode);
                    let target = cpu_pacing.target(
                        (frame.width(), frame.height()),
                        CpuTarget {
                            width: requested.width,
                            height: requested.height,
                            fps: requested.fps,
                        },
                        Instant::now(),
                    );
                    let label = if (target.width, target.height, target.fps)
                        == (requested.width, requested.height, requested.fps)
                    {
                        requested.label
                    } else {
                        match (target.height, target.fps) {
                            (720, 60) => "720p60",
                            (720, 30) => "720p30",
                            (540, 30) => "540p30",
                            (540, 60) => "540p60",
                            (540, 24) => "540p24",
                            (480, 24) => "480p24",
                            _ => "native",
                        }
                    };
                    let capped = VideoMode {
                        label,
                        width: target.width,
                        height: target.height,
                        fps: target.fps,
                        ..requested
                    };
                    cpu_applied_mode = Some(capped);
                    if capped != mode {
                        if (capped.width, capped.height, capped.fps)
                            != (old_mode.width, old_mode.height, old_mode.fps)
                        {
                            info!(
                                width = capped.width,
                                height = capped.height,
                                fps = capped.fps,
                                previous_width = old_mode.width,
                                previous_height = old_mode.height,
                                previous_fps = old_mode.fps,
                                "CPU output target changed; scheduling IDR"
                            );
                        }
                        mode = capped;
                        // CPU scaling has its own trustworthy texture path, so
                        // the GPU scaler's pinned native fallback is irrelevant.
                        profiles.staged = (capped != old_mode).then_some((capped, "cpu-pressure"));
                    }
                }
                let transition = profiles.staged().is_some();
                let encoder_resource_mode = active_benchmark_profile.map_or(
                    ResourceMode::PerFrame,
                    |profile| match profile.encoder.resource_mode {
                        BenchmarkResourceMode::PerFrame => ResourceMode::PerFrame,
                        BenchmarkResourceMode::Persistent => ResourceMode::Persistent,
                    },
                );
                let frame_interval = Duration::from_secs_f64(1.0 / (mode.fps as f64).max(1.0));
                // A waiter result can arrive after the loop's earlier clock
                // sample. Admit against its actual consumption time.
                let now = Instant::now();
                let urgent = latest_idr.is_none() || transition;
                if (is_new && frame_admission_due(now, next_admission, frame_interval, urgent))
                    || (!is_new && urgent)
                {
                    frame.prepare_damage();
                    // v4 tile quality: hash the dirty tiles (damage-sized
                    // readback), skip a repaint that changes nothing the
                    // receiver shows, and plan a refine of settled debt tiles.
                    let mut tile_hashes: Vec<(usize, Option<u64>)> = Vec::new();
                    let mut tile_changed: Vec<usize> = Vec::new();
                    let mut wss_v4_refine_plan: Option<RefinePlan> = None;
                    let v4_pointer = v4_pointer_norm
                        .map(|(x, y)| pointer_pixels(x, y, frame.width(), frame.height()));
                    if let Some(tracker) = tile_tracker.as_mut() {
                        let dirty = tracker.dirty_tiles();
                        let mut hashed = None;
                        // Damage too large to hash in full: read back a few
                        // columns only, for scroll detection.
                        let hash_all = tracker.dirty_per_mille() <= MAX_HASHED_PER_MILLE;
                        let to_hash = if hash_all {
                            dirty.clone()
                        } else {
                            sample_tiles(tracker.grid(), &dirty)
                        };
                        let mut row_hashes = None;
                        if !to_hash.is_empty() && !tile_hashing_failed && frame.damage().is_some() {
                            if tile_hasher.as_ref().is_none_or(|hasher| {
                                !hasher.is_for(frame.device(), frame.texture())
                            }) {
                                tile_hasher = match TileHasher::new(frame.device(), frame.texture())
                                {
                                    Ok(hasher) => Some(hasher),
                                    Err(error) => {
                                        warn!(
                                            ?error,
                                            "tile hashing unavailable; no-op repaints are encoded"
                                        );
                                        tile_hashing_failed = true;
                                        None
                                    }
                                };
                            }
                            if let Some(hasher) = tile_hasher.as_mut() {
                                match hasher.hash_tiles(frame.texture(), tracker.grid(), &to_hash) {
                                    Ok((hashes, rows, elapsed_us)) => {
                                        tile_hash_us = tile_hash_us.saturating_add(elapsed_us);
                                        if hash_all && !hashes.is_empty() {
                                            hashed = Some(hashes);
                                        }
                                        if !rows.is_empty() {
                                            row_hashes = Some(rows);
                                        }
                                    }
                                    Err(error) => {
                                        warn!(
                                            ?error,
                                            "tile hashing failed; no-op repaints are encoded"
                                        );
                                        tile_hasher = None;
                                        tile_hashing_failed = true;
                                    }
                                }
                            }
                        }
                        if let Some(rows) = row_hashes {
                            let grid = tracker.grid();
                            let detector = match scroll_detector.as_mut() {
                                Some(detector) if detector.is_for(grid) => detector,
                                _ => scroll_detector.insert(ScrollDetector::new(grid)),
                            };
                            if let Some(found) = detector.detect(&rows) {
                                scroll_detections += 1;
                                scroll_dy = found.dy;
                                v4_pacer
                                    .on_scrolled(now, &extend_to_runs(grid, &dirty, &found.tiles));
                            }
                        }
                        tile_hashes = hashed
                            .unwrap_or_else(|| dirty.iter().map(|&tile| (tile, None)).collect());
                        let classification = tracker.classify(&tile_hashes);
                        tile_changed = classification.changed;
                        let key_due =
                            urgent || encoder.as_ref().is_some_and(VideoEncoder::idr_pending);
                        v4_pacer.begin_frame(
                            now,
                            wss_media.as_ref().and_then(|egress| egress.capacity()),
                            tile_changed.is_empty(),
                        );
                        // Changed tiles go in any case; a refine is paid from
                        // the budget they leave (constrained link only).
                        let mut refine_wait = Duration::ZERO;
                        if !key_due {
                            if let Some(plan) = tracker.refine_plan(now) {
                                wss_v4_refine_plan = v4_pacer.fit_refine(
                                    &tracker.grid(),
                                    plan,
                                    v4_pointer,
                                    !tile_changed.is_empty(),
                                    encoder
                                        .as_ref()
                                        .is_some_and(VideoEncoder::refine_qp_map_enabled),
                                );
                                if wss_v4_refine_plan.is_none() {
                                    refine_wait = v4_pacer.refine_retry_after();
                                }
                            }
                        }
                        if is_new
                            && !is_idle_snapshot
                            && !key_due
                            && wss_v4_refine_plan.is_none()
                            && classification.noop
                        {
                            tracker.on_noop_skipped();
                            if let Some(at) = tracker.next_refine_at() {
                                capture.request_idle_followup_snapshot(
                                    at.saturating_duration_since(Instant::now())
                                        .max(refine_wait),
                                );
                            }
                            continue 'session;
                        }
                        // Text-dominant on a constrained link: hold changes
                        // while the budget is spent (never past 100 ms).
                        if let PaceDecision::Skip { retry_after } = v4_pacer.pace(now, key_due) {
                            capture.request_idle_followup_snapshot(retry_after);
                            continue 'session;
                        }
                    }
                    if let Some(egress) = wss_media.as_ref() {
                        egress.set_frame_size(mode.width, mode.height);
                    }
                    let mut wss_reservation = None;
                    let mut wss_forced_idr = false;
                    let mut wss_v4_key_cause = if transition {
                        "transition"
                    } else if latest_idr.is_none() {
                        "start"
                    } else {
                        "forced"
                    };
                    if let Some(egress) = wss_media.as_ref() {
                        let mut wants_key = latest_idr.is_none() || transition;
                        let base_meta = WssAccessUnitMeta {
                            codec: wss_codec(active_codec),
                            generation: media_generation,
                            sequence: media_sequence.wrapping_add(1),
                            timestamp90k: (media_clock_start.elapsed().as_secs_f64() * 90_000.0)
                                as u32,
                            key: wants_key,
                        };
                        let reservation: Result<_, WssReserveOutcome> = match egress
                            .reserve(base_meta)
                        {
                            Ok(reservation) => Ok(reservation),
                            Err(WssReserveOutcome::RequireKeyframe) if !wants_key => {
                                wants_key = true;
                                wss_v4_key_cause = "link";
                                encoder.as_mut().unwrap().force_idr();
                                match egress.reserve(WssAccessUnitMeta {
                                    key: true,
                                    ..base_meta
                                }) {
                                    Ok(reservation) => Ok(reservation),
                                    Err(
                                        WssReserveOutcome::Backpressured
                                        | WssReserveOutcome::RequireKeyframe,
                                    ) => {
                                        wss_preencode_skips = wss_preencode_skips.saturating_add(1);
                                        benchmark_skips = benchmark_skips.saturating_add(1);
                                        wss_preencode_skips_total =
                                            wss_preencode_skips_total.saturating_add(1);
                                        capture.request_idle_followup_snapshot(frame_interval);
                                        continue 'session;
                                    }
                                    Err(WssReserveOutcome::Stopped) => {
                                        warn!(
                                            "WSS media egress stopped during keyframe reservation"
                                        );
                                        continue 'session;
                                    }
                                }
                            }
                            Err(WssReserveOutcome::Backpressured) => {
                                wss_preencode_skips = wss_preencode_skips.saturating_add(1);
                                benchmark_skips = benchmark_skips.saturating_add(1);
                                wss_preencode_skips_total =
                                    wss_preencode_skips_total.saturating_add(1);
                                capture.request_idle_followup_snapshot(frame_interval);
                                continue 'session;
                            }
                            Err(WssReserveOutcome::RequireKeyframe) => {
                                wss_preencode_skips = wss_preencode_skips.saturating_add(1);
                                benchmark_skips = benchmark_skips.saturating_add(1);
                                wss_preencode_skips_total =
                                    wss_preencode_skips_total.saturating_add(1);
                                capture.request_idle_followup_snapshot(frame_interval);
                                continue 'session;
                            }
                            Err(WssReserveOutcome::Stopped) => {
                                warn!("WSS media egress stopped during pre-encode reservation");
                                continue 'session;
                            }
                        };
                        let reservation = match reservation {
                            Ok(reservation) => reservation,
                            Err(_) => unreachable!("all WSS reservation errors are handled above"),
                        };
                        if wants_key {
                            encoder.as_mut().unwrap().force_idr();
                            wss_forced_idr = true;
                        }
                        wss_reservation = Some(reservation);
                    }
                    // v4 refine: settled debt tiles ride on this P frame
                    // (lower QP through the delta-QP map, or the whole frame
                    // without one), flagged `refine` on the wire.
                    if wss_forced_idr
                        || latest_idr.is_none()
                        || transition
                        || encoder.as_ref().is_some_and(VideoEncoder::idr_pending)
                    {
                        wss_v4_refine_plan = None;
                    }
                    let wss_v4_refine_frame = wss_v4_refine_plan.is_some();
                    // A fallback is a separate transaction: first prepare a
                    // texture for its exact dimensions, then recreate the encoder
                    // for that same mode, and only commit after its IDR is
                    // admitted by the transport.
                    let mut fallback_reason = None;
                    let mut scaled = match if cpu_conversion && viewport_crop.is_none() {
                        Ok(cpu_mode_texture(&frame, mode))
                    } else if viewport_crop.is_some() && mode.label != "native" {
                        prepare_profile_texture_with_crop(
                            &mut scaler,
                            frame.device(),
                            frame.texture(),
                            frame.width(),
                            frame.height(),
                            mode.profile,
                            viewport_crop,
                            main10,
                        )
                    } else {
                        prepare_video_mode_texture(
                            &mut scaler,
                            frame.device(),
                            frame.texture(),
                            frame.width(),
                            frame.height(),
                            mode,
                            main10,
                            encoder_resource_mode,
                        )
                    } {
                        Ok(output) => output,
                        Err(error) => {
                            warn!(
                                ?error,
                                target = mode.label,
                                "target texture preparation failed; recovering native mode"
                            );
                            mode = fallback_mode(old_mode, frame.width(), frame.height(), false);
                            fallback_reason = Some("scaler-fallback");
                            match prepare_video_mode_texture(
                                &mut scaler,
                                frame.device(),
                                frame.texture(),
                                frame.width(),
                                frame.height(),
                                mode,
                                main10,
                                encoder_resource_mode,
                            ) {
                                Ok(output) => output,
                                Err(native_error) => {
                                    warn!(?native_error, "fallback texture preparation failed; retaining session for retry");
                                    latest_idr = None;
                                    pending_transition_idr = None;
                                    next_idr = now;
                                    continue 'session;
                                }
                            }
                        }
                    };
                    if transition
                        && fallback_reason.is_none()
                        && configure_encoder_profile(
                            &mut encoder,
                            args.encoder_preference(),
                            &mut discovered_backend,
                            &mut discovery_reason,
                            dev,
                            mode,
                            frame.width(),
                            frame.height(),
                            quality.target(),
                            preset,
                            active_codec,
                            active_benchmark_profile,
                        )
                        .is_err()
                    {
                        warn!(
                            target = mode.label,
                            "transition reconfigure failed; recovering last working mode"
                        );
                        mode = fallback_mode(old_mode, frame.width(), frame.height(), true);
                        fallback_reason = Some("encoder-fallback");
                    }
                    adopt_encoder_codec(&mut active_codec, encoder.as_ref());
                    if transition {
                        if let Some(adapter) = discovered_adapter {
                            encoder_discovery.record_actual(
                                adapter,
                                discovered_backend,
                                &discovery_reason,
                            );
                        }
                    }
                    ensure_rtc_encoder_codec(
                        discovered_backend,
                        wss_media.is_some(),
                        negotiated_video_codec,
                        active_codec,
                    )?;
                    if (main10
                        && encoder
                            .as_ref()
                            .is_some_and(|encoder| !encoder.capabilities().main10))
                        || (active_benchmark_profile.is_some() && active_codec != VideoCodec::Hevc)
                    {
                        // A reconfiguration can replace NVENC with MF after
                        // this pass already prepared P010 or benchmark state.
                        // Start again with the actual encoder's 8-bit contract.
                        latest_idr = None;
                        pending_transition_idr = None;
                        pending_wss_idr = None;
                        pending_viewport_idr = None;
                        next_admission = now;
                        next_idr = now;
                        if let Some(encoder) = encoder.as_mut() {
                            encoder.force_idr();
                        }
                        capture.request_idle_followup_snapshot(Duration::ZERO);
                        continue 'session;
                    }
                    if fallback_reason.is_some() {
                        // The target texture is never passed to the fallback
                        // encoder, even if both textures happen to share a
                        // device.  That contract prevents dimension drift.
                        scaled = match if cpu_conversion && viewport_crop.is_none() {
                            Ok(cpu_mode_texture(&frame, mode))
                        } else {
                            prepare_video_mode_texture(
                                &mut scaler,
                                frame.device(),
                                frame.texture(),
                                frame.width(),
                                frame.height(),
                                mode,
                                main10,
                                encoder_resource_mode,
                            )
                        } {
                            Ok(output) => output,
                            Err(error) => {
                                warn!(
                                    ?error,
                                    "scaled fallback unavailable; recovering exact native mode"
                                );
                                mode =
                                    fallback_mode(old_mode, frame.width(), frame.height(), false);
                                fallback_reason = Some("scaler-fallback");
                                match if cpu_conversion && viewport_crop.is_none() {
                                    Ok(cpu_mode_texture(&frame, mode))
                                } else {
                                    prepare_video_mode_texture(
                                        &mut scaler,
                                        frame.device(),
                                        frame.texture(),
                                        frame.width(),
                                        frame.height(),
                                        mode,
                                        main10,
                                        encoder_resource_mode,
                                    )
                                } {
                                    Ok(output) => output,
                                    Err(native_error) => {
                                        warn!(?native_error, "native fallback texture preparation failed; retaining session for retry");
                                        latest_idr = None;
                                        pending_transition_idr = None;
                                        next_idr = now;
                                        continue 'session;
                                    }
                                }
                            }
                        };
                    }
                    if fallback_reason.is_none()
                        && !wss_forced_idr
                        && (periodic_idr_due(now, next_idr, wss_media.is_some())
                            || latest_idr.is_none()
                            || transition)
                    {
                        encoder.as_mut().unwrap().force_idr();
                        next_idr = now + IDR_INTERVAL;
                    }
                    // Benchmark ROI and HEVC tile features need an NVENC-only
                    // capability; another backend leaves them off for this frame.
                    let encoder_caps = encoder
                        .as_ref()
                        .map_or(EncoderCapabilities::MF, VideoEncoder::capabilities);
                    let static_hybrid_enabled = active_benchmark_profile.is_some_and(|profile| {
                        profile.schema_version == 2
                            && profile.implementation_variant
                                == benchmark::BenchmarkImplementationVariant::Candidate
                            && profile.hybrid_mode == BenchmarkHybridMode::PngStaticRoi
                            && !profile.encoder.main10
                            && wss_media.is_some()
                    });
                    let feedback_roi_enabled = encoder_caps.roi_qp_map
                        && encoder_caps.refine_feedback
                        && active_benchmark_profile.is_some_and(|profile| {
                            profile.schema_version == 2
                                && profile.implementation_variant
                                    == benchmark::BenchmarkImplementationVariant::Candidate
                                && matches!(
                                    profile.effective_roi_mode(),
                                    BenchmarkRoiMode::Feedback | BenchmarkRoiMode::FeedbackHeadroom
                                )
                                && !profile.encoder.main10
                                && wss_media.is_some()
                                && stream_mode == StreamMode::Work
                        });
                    let feedback_headroom_enabled =
                        active_benchmark_profile.is_some_and(|profile| {
                            feedback_roi_enabled
                                && profile.effective_roi_mode()
                                    == BenchmarkRoiMode::FeedbackHeadroom
                        });
                    let hevc_quiescent_tiles_enabled = encoder_caps.hevc_tiles
                        && active_benchmark_profile.is_some_and(|profile| {
                            profile.schema_version == 2
                                && profile.implementation_variant
                                    == benchmark::BenchmarkImplementationVariant::Candidate
                                && profile.hybrid_mode == BenchmarkHybridMode::HevcQuiescentTiles
                                && !profile.encoder.main10
                                && wss_media.is_some()
                                && stream_mode == StreamMode::Work
                        });
                    let quiescent_hybrid_enabled =
                        active_benchmark_profile.is_some_and(|profile| {
                            profile.schema_version == 2
                                && profile.implementation_variant
                                    == benchmark::BenchmarkImplementationVariant::Candidate
                                && matches!(
                                    profile.hybrid_mode,
                                    BenchmarkHybridMode::PngQuiescentDetail
                                        | BenchmarkHybridMode::HevcQuiescentTiles
                                )
                                && !profile.encoder.main10
                                && wss_media.is_some()
                                && stream_mode == StreamMode::Work
                        });
                    if !hevc_quiescent_tiles_enabled {
                        hevc_tile_encoder = None;
                        hevc_tile_encoder_failed_key = None;
                    }
                    let quiescent_cache_mode = active_benchmark_profile
                        .map_or(BenchmarkHybridCacheMode::SuppressReactivate, |profile| {
                            profile.hybrid_cache_mode
                        });
                    let quiescent_suppress_reactivate = quiescent_hybrid_enabled
                        && quiescent_cache_mode == BenchmarkHybridCacheMode::SuppressReactivate;
                    let hybrid_enabled = static_hybrid_enabled || quiescent_hybrid_enabled;
                    let mut hybrid_temporal_result = None;
                    let mut quiescent_temporal_result = None;
                    let mut quiescent_temporal_submission = None;
                    let mut quiescent_suppression_operations = Vec::new();
                    let mut quiescent_resolution_operations = Vec::new();
                    let mut quiescent_verified_changed_rects = Vec::new();
                    let quiescent_viewport = quiescent_hybrid_enabled.then(|| {
                        let source = viewport_crop.map_or(
                            frame_damage::DamageRect {
                                left: 0,
                                top: 0,
                                right: frame.width(),
                                bottom: frame.height(),
                            },
                            |crop| frame_damage::DamageRect {
                                left: crop.x,
                                top: crop.y,
                                right: crop.x + crop.width,
                                bottom: crop.y + crop.height,
                            },
                        );
                        ViewportGeometry {
                            generation: media_generation,
                            crop_revision: viewport_revision,
                            source,
                            output_width: scaled.width,
                            output_height: scaled.height,
                        }
                    });
                    if quiescent_hybrid_enabled {
                        if quiescent_diff_revision != benchmark_profile_revision {
                            quiescent_diff_revision = benchmark_profile_revision;
                            quiescent_diff_analyzer = None;
                            quiescent_tile_planner.reset();
                            quiescent_wire_budget.reset();
                        }
                        let analyzer_matches =
                            quiescent_diff_analyzer.as_ref().is_some_and(|analyzer| {
                                analyzer.is_for(frame.device(), frame.width(), frame.height())
                            });
                        if !analyzer_matches {
                            quiescent_diff_analyzer = TemporalDiffAnalyzer::new(
                                frame.device(),
                                frame.width(),
                                frame.height(),
                            )
                            .ok();
                        }
                        let mut submission_failed = false;
                        if let Some(analyzer) = quiescent_diff_analyzer.as_mut() {
                            let validation_requests = if quiescent_suppress_reactivate {
                                quiescent_validation_requests(
                                    &quiescent_tile_planner,
                                    quiescent_viewport.expect("quiescent viewport is available"),
                                    media_sequence.wrapping_add(1),
                                    frame.damage(),
                                    is_idle_snapshot,
                                )
                            } else {
                                Vec::new()
                            };
                            match analyzer.submit_quiescent(
                                frame.texture(),
                                &validation_requests,
                                frame.damage(),
                            ) {
                                Ok(submission) => {
                                    if quiescent_suppress_reactivate {
                                        quiescent_suppression_operations = quiescent_tile_planner
                                            .suppress_requests(&submission.accepted_requests);
                                    }
                                    if let Some(activity) = benchmark_hybrid_activity.as_mut() {
                                        activity.cached_bytes_retained =
                                            activity
                                                .cached_bytes_retained
                                                .max(quiescent_tile_planner.retained_png_bytes()
                                                    as u64);
                                    }
                                    quiescent_temporal_submission = Some(submission);
                                }
                                Err(error) => {
                                    submission_failed = true;
                                    warn!(
                                        ?error,
                                        "GPU quiescent-detail validation submission failed"
                                    );
                                }
                            }
                        }
                        if submission_failed {
                            quiescent_diff_analyzer = None;
                            quiescent_tile_planner.reset();
                        }
                    }
                    let feedback_key = (
                        benchmark_profile_revision,
                        media_generation,
                        viewport_revision,
                        mode,
                    );
                    if feedback_roi_enabled {
                        let analyzer_mismatch = reconstruction_feedback_analyzer
                            .as_ref()
                            .is_some_and(|analyzer| {
                                !analyzer.is_for(frame.device(), scaled.width, scaled.height)
                            });
                        if reconstruction_feedback_key != Some(feedback_key) || analyzer_mismatch {
                            reconstruction_feedback_analyzer = None;
                            feedback_roi_planner = None;
                            feedback_wire_budget = None;
                            reconstruction_feedback_key = Some(feedback_key);
                            reconstruction_feedback_failed_key = None;
                        }
                        if reconstruction_feedback_analyzer.is_none()
                            && reconstruction_feedback_failed_key != Some(feedback_key)
                        {
                            match ReconstructionFeedbackAnalyzer::new(
                                frame.device(),
                                scaled.width,
                                scaled.height,
                            )
                            .and_then(|analyzer| {
                                Ok((
                                    analyzer,
                                    FeedbackRoiPlanner::new(scaled.width, scaled.height)?,
                                ))
                            }) {
                                Ok((analyzer, planner)) => {
                                    reconstruction_feedback_analyzer = Some(analyzer);
                                    feedback_roi_planner = Some(planner);
                                    feedback_wire_budget = feedback_headroom_enabled
                                        .then(|| FeedbackWireBudget::new(Instant::now()));
                                }
                                Err(error) => {
                                    reconstruction_feedback_failed_key = Some(feedback_key);
                                    warn!(
                                        ?error,
                                        width = scaled.width,
                                        height = scaled.height,
                                        "reconstruction-feedback candidate initialization failed"
                                    );
                                }
                            }
                        }
                        if let Some(activity) = benchmark_roi_activity.as_mut() {
                            activity.reconstruction_active = Some(
                                reconstruction_feedback_analyzer.is_some()
                                    && feedback_roi_planner.is_some(),
                            );
                        }
                        if reconstruction_feedback_analyzer.is_none()
                            || feedback_roi_planner.is_none()
                        {
                            continue 'session;
                        }
                    }
                    let feedback_sequence = media_sequence.wrapping_add(1);
                    let feedback_reconstruction_texture = feedback_roi_enabled.then(|| {
                        reconstruction_feedback_analyzer
                            .as_ref()
                            .expect("feedback analyzer initialized above")
                            .reconstruction_texture_raw(feedback_sequence)
                    });
                    let mut feedback_plan = None;
                    let mut feedback_wire_decision = None;
                    session_stage!(Encode);
                    let encoded = if let Some(reason) = fallback_reason {
                        match recreate_and_encode_profile(
                            &mut encoder,
                            args.encoder_preference(),
                            &mut discovered_backend,
                            &mut discovery_reason,
                            dev,
                            mode,
                            frame.width(),
                            frame.height(),
                            quality.target(),
                            preset,
                            &mut active_codec,
                            active_benchmark_profile,
                            scaled.texture,
                            feedback_reconstruction_texture,
                            main10,
                            || {
                                prepare_recovery_bgra_texture(
                                    &mut scaler,
                                    frame.device(),
                                    frame.texture(),
                                    frame.width(),
                                    frame.height(),
                                    mode,
                                    viewport_crop,
                                )
                            },
                        ) {
                            Ok(encoded) => {
                                adopt_encoder_codec(&mut active_codec, encoder.as_ref());
                                encoded
                            }
                            Err(error) => {
                                if let Some(adapter) = discovered_adapter {
                                    encoder_discovery.record_actual(
                                        adapter,
                                        discovered_backend,
                                        &discovery_reason,
                                    );
                                }
                                warn!(
                                    ?error,
                                    ?reason,
                                    "fallback encoder recovery failed; retaining session for retry"
                                );
                                latest_idr = None;
                                pending_transition_idr = None;
                                next_idr = now;
                                continue 'session;
                            }
                        }
                    } else {
                        let temporal_roi_enabled = encoder_caps.roi_qp_map
                            && active_benchmark_profile.is_some_and(|profile| {
                                profile.schema_version == 2
                                    && profile.implementation_variant
                                        == benchmark::BenchmarkImplementationVariant::Candidate
                                    && matches!(
                                        profile.effective_roi_mode(),
                                        BenchmarkRoiMode::Damage
                                            | BenchmarkRoiMode::TextEdge
                                            | BenchmarkRoiMode::Feedback
                                            | BenchmarkRoiMode::FeedbackHeadroom
                                    )
                                    && !profile.encoder.main10
                            });
                        let temporal_analysis_enabled =
                            temporal_roi_enabled || static_hybrid_enabled;
                        let temporal_key = (
                            benchmark_profile_revision,
                            media_generation,
                            viewport_revision,
                            mode,
                        );
                        let mut temporal_result = None;
                        // Scroll/video-dominant on a constrained link: changed
                        // tiles near the pointer get the lower QP instead.
                        let wss_v4_pointer_tiles = match tile_tracker.as_ref() {
                            Some(tracker)
                                if !wss_v4_refine_frame
                                    && !wss_forced_idr
                                    && latest_idr.is_some()
                                    && !transition
                                    && !encoder.as_ref().is_some_and(VideoEncoder::idr_pending) =>
                            {
                                v4_pacer.pointer_tiles(&tracker.grid(), v4_pointer, &tile_changed)
                            }
                            _ => Vec::new(),
                        };
                        let wss_v4_qp_map = if (wss_v4_refine_frame
                            || !wss_v4_pointer_tiles.is_empty())
                            && encoder
                                .as_ref()
                                .is_some_and(VideoEncoder::refine_qp_map_enabled)
                        {
                            let lowered = if wss_v4_refine_frame {
                                wss_v4_refine_plan
                                    .as_ref()
                                    .filter(|plan| !plan.whole_frame)
                                    .map(|plan| plan.tiles.as_slice())
                            } else {
                                Some(wss_v4_pointer_tiles.as_slice())
                            };
                            lowered
                                .zip(tile_tracker.as_ref())
                                .and_then(|(tiles, tracker)| {
                                    refine_qp_map(
                                        tracker.grid(),
                                        tiles,
                                        &tile_changed,
                                        encode_crop(
                                            frame.width(),
                                            frame.height(),
                                            viewport_crop,
                                            mode,
                                        ),
                                        scaled.width,
                                        scaled.height,
                                    )
                                })
                        } else {
                            None
                        };
                        if wss_v4_qp_map.is_none() {
                            // No per-region QP: the refine covers the frame.
                            if let Some(plan) = wss_v4_refine_plan.as_mut() {
                                plan.whole_frame = true;
                            }
                        }
                        let frame_damage = if temporal_analysis_enabled {
                            let analyzer_matches =
                                temporal_diff_analyzer.as_ref().is_some_and(|analyzer| {
                                    analyzer.is_for(frame.device(), scaled.width, scaled.height)
                                });
                            if temporal_diff_key != Some(temporal_key) || !analyzer_matches {
                                temporal_diff_analyzer = None;
                                temporal_diff_key = Some(temporal_key);
                                temporal_diff_failed_key = None;
                            }
                            if temporal_diff_analyzer.is_none()
                                && temporal_diff_failed_key != Some(temporal_key)
                            {
                                match TemporalDiffAnalyzer::new(
                                    frame.device(),
                                    scaled.width,
                                    scaled.height,
                                ) {
                                    Ok(analyzer) => temporal_diff_analyzer = Some(analyzer),
                                    Err(error) => {
                                        temporal_diff_failed_key = Some(temporal_key);
                                        warn!(
                                            ?error,
                                            width = scaled.width,
                                            height = scaled.height,
                                            "GPU temporal-difference ROI is unsupported for this candidate state"
                                        );
                                    }
                                }
                            }
                            if let Some(analyzer) = temporal_diff_analyzer.as_mut() {
                                let source = unsafe {
                                    windows::Win32::Graphics::Direct3D11::ID3D11Texture2D::from_raw_borrowed(
                                        &scaled.texture,
                                    )
                                };
                                match source.context("scaled texture pointer is null").and_then(
                                    |texture| match temporal_readback_policy(
                                        static_hybrid_enabled,
                                        feedback_roi_enabled,
                                    ) {
                                        TemporalReadbackPolicy::Aligned => {
                                            analyzer.analyze_aligned(texture)
                                        }
                                        TemporalReadbackPolicy::Deferred => {
                                            analyzer.analyze(texture)
                                        }
                                    },
                                ) {
                                    Ok(result) => {
                                        if temporal_roi_enabled {
                                            record_benchmark_temporal_activity(
                                                &mut benchmark_roi_activity,
                                                &mut benchmark_temporal_analysis_us,
                                                &result,
                                                wss_media
                                                    .as_ref()
                                                    .map(|_| media_sequence.wrapping_add(1)),
                                            );
                                        }
                                        temporal_result = Some(result);
                                    }
                                    Err(error) => {
                                        temporal_diff_analyzer = None;
                                        temporal_diff_failed_key = Some(temporal_key);
                                        warn!(
                                            ?error,
                                            "GPU temporal-difference ROI failed; DDA fallback remains disabled"
                                        );
                                    }
                                }
                            }
                            if static_hybrid_enabled {
                                hybrid_temporal_result = temporal_result.clone();
                            }
                            if feedback_roi_enabled {
                                let completed = match reconstruction_feedback_analyzer
                                    .as_mut()
                                    .expect("feedback analyzer initialized above")
                                    .collect_blocking(feedback_sequence)
                                {
                                    Ok(completed) => completed,
                                    Err(error) => {
                                        warn!(
                                            ?error,
                                            sequence = feedback_sequence,
                                            "reconstruction-feedback readback failed"
                                        );
                                        reconstruction_feedback_analyzer = None;
                                        feedback_roi_planner = None;
                                        feedback_wire_budget = None;
                                        reconstruction_feedback_failed_key = Some(feedback_key);
                                        if let Some(activity) = benchmark_roi_activity.as_mut() {
                                            activity.reconstruction_active = Some(false);
                                        }
                                        continue 'session;
                                    }
                                };
                                let mut feedback_errors = None;
                                match completed {
                                    Some(completed)
                                        if is_exact_previous_feedback(
                                            feedback_sequence,
                                            completed.sequence,
                                            completed.age_frames,
                                        ) =>
                                    {
                                        if let Some(activity) = benchmark_roi_activity.as_mut() {
                                            add_feedback_count(
                                                &mut activity.feedback_ready_frames,
                                                1,
                                            );
                                            activity.last_feedback_sequence =
                                                Some(completed.sequence);
                                            activity.last_feedback_age_frames =
                                                Some(completed.age_frames);
                                            record_feedback_timing(
                                                &mut benchmark_feedback_readback_us,
                                                &mut activity.feedback_readback_p95_us,
                                                &mut activity.feedback_readback_max_us,
                                                completed.readback_us,
                                            );
                                        }
                                        feedback_errors = Some(completed.errors);
                                    }
                                    Some(completed) => {
                                        if let Some(activity) = benchmark_roi_activity.as_mut() {
                                            add_feedback_count(
                                                &mut activity.feedback_late_frames,
                                                1,
                                            );
                                            record_feedback_timing(
                                                &mut benchmark_feedback_readback_us,
                                                &mut activity.feedback_readback_p95_us,
                                                &mut activity.feedback_readback_max_us,
                                                completed.readback_us,
                                            );
                                        }
                                    }
                                    None => {
                                        if let Some(activity) = benchmark_roi_activity.as_mut() {
                                            add_feedback_count(
                                                &mut activity.feedback_missing_frames,
                                                1,
                                            );
                                        }
                                    }
                                }
                                if let Some(result) = temporal_result.as_ref() {
                                    let changed = match changed_ctb_mask(
                                        scaled.width,
                                        scaled.height,
                                        &result.damage_rects,
                                    ) {
                                        Ok(changed) => changed,
                                        Err(error) => {
                                            warn!(
                                                ?error,
                                                "reconstruction-feedback damage mask failed"
                                            );
                                            continue 'session;
                                        }
                                    };
                                    let edge_fallback;
                                    let edge_scores = if result.edge_scores.len() == changed.len() {
                                        result.edge_scores.as_slice()
                                    } else {
                                        edge_fallback = vec![0; changed.len()];
                                        edge_fallback.as_slice()
                                    };
                                    let decision = if feedback_headroom_enabled {
                                        let cap_bps = active_benchmark_profile
                                            .and_then(|profile| profile.total_media_cap_bps)
                                            .or_else(|| {
                                                active_benchmark_profile
                                                    .map(|profile| profile.bitrate_bps)
                                            })
                                            .unwrap_or(6_000_000);
                                        feedback_wire_budget
                                            .as_mut()
                                            .expect("headroom budget initialized with analyzer")
                                            .decision(Instant::now(), u64::from(cap_bps))
                                    } else {
                                        FeedbackWireDecision {
                                            policy: FeedbackBudgetPolicy::NetZero,
                                            occupancy_bps: 0,
                                        }
                                    };
                                    match feedback_roi_planner
                                        .as_mut()
                                        .expect("feedback planner initialized above")
                                        .plan_with_policy(
                                            &changed,
                                            edge_scores,
                                            feedback_errors.as_deref(),
                                            false,
                                            decision.policy,
                                        ) {
                                        Ok(plan) => {
                                            feedback_plan = Some(plan);
                                            feedback_wire_decision =
                                                feedback_headroom_enabled.then_some(decision);
                                        }
                                        Err(error) => {
                                            warn!(
                                                ?error,
                                                "reconstruction-feedback ROI plan failed"
                                            );
                                            continue 'session;
                                        }
                                    }
                                }
                            }
                            temporal_roi_enabled
                                .then(|| {
                                    temporal_result.as_ref().map(|result| FrameDamage {
                                        source_width: scaled.width,
                                        source_height: scaled.height,
                                        crop: frame_damage::DamageRect {
                                            left: 0,
                                            top: 0,
                                            right: scaled.width,
                                            bottom: scaled.height,
                                        },
                                        rects: &result.damage_rects,
                                        edge_scores: active_benchmark_profile
                                            .filter(|profile| {
                                                profile.effective_roi_mode()
                                                    == BenchmarkRoiMode::TextEdge
                                            })
                                            .map(|_| result.edge_scores.as_slice()),
                                        prepared_qp_map: feedback_plan
                                            .as_ref()
                                            .and_then(|plan| plan.map.as_ref()),
                                    })
                                })
                                .flatten()
                        } else {
                            encode_frame_damage(&frame, viewport_crop, mode, wss_v4_qp_map.as_ref())
                        };
                        if tile_tracker.is_some() {
                            // Text-dominant on a constrained link: a QP
                            // ceiling (reconfigured in place, no IDR).
                            let _ = encoder.as_mut().unwrap().set_max_qp(v4_pacer.max_qp());
                        }
                        let wss_v4_boost = wss_v4_refine_frame
                            || (tile_tracker.is_some()
                                && encoder.as_ref().is_some_and(VideoEncoder::idr_pending));
                        let refine_restore_bps = if wss_v4_boost {
                            let enc = encoder.as_mut().unwrap();
                            let base = enc.bitrate();
                            enc.set_bitrate(wss_v4_refine_bitrate(base))
                                .ok()
                                .map(|()| base)
                        } else {
                            None
                        };
                        let encode_result = if cpu_conversion && viewport_crop.is_none() {
                            let damage = if is_idle_snapshot {
                                None
                            } else {
                                cpu_damage.rects()
                            };
                            encoder
                                .as_mut()
                                .unwrap()
                                .encode_cpu_texture(scaled.texture, damage)
                        } else if let Some(reconstruction_texture) = feedback_reconstruction_texture
                        {
                            encoder.as_mut().unwrap().encode_texture_with_feedback(
                                scaled.texture,
                                frame_damage,
                                reconstruction_texture,
                            )
                        } else {
                            encoder
                                .as_mut()
                                .unwrap()
                                .encode_texture_with_damage(scaled.texture, frame_damage)
                        };
                        if let Some(base) = refine_restore_bps {
                            if let Err(error) = encoder.as_mut().unwrap().set_bitrate(base) {
                                warn!(?error, "WSS v4 refine bitrate restore failed");
                            }
                        }
                        match encode_result {
                            Ok(encoded) if !transition || encoded.is_idr => encoded,
                            Ok(_) | Err(_) => {
                                if feedback_roi_enabled {
                                    warn!(
                                        target = mode.label,
                                        "feedback encode failed; resetting the candidate session"
                                    );
                                    encoder = None;
                                    reconstruction_feedback_analyzer = None;
                                    feedback_roi_planner = None;
                                    feedback_wire_budget = None;
                                    latest_idr = None;
                                    pending_transition_idr = None;
                                    next_idr = now;
                                    continue 'session;
                                }
                                warn!(
                                    target = mode.label,
                                    "transition encode failed; recovering last working mode"
                                );
                                mode = fallback_mode(old_mode, frame.width(), frame.height(), true);
                                let reason = "encoder-fallback";
                                let fallback_texture = match if cpu_conversion
                                    && viewport_crop.is_none()
                                {
                                    Ok(cpu_mode_texture(&frame, mode))
                                } else {
                                    prepare_video_mode_texture(
                                        &mut scaler,
                                        frame.device(),
                                        frame.texture(),
                                        frame.width(),
                                        frame.height(),
                                        mode,
                                        main10,
                                        encoder_resource_mode,
                                    )
                                } {
                                    Ok(output) => output,
                                    Err(error) => {
                                        warn!(?error, "scaled fallback unavailable; recovering exact native mode");
                                        mode = fallback_mode(
                                            old_mode,
                                            frame.width(),
                                            frame.height(),
                                            false,
                                        );
                                        fallback_reason = Some("scaler-fallback");
                                        match if cpu_conversion && viewport_crop.is_none() {
                                            Ok(cpu_mode_texture(&frame, mode))
                                        } else {
                                            prepare_video_mode_texture(
                                                &mut scaler,
                                                frame.device(),
                                                frame.texture(),
                                                frame.width(),
                                                frame.height(),
                                                mode,
                                                main10,
                                                encoder_resource_mode,
                                            )
                                        } {
                                            Ok(output) => output,
                                            Err(native_error) => {
                                                warn!(?native_error, "native fallback texture preparation failed; retaining session for retry");
                                                latest_idr = None;
                                                pending_transition_idr = None;
                                                next_idr = now;
                                                continue 'session;
                                            }
                                        }
                                    }
                                };
                                match recreate_and_encode_profile(
                                    &mut encoder,
                                    args.encoder_preference(),
                                    &mut discovered_backend,
                                    &mut discovery_reason,
                                    dev,
                                    mode,
                                    frame.width(),
                                    frame.height(),
                                    quality.target(),
                                    preset,
                                    &mut active_codec,
                                    active_benchmark_profile,
                                    fallback_texture.texture,
                                    None,
                                    main10,
                                    || {
                                        if cpu_conversion && viewport_crop.is_none() {
                                            Ok(frame.texture().clone())
                                        } else {
                                            prepare_recovery_bgra_texture(
                                                &mut scaler,
                                                frame.device(),
                                                frame.texture(),
                                                frame.width(),
                                                frame.height(),
                                                mode,
                                                viewport_crop,
                                            )
                                        }
                                    },
                                ) {
                                    Ok(encoded) => {
                                        adopt_encoder_codec(&mut active_codec, encoder.as_ref());
                                        fallback_reason.get_or_insert(reason);
                                        encoded
                                    }
                                    Err(error) => {
                                        if let Some(adapter) = discovered_adapter {
                                            encoder_discovery.record_actual(
                                                adapter,
                                                discovered_backend,
                                                &discovery_reason,
                                            );
                                        }
                                        warn!(?error, "fallback encoder recovery failed; retaining session for retry");
                                        latest_idr = None;
                                        pending_transition_idr = None;
                                        next_idr = now;
                                        continue 'session;
                                    }
                                }
                            }
                        }
                    };
                    if fallback_reason.is_some() {
                        if let Some(adapter) = discovered_adapter {
                            encoder_discovery.record_actual(
                                adapter,
                                discovered_backend,
                                &discovery_reason,
                            );
                        }
                    }
                    if cpu_conversion && viewport_crop.is_none() {
                        cpu_damage.converted();
                        if cpu_pacing.observe(
                            encoded.encode_us,
                            CpuTarget {
                                width: mode.width,
                                height: mode.height,
                                fps: mode.fps,
                            },
                            Instant::now(),
                        ) {
                            info!(
                                work_us = encoded.encode_us,
                                width = mode.width,
                                height = mode.height,
                                fps = mode.fps,
                                "CPU frame pressure changed resolution/fps cap"
                            );
                        }
                    }
                    ensure_rtc_encoder_codec(
                        discovered_backend,
                        wss_media.is_some(),
                        negotiated_video_codec,
                        active_codec,
                    )?;
                    if active_benchmark_profile.is_some() && active_codec != VideoCodec::Hevc {
                        // Late recovery selected an H.264-only backend. The
                        // next pass drops the HEVC lease before using any
                        // reconstruction, ROI or hybrid benchmark state.
                        latest_idr = None;
                        pending_transition_idr = None;
                        pending_wss_idr = None;
                        pending_viewport_idr = None;
                        next_admission = now;
                        next_idr = now;
                        if let Some(encoder) = encoder.as_mut() {
                            encoder.force_idr();
                        }
                        capture.request_idle_followup_snapshot(Duration::ZERO);
                        continue 'session;
                    }
                    if feedback_roi_enabled {
                        if encoded.is_idr {
                            feedback_roi_planner
                                .as_mut()
                                .expect("feedback planner initialized above")
                                .reset();
                            reconstruction_feedback_analyzer
                                .as_mut()
                                .expect("feedback analyzer initialized above")
                                .reset();
                        } else if let Some(plan) = feedback_plan.as_ref() {
                            record_benchmark_feedback_plan(
                                &mut benchmark_roi_activity,
                                &mut benchmark_feedback_wire_occupancy_bps,
                                plan,
                                feedback_wire_decision,
                            );
                        }
                        let source = unsafe {
                            windows::Win32::Graphics::Direct3D11::ID3D11Texture2D::from_raw_borrowed(
                                &scaled.texture,
                            )
                        };
                        let submission = source
                            .context("feedback source texture pointer is null")
                            .and_then(|texture| {
                                reconstruction_feedback_analyzer
                                    .as_mut()
                                    .expect("feedback analyzer initialized above")
                                    .submit(texture, feedback_sequence)
                            });
                        match submission {
                            Ok(submission) => record_benchmark_feedback_submission(
                                &mut benchmark_roi_activity,
                                &mut benchmark_feedback_submit_us,
                                submission,
                            ),
                            Err(error) => {
                                warn!(
                                    ?error,
                                    sequence = feedback_sequence,
                                    "reconstruction-feedback GPU submission failed"
                                );
                                reconstruction_feedback_analyzer = None;
                                feedback_roi_planner = None;
                                feedback_wire_budget = None;
                                reconstruction_feedback_failed_key = Some(feedback_key);
                                encoder = None;
                                if let Some(activity) = benchmark_roi_activity.as_mut() {
                                    activity.reconstruction_active = Some(false);
                                }
                            }
                        }
                    }
                    if let Some(submission) = quiescent_temporal_submission.take() {
                        if let Some(analyzer) = quiescent_diff_analyzer.as_mut() {
                            let completion = analyzer.collect_quiescent(submission);
                            let validation_failed =
                                completion.validation_status == QuiescentValidationStatus::Failed;
                            if let Some(error) = completion.validation_error.as_deref() {
                                warn!(
                                    error,
                                    sequence = media_sequence.wrapping_add(1),
                                    "GPU quiescent-detail validation collection failed; invalidating active tiles"
                                );
                            }
                            record_benchmark_hybrid_validation(
                                &mut benchmark_hybrid_activity,
                                &mut benchmark_hybrid_validation_submit_us,
                                &mut benchmark_hybrid_validation_collect_us,
                                &mut benchmark_hybrid_validation_age_frames,
                                completion.submit_us,
                                completion.collect_us,
                                completion.validation_status,
                                &completion.deferred_readback,
                                completion.result.damage_audit,
                                media_sequence.wrapping_add(1),
                            );
                            let mut result = completion.result;
                            for verdict in &completion.deferred_readback.verdicts {
                                let cached_png_bytes = quiescent_tile_planner
                                    .cached_png_bytes(verdict.request.tile_id)
                                    .unwrap_or_default()
                                    as u64;
                                let superseded = quiescent_tile_planner
                                    .validation_verdict_superseded(
                                        quiescent_viewport
                                            .expect("quiescent viewport is available"),
                                        verdict,
                                    );
                                if let Some(operation) = quiescent_tile_planner.resolve_validation(
                                    quiescent_viewport.expect("quiescent viewport is available"),
                                    verdict,
                                ) {
                                    if let Some(activity) = benchmark_hybrid_activity.as_mut() {
                                        match &operation {
                                            HybridTileOperation::Reactivate { .. } => {
                                                activity.png_bytes_avoided = activity
                                                    .png_bytes_avoided
                                                    .saturating_add(cached_png_bytes);
                                            }
                                            HybridTileOperation::InvalidateValidated { .. } => {
                                                quiescent_verified_changed_rects
                                                    .push(verdict.request.source);
                                                activity.permanent_invalidations = activity
                                                    .permanent_invalidations
                                                    .saturating_add(1);
                                                activity.invalidations =
                                                    activity.invalidations.saturating_add(1);
                                            }
                                            _ => {}
                                        }
                                        activity.cached_bytes_retained =
                                            activity
                                                .cached_bytes_retained
                                                .max(quiescent_tile_planner.retained_png_bytes()
                                                    as u64);
                                    }
                                    quiescent_resolution_operations.push(operation);
                                } else if !superseded {
                                    if let Some(activity) = benchmark_hybrid_activity.as_mut() {
                                        activity.stale_reactivation_rejects =
                                            activity.stale_reactivation_rejects.saturating_add(1);
                                    }
                                }
                            }
                            result.immediate_damage_rects = quiescent_immediate_damage_rects(
                                quiescent_cache_mode,
                                frame.damage(),
                                &quiescent_verified_changed_rects,
                                is_idle_snapshot,
                            )
                            .to_vec();
                            result.damage_rects = quiescent_damage_rects(
                                frame.damage(),
                                &result.damage_rects,
                                is_idle_snapshot,
                            )
                            .to_vec();
                            quiescent_temporal_result = Some(result);
                            if validation_failed {
                                quiescent_diff_analyzer = None;
                            }
                        }
                    }
                    if active_benchmark_profile.is_some() {
                        record_benchmark_encode_stages(
                            &mut benchmark_encode_stages,
                            encoded.stage_timings,
                        );
                        if let Some(activity) = encoded.roi_activity {
                            record_benchmark_roi_activity(
                                &mut benchmark_roi_activity,
                                activity,
                                wss_media.as_ref().map(|_| media_sequence.wrapping_add(1)),
                            );
                        }
                        benchmark_encoded_bytes =
                            benchmark_encoded_bytes.saturating_add(encoded.data.len() as u64);
                        benchmark_encode_count = benchmark_encode_count.saturating_add(1);
                        benchmark_encode_us_total =
                            benchmark_encode_us_total.saturating_add(encoded.encode_us);
                        benchmark_encode_us_max = benchmark_encode_us_max
                            .max(encoded.encode_us.min(u64::from(u32::MAX)) as u32);
                        if let Some(qp_map) = encoded.qp_map {
                            benchmark_qp_map_frames = benchmark_qp_map_frames.saturating_add(1);
                            benchmark_qp_map_changed_ctbs = benchmark_qp_map_changed_ctbs
                                .saturating_add(u64::from(qp_map.changed_ctbs));
                            benchmark_qp_map_total_ctbs = benchmark_qp_map_total_ctbs
                                .saturating_add(u64::from(qp_map.total_ctbs));
                        }
                        if encoded.is_idr {
                            benchmark_idr_count = benchmark_idr_count.saturating_add(1);
                        } else {
                            benchmark_delta_count = benchmark_delta_count.saturating_add(1);
                        }
                    }
                    let mut hybrid_wss_payload = None;
                    let mut request_quiescent_transition_followup = false;
                    if hybrid_enabled {
                        let mut operations = Vec::new();
                        if quiescent_hybrid_enabled {
                            operations.append(&mut quiescent_suppression_operations);
                            operations.append(&mut quiescent_resolution_operations);
                            coalesce_quiescent_cache_round_trips(&mut operations);
                            if let Some(activity) = benchmark_hybrid_activity.as_mut() {
                                for operation in &operations {
                                    match operation {
                                        HybridTileOperation::Suppress { .. } => {
                                            activity.suppressions =
                                                activity.suppressions.saturating_add(1);
                                        }
                                        HybridTileOperation::Reactivate { .. } => {
                                            activity.reactivations =
                                                activity.reactivations.saturating_add(1);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            if let Some(result) = quiescent_temporal_result.as_ref() {
                                let plan = quiescent_tile_planner.plan_with_immediate_damage(
                                    frame.width(),
                                    frame.height(),
                                    quiescent_viewport.expect("quiescent viewport is available"),
                                    &result.damage_rects,
                                    &result.immediate_damage_rects,
                                    &result.edge_scores,
                                );
                                let planned_sequence = media_sequence.wrapping_add(1);
                                if plan.output_changed || planned_sequence.is_multiple_of(30) {
                                    info!(
                                        sequence = planned_sequence,
                                        generation = media_generation,
                                        crop_revision = viewport_revision,
                                        history_damage_rects = result.damage_rects.len(),
                                        immediate_damage_rects =
                                            result.immediate_damage_rects.len(),
                                        changed_cells = plan.changed_cells,
                                        total_cells = plan.total_cells,
                                        edge_score_cells = plan.edge_score_cells,
                                        expected_edge_score_cells = plan.expected_edge_score_cells,
                                        stability_zero_cells = plan.stability_zero_cells,
                                        stability_one_cells = plan.stability_one_cells,
                                        stability_two_cells = plan.stability_two_cells,
                                        stability_three_plus_cells =
                                            plan.stability_three_plus_cells,
                                        fully_stable_blocks = plan.fully_stable_blocks,
                                        edge_eligible_blocks = plan.edge_eligible_blocks,
                                        candidates = plan.candidates.len(),
                                        motion_bypass = plan.motion_bypass,
                                        "quiescent detail eligibility diagnostic"
                                    );
                                }
                                let plan_diagnostic = plan.output_changed.then_some((
                                    plan.history_preserved,
                                    plan.active_before_planning,
                                    plan.changed_cells,
                                    plan.total_cells,
                                    plan.motion_bypass,
                                    plan.preserved_candidates,
                                    plan.rejected_damage,
                                    plan.rejected_partial_visibility,
                                    plan.rejected_mapping,
                                    plan.rejected_dimension,
                                    plan.candidates.len(),
                                ));
                                record_benchmark_hybrid_plan(
                                    &mut benchmark_hybrid_activity,
                                    &mut benchmark_hybrid_analysis_us,
                                    &plan,
                                    result.damage_rects.is_empty(),
                                    result.analysis_us,
                                );
                                if let Some(activity) = benchmark_hybrid_activity.as_mut() {
                                    activity.cached_bytes_retained = activity
                                        .cached_bytes_retained
                                        .max(quiescent_tile_planner.retained_png_bytes() as u64);
                                }
                                if plan.clear {
                                    operations.push(HybridTileOperation::Clear);
                                }
                                operations.extend(
                                    plan.invalidated
                                        .into_iter()
                                        .map(|tile_id| HybridTileOperation::Invalidate { tile_id }),
                                );
                                let mut sidecar_bytes = operations
                                    .iter()
                                    .map(hybrid_operation_wire_bytes)
                                    .sum::<usize>();
                                let sidecar_budget = active_benchmark_profile
                                    .map(|profile| {
                                        // One unsharded v4 media socket.
                                        quiescent_wire_budget.begin_frame(
                                            profile.bitrate_bps,
                                            u32::from(profile.fps),
                                            1,
                                            1,
                                            encoded.data.len(),
                                        )
                                    })
                                    .unwrap_or(MAX_HYBRID_SIDECAR_BYTES);
                                let mut frame_upserts = 0usize;
                                let mut frame_budget_skipped_upserts = 0usize;
                                let mut frame_deferred_upserts = 0usize;
                                let progressive_transition = plan.output_changed;
                                let scaled_texture = unsafe {
                                    windows::Win32::Graphics::Direct3D11::ID3D11Texture2D::from_raw_borrowed(
                                        &scaled.texture,
                                    )
                                };
                                if let Some(scaled_texture) = scaled_texture {
                                    let tile_encoder_key =
                                        (benchmark_profile_revision, dev as usize);
                                    let has_supported_hevc_geometry = hevc_quiescent_tiles_enabled
                                        && plan.candidates.iter().any(|candidate| {
                                            is_supported_output_rect(candidate.output)
                                        });
                                    if hevc_quiescent_tiles_enabled
                                        && hevc_tile_encoder.as_ref().is_some_and(|encoder| {
                                            !encoder.is_for_device(frame.device())
                                        })
                                    {
                                        hevc_tile_encoder = None;
                                    }
                                    if has_supported_hevc_geometry
                                        && hevc_tile_encoder.is_none()
                                        && hevc_tile_encoder_failed_key != Some(tile_encoder_key)
                                    {
                                        match HevcTileEncoder::new(frame.device()) {
                                            Ok(encoder) => {
                                                hevc_tile_encoder = Some(encoder);
                                                if let Some(activity) =
                                                    benchmark_hybrid_activity.as_mut()
                                                {
                                                    activity.hevc_tile_encoder_opens = activity
                                                        .hevc_tile_encoder_opens
                                                        .saturating_add(1);
                                                }
                                            }
                                            Err(error) => {
                                                hevc_tile_encoder_failed_key =
                                                    Some(tile_encoder_key);
                                                if let Some(activity) =
                                                    benchmark_hybrid_activity.as_mut()
                                                {
                                                    activity.hevc_tile_encoder_failures = activity
                                                        .hevc_tile_encoder_failures
                                                        .saturating_add(1);
                                                }
                                                warn!(
                                                    ?error,
                                                    "HEVC quiescent tile capability probe failed; base video remains active"
                                                );
                                            }
                                        }
                                    }
                                    for candidate in plan.candidates {
                                        if !quiescent_transition_upsert_allowed(
                                            progressive_transition,
                                            frame_upserts,
                                        ) {
                                            frame_deferred_upserts =
                                                frame_deferred_upserts.saturating_add(1);
                                            quiescent_tile_planner
                                                .reject_candidate(candidate.tile_id);
                                            continue;
                                        }
                                        if hevc_quiescent_tiles_enabled {
                                            if !is_supported_output_rect(candidate.output) {
                                                quiescent_tile_planner
                                                    .reject_candidate(candidate.tile_id);
                                                continue;
                                            }
                                            if sidecar_bytes.saturating_add(16) > sidecar_budget {
                                                frame_budget_skipped_upserts =
                                                    frame_budget_skipped_upserts.saturating_add(1);
                                                if let Some(activity) =
                                                    benchmark_hybrid_activity.as_mut()
                                                {
                                                    activity.budget_skipped_upserts = activity
                                                        .budget_skipped_upserts
                                                        .saturating_add(1);
                                                }
                                                quiescent_tile_planner
                                                    .reject_candidate(candidate.tile_id);
                                                continue;
                                            }
                                            let Some(tile_encoder) = hevc_tile_encoder.as_mut()
                                            else {
                                                quiescent_tile_planner
                                                    .reject_candidate(candidate.tile_id);
                                                continue;
                                            };
                                            match tile_encoder
                                                .copy_and_encode(scaled_texture, candidate.output)
                                            {
                                                Ok(tile)
                                                    if sidecar_bytes
                                                        .saturating_add(16)
                                                        .saturating_add(tile.frame.data.len())
                                                        <= sidecar_budget =>
                                                {
                                                    let access_unit_len = tile.frame.data.len();
                                                    sidecar_bytes += 16 + access_unit_len;
                                                    frame_upserts = frame_upserts.saturating_add(1);
                                                    quiescent_tile_planner.commit_candidate(
                                                        candidate.tile_id,
                                                        media_sequence.wrapping_add(1),
                                                        access_unit_len,
                                                    );
                                                    if let Some(activity) =
                                                        benchmark_hybrid_activity.as_mut()
                                                    {
                                                        activity.upserts =
                                                            activity.upserts.saturating_add(1);
                                                        activity.hevc_tile_bytes = activity
                                                            .hevc_tile_bytes
                                                            .saturating_add(access_unit_len as u64);
                                                        activity.hevc_tile_copy_us_total = activity
                                                            .hevc_tile_copy_us_total
                                                            .saturating_add(tile.copy_us);
                                                        activity.hevc_tile_copy_us_max =
                                                            activity.hevc_tile_copy_us_max.max(
                                                                tile.copy_us
                                                                    .min(u64::from(u32::MAX))
                                                                    as u32,
                                                            );
                                                        activity.hevc_tile_encode_us_total =
                                                            activity
                                                                .hevc_tile_encode_us_total
                                                                .saturating_add(
                                                                    tile.frame.encode_us,
                                                                );
                                                        activity.hevc_tile_encode_us_max =
                                                            activity.hevc_tile_encode_us_max.max(
                                                                tile.frame
                                                                    .encode_us
                                                                    .min(u64::from(u32::MAX))
                                                                    as u32,
                                                            );
                                                    }
                                                    operations.push(
                                                        HybridTileOperation::HevcUpsert {
                                                            tile_id: candidate.tile_id,
                                                            rect: candidate.output,
                                                            access_unit: tile.frame.data,
                                                        },
                                                    );
                                                }
                                                Ok(_) => {
                                                    frame_budget_skipped_upserts =
                                                        frame_budget_skipped_upserts
                                                            .saturating_add(1);
                                                    if let Some(activity) =
                                                        benchmark_hybrid_activity.as_mut()
                                                    {
                                                        activity.budget_skipped_upserts = activity
                                                            .budget_skipped_upserts
                                                            .saturating_add(1);
                                                    }
                                                    quiescent_tile_planner
                                                        .reject_candidate(candidate.tile_id);
                                                }
                                                Err(error) => {
                                                    hevc_tile_encoder = None;
                                                    hevc_tile_encoder_failed_key =
                                                        Some(tile_encoder_key);
                                                    if let Some(activity) =
                                                        benchmark_hybrid_activity.as_mut()
                                                    {
                                                        activity.hevc_tile_encoder_failures =
                                                            activity
                                                                .hevc_tile_encoder_failures
                                                                .saturating_add(1);
                                                    }
                                                    quiescent_tile_planner
                                                        .reject_candidate(candidate.tile_id);
                                                    warn!(
                                                        ?error,
                                                        tile_id = candidate.tile_id,
                                                        "HEVC quiescent tile encode failed; base video remains active"
                                                    );
                                                }
                                            }
                                            continue;
                                        }
                                        let png_started = Instant::now();
                                        let png_result = encode_texture_png_crop(
                                            scaled_texture,
                                            candidate.output,
                                        );
                                        let png_us = png_started
                                            .elapsed()
                                            .as_micros()
                                            .min(u128::from(u64::MAX))
                                            as u64;
                                        if let Some(activity) = benchmark_hybrid_activity.as_mut() {
                                            activity.png_us_total =
                                                activity.png_us_total.saturating_add(png_us);
                                            activity.png_us_max = activity
                                                .png_us_max
                                                .max(png_us.min(u64::from(u32::MAX)) as u32);
                                        }
                                        match png_result {
                                            Ok(png)
                                                if sidecar_bytes
                                                    .saturating_add(16)
                                                    .saturating_add(png.len())
                                                    <= sidecar_budget =>
                                            {
                                                sidecar_bytes += 16 + png.len();
                                                frame_upserts = frame_upserts.saturating_add(1);
                                                quiescent_tile_planner.commit_candidate(
                                                    candidate.tile_id,
                                                    media_sequence.wrapping_add(1),
                                                    png.len(),
                                                );
                                                if let Some(activity) =
                                                    benchmark_hybrid_activity.as_mut()
                                                {
                                                    activity.upserts =
                                                        activity.upserts.saturating_add(1);
                                                    activity.png_bytes = activity
                                                        .png_bytes
                                                        .saturating_add(png.len() as u64);
                                                }
                                                operations.push(HybridTileOperation::Upsert {
                                                    tile_id: candidate.tile_id,
                                                    rect: candidate.output,
                                                    png,
                                                });
                                            }
                                            Ok(_) => {
                                                frame_budget_skipped_upserts =
                                                    frame_budget_skipped_upserts.saturating_add(1);
                                                if let Some(activity) =
                                                    benchmark_hybrid_activity.as_mut()
                                                {
                                                    activity.budget_skipped_upserts = activity
                                                        .budget_skipped_upserts
                                                        .saturating_add(1);
                                                }
                                                quiescent_tile_planner
                                                    .reject_candidate(candidate.tile_id);
                                            }
                                            Err(error) => {
                                                quiescent_tile_planner
                                                    .reject_candidate(candidate.tile_id);
                                                warn!(
                                                    ?error,
                                                    tile_id = candidate.tile_id,
                                                    "quiescent detail tile generation failed"
                                                );
                                            }
                                        }
                                    }
                                }
                                if let Some((
                                    history_preserved,
                                    active_before_planning,
                                    changed_cells,
                                    total_cells,
                                    motion_bypass,
                                    preserved_candidates,
                                    rejected_damage,
                                    rejected_partial_visibility,
                                    rejected_mapping,
                                    rejected_dimension,
                                    candidates_after_remap,
                                )) = plan_diagnostic
                                {
                                    info!(
                                        sequence = media_sequence.wrapping_add(1),
                                        generation = media_generation,
                                        crop_revision = viewport_revision,
                                        history_preserved,
                                        active_before_planning,
                                        changed_cells,
                                        total_cells,
                                        motion_bypass,
                                        preserved_candidates,
                                        rejected_damage,
                                        rejected_partial_visibility,
                                        rejected_mapping,
                                        rejected_dimension,
                                        candidates_after_remap,
                                        budget_skipped_candidates = frame_budget_skipped_upserts,
                                        deferred_candidates = frame_deferred_upserts,
                                        sidecar_budget,
                                        sidecar_bytes,
                                        "quiescent detail transition plan"
                                    );
                                }
                                let suppressed_tiles =
                                    quiescent_tile_planner.suppressed_tile_count();
                                if quiescent_transition_followup_needed(
                                    plan_diagnostic.is_some(),
                                    frame_upserts,
                                    frame_budget_skipped_upserts,
                                    suppressed_tiles,
                                ) {
                                    request_quiescent_transition_followup = true;
                                    info!(
                                        sequence = media_sequence.wrapping_add(1),
                                        generation = media_generation,
                                        crop_revision = viewport_revision,
                                        upserts = frame_upserts,
                                        budget_skipped_candidates = frame_budget_skipped_upserts,
                                        suppressed_tiles,
                                        "quiescent detail transition requested one activation frame"
                                    );
                                }
                                if let Some(activity) = benchmark_hybrid_activity.as_mut() {
                                    activity.sidecar_bytes =
                                        activity.sidecar_bytes.saturating_add(sidecar_bytes as u64);
                                }
                                quiescent_wire_budget.commit_sidecar(sidecar_bytes);
                            } else {
                                operations.push(HybridTileOperation::Clear);
                            }
                        } else if scaled.width != frame.width()
                            || scaled.height != frame.height()
                            || viewport_crop.is_some()
                        {
                            operations.push(HybridTileOperation::Clear);
                        } else if let Some(result) = hybrid_temporal_result.as_ref() {
                            let plan = hybrid_tile_planner.plan(
                                media_generation,
                                viewport_revision,
                                scaled.width,
                                scaled.height,
                                &result.damage_rects,
                                &result.edge_scores,
                            );
                            if plan.clear {
                                operations.push(HybridTileOperation::Clear);
                            }
                            operations.extend(
                                plan.invalidated
                                    .into_iter()
                                    .map(|tile_id| HybridTileOperation::Invalidate { tile_id }),
                            );
                            let mut sidecar_bytes = operations.len() * 4;
                            for (tile_id, rect) in plan.candidates {
                                match frame.encode_png_crop(rect) {
                                    Ok(png)
                                        if sidecar_bytes
                                            .saturating_add(16)
                                            .saturating_add(png.len())
                                            <= MAX_HYBRID_SIDECAR_BYTES =>
                                    {
                                        sidecar_bytes += 16 + png.len();
                                        operations.push(HybridTileOperation::Upsert {
                                            tile_id,
                                            rect,
                                            png,
                                        });
                                    }
                                    Ok(_) => hybrid_tile_planner.reject_candidate(tile_id),
                                    Err(error) => {
                                        hybrid_tile_planner.reject_candidate(tile_id);
                                        warn!(
                                            ?error,
                                            tile_id, "lossless detail tile generation failed"
                                        );
                                    }
                                }
                            }
                        } else {
                            operations.push(HybridTileOperation::Clear);
                        }
                        match encode_hybrid_frame_v1(&HybridFrame {
                            generation: media_generation,
                            sequence: media_sequence.wrapping_add(1),
                            crop_revision: viewport_revision,
                            access_unit: encoded.data.clone(),
                            operations,
                        }) {
                            Ok(payload) => hybrid_wss_payload = Some(payload),
                            Err(error) => {
                                warn!(?error, "hybrid frame envelope rejected");
                                if quiescent_hybrid_enabled {
                                    recover_quiescent_after_unsubmitted_frame(
                                        &mut quiescent_tile_planner,
                                        &mut quiescent_wire_budget,
                                    );
                                    quiescent_diff_analyzer = None;
                                }
                                continue 'session;
                            }
                        }
                    }
                    if encoded.is_idr {
                        latest_idr = Some(CachedIdr {
                            data: encoded.data.clone(),
                            mode,
                            requires_default: !frame.is_secure_snapshot(),
                            generation: media_generation,
                        });
                    }
                    capture.cancel_idle_followup_snapshot();
                    match idle_snapshot_completion_order(
                        encoded.is_idr,
                        request_quiescent_transition_followup,
                    ) {
                        IdleSnapshotCompletionOrder::None => {}
                        IdleSnapshotCompletionOrder::Acknowledge => {
                            capture.acknowledge_idle_snapshot_idr();
                        }
                        IdleSnapshotCompletionOrder::Request => {
                            capture.request_idle_followup_snapshot(frame_interval);
                        }
                        IdleSnapshotCompletionOrder::AcknowledgeThenRequest => {
                            capture.acknowledge_idle_snapshot_idr();
                            capture.request_idle_followup_snapshot(frame_interval);
                        }
                    }
                    next_admission = advance_frame_deadline(next_admission, now, frame_interval);
                    let id = next_au_id;
                    next_au_id += 1;
                    let rtc_active = ice_connected && wss_media.is_none();
                    let rtc_data = if rtc_active {
                        Some(encoded.data.clone())
                    } else {
                        None
                    };
                    if wss_media.is_some() {
                        media_sequence = media_sequence.wrapping_add(1);
                        let meta = WssAccessUnitMeta {
                            codec: wss_codec(active_codec),
                            generation: media_generation,
                            sequence: media_sequence,
                            timestamp90k: (media_clock_start.elapsed().as_secs_f64() * 90_000.0)
                                as u32,
                            key: encoded.is_idr,
                        };
                        let payload = hybrid_wss_payload
                            .take()
                            .unwrap_or_else(|| encoded.data.clone());
                        let encoded_len = payload.len();
                        wss_max_access_unit_bytes = wss_max_access_unit_bytes.max(encoded_len);
                        if encoded.is_idr {
                            wss_idr_count = wss_idr_count.saturating_add(1);
                        } else {
                            wss_delta_count = wss_delta_count.saturating_add(1);
                        }
                        if let Some(mut wss_reservation) = wss_reservation.take() {
                            wss_reservation.set_meta(meta);
                            wss_reservation.set_refine(wss_v4_refine_frame && !encoded.is_idr);
                            if matches!(wss_reservation.submit(payload), WssSubmitOutcome::Queued) {
                                if encoded.is_idr {
                                    startup_idr_admitted = Some((media_generation, "wss"));
                                }
                                if encoded.is_idr {
                                    let reason = wss_v4_key_reason
                                        .take()
                                        .unwrap_or_else(|| wss_v4_key_cause.to_string());
                                    *wss_v4_keyframes.entry(reason).or_default() += 1;
                                }
                                wss_v4_qp_window.0 = wss_v4_qp_window.0.max(encoded.frame_avg_qp);
                                wss_v4_qp_window.1 += u64::from(encoded.frame_avg_qp);
                                wss_v4_qp_window.2 += 1;
                                if let Some(tracker) = tile_tracker.as_mut() {
                                    let now = Instant::now();
                                    let refined_tiles = wss_v4_refine_plan
                                        .as_ref()
                                        .filter(|_| wss_v4_refine_frame && !encoded.is_idr)
                                        .map_or(0, |plan| {
                                            if plan.whole_frame {
                                                tracker.grid().tile_count()
                                            } else {
                                                plan.tiles.len()
                                            }
                                        });
                                    v4_pacer.on_encoded(
                                        now,
                                        encoded_len as u64,
                                        encoded.is_idr,
                                        refined_tiles,
                                        !tile_changed.is_empty(),
                                    );
                                    tracker.on_encoded(
                                        now,
                                        &tile_hashes,
                                        encoded.frame_avg_qp,
                                        encoded.is_idr,
                                        wss_v4_refine_plan.as_ref().filter(|_| wss_v4_refine_frame),
                                    );
                                    // An IDR's own follow-up comes first; the
                                    // refine is scheduled after that frame.
                                    if !encoded.is_idr {
                                        if let Some(at) = tracker.next_refine_at() {
                                            // On a constrained link, not before
                                            // the budget covers a tile.
                                            let budget_wait = if v4_pacer.capacity().is_some() {
                                                v4_pacer.refine_retry_after()
                                            } else {
                                                Duration::ZERO
                                            };
                                            capture.request_idle_followup_snapshot(
                                                at.saturating_duration_since(now).max(budget_wait),
                                            );
                                        }
                                    }
                                }
                                if active_benchmark_profile.is_some() {
                                    let wire_payload_bytes = wss_media
                                        .as_ref()
                                        .map_or(0, |egress| egress.wire_payload_bytes(encoded_len));
                                    benchmark_wss_payload_bytes = benchmark_wss_payload_bytes
                                        .saturating_add(wire_payload_bytes);
                                    if feedback_headroom_enabled {
                                        if let Some(budget) = feedback_wire_budget.as_mut() {
                                            budget.record(Instant::now(), wire_payload_bytes);
                                        }
                                    }
                                }
                                if let Some(pending) = pending_first_frame.as_mut() {
                                    let armed_generation = pending.generation;
                                    if pending
                                        .admit_wss_access_unit(media_generation, encoded.is_idr)
                                    {
                                        signaling.send_first_frame(
                                            "admitted",
                                            &pending.request_id,
                                            pending.generation,
                                            Some("wss"),
                                            pending.epoch.as_deref(),
                                            Some(active_codec.name()),
                                        );
                                        info!(
                                            armed_generation,
                                            admitted_generation = pending.generation,
                                            source = "fresh-idr",
                                            "first-frame WSS IDR admitted"
                                        );
                                    }
                                }
                                if encoded.is_idr {
                                    if let Some(request_id) = pending_lab_quality_keyframe.take() {
                                        signaling.send_lab_quality_keyframe_admitted(
                                            &request_id,
                                            media_generation,
                                            media_sequence,
                                            active_codec.name(),
                                        );
                                    }
                                    if let Some(crop) = viewport_crop.filter(|crop| {
                                        active_viewport != Some((crop.revision, media_generation))
                                            && pending_viewport_idr.is_none()
                                    }) {
                                        pending_viewport_idr = Some(PendingViewportIdr {
                                            generation: media_generation,
                                            sequence: media_sequence,
                                            crop,
                                        });
                                    }
                                    let transition_reason = fallback_reason.or_else(|| {
                                        profiles
                                            .staged
                                            .filter(|(staged_mode, _)| *staged_mode == mode)
                                            .map(|(_, reason)| reason)
                                    });
                                    if let Some(reason) = transition_reason {
                                        pending_wss_idr = Some(PendingWssIdr {
                                            generation: media_generation,
                                            sequence: media_sequence,
                                            mode,
                                            reason,
                                        });
                                    }
                                }
                            } else {
                                wss_submission_failures = wss_submission_failures.saturating_add(1);
                                wss_v4_key_reason = Some("submit-failure".into());
                                pending_wss_idr = None;
                                pending_viewport_idr = None;
                                if quiescent_hybrid_enabled {
                                    recover_quiescent_after_unsubmitted_frame(
                                        &mut quiescent_tile_planner,
                                        &mut quiescent_wire_budget,
                                    );
                                    quiescent_diff_analyzer = None;
                                }
                                if let Some(enc) = encoder.as_mut() {
                                    enc.force_idr();
                                }
                                next_idr = Instant::now();
                            }
                        } else {
                            wss_submission_failures = wss_submission_failures.saturating_add(1);
                            warn!("WSS access unit reached submit without a reservation");
                            if quiescent_hybrid_enabled {
                                recover_quiescent_after_unsubmitted_frame(
                                    &mut quiescent_tile_planner,
                                    &mut quiescent_wire_budget,
                                );
                                quiescent_diff_analyzer = None;
                            }
                            if let Some(enc) = encoder.as_mut() {
                                enc.force_idr();
                            }
                            next_idr = Instant::now();
                        }
                    }
                    if rtc_active {
                        if let Some(reservation) = rtc_reservation {
                            if reservation.submit(AccessUnit {
                                id,
                                data: rtc_data.expect("ICE data cloned above"),
                                produced_at: Instant::now(),
                                is_idr: encoded.is_idr,
                            }) && encoded.is_idr
                            {
                                startup_idr_submitted = Some((media_generation, "webrtc", id));
                                if let Some(pending) =
                                    pending_first_frame.as_mut().filter(|pending| {
                                        pending.transport == "webrtc"
                                            && !pending.admitted
                                            && pending.generation == media_generation
                                    })
                                {
                                    pending.rtc_idr_id.get_or_insert(id);
                                }
                                if let Some(reason) = fallback_reason {
                                    pending_transition_idr =
                                        Some(PendingSubmittedIdr { id, mode, reason });
                                } else if transition {
                                    pending_transition_idr = Some(PendingSubmittedIdr {
                                        id,
                                        mode,
                                        reason: "transition",
                                    });
                                }
                            }
                        }
                    }
                }
            }
        } else if capture_demand {
            skipped_before_encode += 1;
        }
        if skipped_before_encode != 0 && Instant::now() >= next_skip_log {
            info!(
                skipped_before_encode,
                "producer skipped frames before encode due to AU backpressure"
            );
            skipped_before_encode = 0;
            next_skip_log = Instant::now() + Duration::from_secs(1);
        }
        session_stage!(Publish);
        if let Some(egress) = wss_media
            .as_ref()
            .filter(|_| Instant::now() >= next_wss_stats)
        {
            let capture_stats = capture.local_capture_stats().unwrap_or(last_capture_stats);
            let window = stats_window::ProducerWindow {
                preencode_skips: wss_preencode_skips,
                submission_failures: wss_submission_failures,
                idr_count: wss_idr_count,
                delta_count: wss_delta_count,
                span_ms: producer_window_started.elapsed().as_millis() as u64,
                loop_iterations: producer_loop_iterations,
                capture_acquired: capture_stats.0.saturating_sub(last_capture_stats.0),
                capture_new: capture_stats.1.saturating_sub(last_capture_stats.1),
                max_access_unit_bytes: wss_max_access_unit_bytes,
                tile_hash_us,
                qp_max: wss_v4_qp_window.0,
                qp_sum: wss_v4_qp_window.1,
                qp_count: wss_v4_qp_window.2,
            };
            last_capture_stats = capture_stats;
            // Every second at debug; one rolled-up line a minute at info.
            wss_stats_line!(debug, egress, &window);
            if let Some(total) = stats_rollup.add(Instant::now(), &window) {
                wss_stats_line!(info, egress, &total);
            }
            tile_hash_us = 0;
            wss_v4_qp_window = (0, 0, 0);
            next_wss_stats = Instant::now() + Duration::from_secs(1);
            producer_loop_iterations = 0;
            producer_window_started = Instant::now();
            wss_preencode_skips = 0;
            wss_submission_failures = 0;
            wss_idr_count = 0;
            wss_delta_count = 0;
            wss_max_access_unit_bytes = 0;
        }
        // Every path, including an early continue, reaches the activity wait
        // at the top of the next iteration. DDA waits on its acquisition thread.
    }
    session_stage!(Cleanup);
    if let (Some(total), Some(egress)) = (stats_rollup.finish(Instant::now()), wss_media.as_ref()) {
        wss_stats_line!(info, egress, &total);
    }
    file_drag_preview.cancel();
    if let Some(pending) = pending_native_drop.take() {
        pending.request.reject("drop-not-ready");
    }
    session_call!(Input, input_sink.release_all());
    signaling.send_bye();
    drop(audio_capture);
    if let Some(egress) = wss_media {
        egress.shutdown();
    }
    pump.shutdown();
    Ok(())
}

/// Discover the local interface IP used for the default route (UDP-connect trick;
/// no packets are sent).
fn pick_host_ip() -> Result<std::net::IpAddr> {
    let probe = UdpSocket::bind("0.0.0.0:0")?;
    let stun_addr = ipv4_probe_target(
        std::net::ToSocketAddrs::to_socket_addrs(STUN_SERVER).context("resolve STUN server")?,
    )
    .context("STUN server has no IPv4 address")?;
    probe
        .connect(stun_addr)
        .context("connect IPv4 route probe")?;
    Ok(probe.local_addr()?.ip())
}

/// The route probe is bound to an IPv4 socket; connecting it to an IPv6
/// resolution result fails with WSAEAFNOSUPPORT (10047). Use IPv4 only.
fn ipv4_probe_target(addrs: impl IntoIterator<Item = SocketAddr>) -> Option<SocketAddr> {
    addrs.into_iter().find(SocketAddr::is_ipv4)
}

/// Start the inline VDD keepalive when the capture target needs a virtual
/// display: the user asked for it (`--vdd`, or a parsec/vdd monitor filter),
/// or the machine has no physical monitor at all (headless). The plug itself
/// is instant, but the driver takes ~1-3 s to attach the display to the
/// desktop; that attach is a mode change that would invalidate a DDA session
/// created in the meantime (endless AccessLost). So we wait for the virtual
/// display to enumerate before returning. Non-fatal if the driver is absent.
fn start_vdd_keepalive(args: &Args) -> Result<Option<vdd::VddKeepalive>> {
    let wants_vdd = args.vdd
        || args.monitor.as_deref().is_some_and(|f| {
            let l = f.to_lowercase();
            l.contains("parsec") || l.contains("vdd") || l.contains("virtual")
        })
        || (args.monitor.is_none() && physical_monitor_count()? == 0);
    if !wants_vdd {
        return Ok(None);
    }

    let keep = match vdd::VddKeepalive::start() {
        Ok(k) => k,
        Err(e) => {
            warn!(
                ?e,
                "VDD keepalive unavailable — physical-monitor capture only"
            );
            return Ok(None);
        }
    };

    // Wait for the driver to attach the virtual display (up to 6 s). Polling
    // `pick_monitor("parsec")` works because windows-capture only lists
    // monitors on the virtual desktop, i.e. attached ones.
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if pick_monitor(Some("parsec")).is_ok() {
            info!("virtual display attached to desktop");
            break;
        }
        if Instant::now() >= deadline {
            warn!("VDD display did not attach within 6s — continuing");
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(Some(keep))
}

/// Number of physical (non-Parsec) monitors currently on the virtual desktop.
/// Used to detect a headless machine that needs the VDD display.
fn physical_monitor_count() -> Result<usize> {
    let monitors = windows_capture::monitor::Monitor::enumerate().context("enumerate monitors")?;
    let mut n = 0usize;
    for m in &monitors {
        let name = m.name().unwrap_or_default();
        if !name.to_lowercase().contains("parsec") {
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    #[test]
    fn serve_backoff_resets_after_a_healthy_run_and_grows_on_quick_failures() {
        use std::time::Duration;
        let max = Duration::from_secs(30);
        assert_eq!(
            super::next_serve_backoff(max, Some(Duration::from_secs(120))),
            Duration::from_secs(1)
        );
        assert_eq!(
            super::next_serve_backoff(Duration::from_secs(4), Some(Duration::from_secs(5))),
            Duration::from_secs(8)
        );
        assert_eq!(
            super::next_serve_backoff(Duration::from_secs(1), None),
            Duration::from_secs(2)
        );
        assert_eq!(super::next_serve_backoff(max, None), max);
    }

    #[test]
    fn audio_cli_defaults_on_and_accepts_no_audio() {
        assert!(super::parse_args_from(std::iter::empty()).unwrap().audio);
        let args = super::parse_args_from(
            [
                "--mode",
                "serve",
                "--worker",
                "wss://example.test",
                "--no-audio",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        assert!(!args.audio);
        assert_eq!(args.mode, "serve");
        assert_eq!(args.worker.as_deref(), Some("wss://example.test"));
    }

    #[test]
    fn encoder_cli_defaults_to_auto_and_accepts_cpu_mode_and_override() {
        let defaults = super::parse_args_from(std::iter::empty()).unwrap();
        assert!(!defaults.cpu_mode);
        assert_eq!(
            defaults.encoder_preference(),
            super::EncoderPreference::default()
        );
        assert_eq!(
            defaults.encoder_preference().encoder,
            super::EncoderOverride::Auto
        );
        let args = super::parse_args_from(
            ["--cpu-mode", "--encoder", "mf-sw"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(
            args.encoder_preference(),
            super::EncoderPreference {
                cpu_mode: true,
                encoder: super::EncoderOverride::MfSoftware,
            }
        );
    }

    #[test]
    fn encoder_cli_rejects_unknown_backend() {
        let error = super::parse_args_from(["--encoder", "amf"].into_iter().map(str::to_owned))
            .unwrap_err();
        assert!(format!("{error:#}").contains("--encoder must be"));
    }

    #[test]
    fn mf_keeps_the_session_on_h264_when_hevc_is_requested() {
        // Media Foundation emits H.264 only. An HEVC request must not switch a
        // session that is already on H.264, and an H.264 session stays put.
        assert_eq!(
            super::EncoderCapabilities::MF.effective_codec(super::VideoCodec::Hevc),
            super::VideoCodec::H264
        );
        assert!(!super::accept_session_codec(
            super::VideoCodec::H264,
            super::VideoCodec::Hevc
        ));
        assert!(super::accept_session_codec(
            super::VideoCodec::Hevc,
            super::VideoCodec::H264
        ));
    }

    #[test]
    fn startup_offer_is_answered_without_any_capture_frame() {
        use super::EncoderBackend;
        use str0m::media::{Direction, MediaKind};
        for backend in [
            EncoderBackend::Nvenc,
            EncoderBackend::MfHardware,
            EncoderBackend::MfSoftware,
        ] {
            let mut viewer = str0m::Rtc::builder()
                .enable_h265(true)
                .build(std::time::Instant::now());
            let mut change = viewer.sdp_api();
            change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
            let (offer, _) = change.apply().unwrap();
            let command = super::startup_offer_command(offer.to_sdp_string(), backend);
            assert!(matches!(
                (&command, backend),
                (super::TransportCommand::Offer(_), EncoderBackend::Nvenc)
                    | (
                        super::TransportCommand::OfferWithCapabilities(_, _),
                        EncoderBackend::MfHardware | EncoderBackend::MfSoftware
                    )
            ));
            // Use the production command channel and actual answer event. No
            // CaptureSource, D3D device, encoder or desktop frame is created.
            let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let pump = super::TransportPump::spawn(socket, None, None, 2_000_000);
            assert!(pump.command(command));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let answer = loop {
                match pump.try_event() {
                    Some(super::TransportEvent::Answer(answer)) => break answer,
                    Some(super::TransportEvent::Stopped) => {
                        panic!("transport stopped before answering frame-free offer")
                    }
                    _ => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "offer required a capture frame or failed to answer"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            };
            assert!(answer.contains("H264/90000"));
            assert_eq!(
                answer.contains("H265/90000"),
                backend == EncoderBackend::Nvenc
            );
            pump.shutdown();
        }
    }

    #[test]
    fn rtc_reconnect_preserves_the_session_downgrade_and_rejects_codec_mismatch() {
        use super::{EncoderBackend, VideoCodec};
        for backend in [EncoderBackend::MfHardware, EncoderBackend::MfSoftware] {
            assert!(
                super::ensure_rtc_encoder_codec(backend, false, None, VideoCodec::H264).is_ok()
            );
            assert!(super::ensure_rtc_encoder_codec(
                backend,
                false,
                Some(VideoCodec::H264),
                VideoCodec::H264
            )
            .is_ok());
            assert!(super::ensure_rtc_encoder_codec(
                backend,
                false,
                Some(VideoCodec::Hevc),
                VideoCodec::H264
            )
            .is_err());
            assert!(super::ensure_rtc_encoder_codec(
                backend,
                true,
                Some(VideoCodec::Hevc),
                VideoCodec::H264
            )
            .is_ok());
        }
        // A warm NVENC session stays on H.264 when a later offer chooses
        // HEVC. Preserve master's behavior instead of ending that session.
        assert!(super::ensure_rtc_encoder_codec(
            EncoderBackend::Nvenc,
            false,
            Some(VideoCodec::Hevc),
            VideoCodec::H264
        )
        .is_ok());
    }

    #[test]
    fn encoder_cli_accepts_every_backend_name_and_rejects_a_missing_value() {
        use super::EncoderOverride;
        for (name, expected) in [
            ("auto", EncoderOverride::Auto),
            ("nvenc", EncoderOverride::Nvenc),
            ("mf-hw", EncoderOverride::MfHardware),
            ("mf-sw", EncoderOverride::MfSoftware),
        ] {
            let args =
                super::parse_args_from(["--encoder", name].into_iter().map(str::to_owned)).unwrap();
            assert_eq!(args.encoder, expected, "--encoder {name}");
            assert!(!args.cpu_mode);
        }
        assert!(super::parse_args_from(["--encoder"].into_iter().map(str::to_owned)).is_err());
    }

    #[test]
    fn encoder_capabilities_decide_the_emitted_codec_and_features() {
        use super::VideoCodec;
        let nvenc = super::EncoderCapabilities::NVENC;
        let mf = super::EncoderCapabilities::MF;
        for requested in [VideoCodec::Hevc, VideoCodec::H264] {
            assert_eq!(nvenc.effective_codec(requested), requested);
            assert_eq!(mf.effective_codec(requested), VideoCodec::H264);
        }
        assert!(nvenc.supports(VideoCodec::Hevc));
        assert!(!mf.supports(VideoCodec::Hevc));
        assert!(mf.supports(VideoCodec::H264));
        // Media Foundation has no Main10, QP map, feedback or HEVC tiles.
        assert!(!(mf.main10 || mf.roi_qp_map || mf.refine_feedback || mf.hevc_tiles));
        assert!(nvenc.main10 && nvenc.roi_qp_map && nvenc.refine_feedback && nvenc.hevc_tiles);
    }

    #[test]
    fn main10_conversion_follows_the_created_encoder() {
        let nvenc = super::EncoderCapabilities::NVENC;
        let mf = super::EncoderCapabilities::MF;
        assert!(!super::main10_conversion(false, mf));
        assert!(!super::main10_conversion(false, nvenc));
        assert!(super::main10_conversion(true, nvenc));
        assert!(!super::main10_conversion(true, mf));
    }

    #[test]
    fn recovery_bgra_provider_is_lazy_and_only_runs_for_main10_to_8bit() {
        for (main10, capabilities) in [
            (false, super::EncoderCapabilities::MF),
            (false, super::EncoderCapabilities::NVENC),
            (true, super::EncoderCapabilities::NVENC),
        ] {
            let input = super::optional_bgra_recovery_input::<&str>(main10, capabilities, || {
                panic!("existing NVENC/BGRA input must not trigger extra scaling")
            })
            .unwrap();
            assert!(input.is_none());
        }
        let calls = std::cell::Cell::new(0);
        let input =
            super::optional_bgra_recovery_input(true, super::EncoderCapabilities::MF, || {
                calls.set(calls.get() + 1);
                Ok("retained BGRA")
            })
            .unwrap();
        assert_eq!(input, Some("retained BGRA"));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn prepared_drop_input_keeps_motion_and_cleanup_but_invalidates_actions() {
        for packet in [
            vec![1, 65, 0],
            vec![4, 0],
            vec![6],
            vec![10, 0, 0],
            vec![2, 1, 0, 0, 0],
            vec![3, 1, 0, 2, 0],
            vec![7, 1, 0, 1, 0, 2, 0, 0, 0, 0, 0],
            vec![8, 1, 0, 1, 0, 2, 0],
            vec![9, 1, 0, 1, 0, 2, 0, 0],
            vec![13, 1, 30, 0, 65, 0],
        ] {
            assert!(
                !super::prepared_drop_input_invalidates(&packet),
                "allowed {packet:?}"
            );
        }
        for packet in [
            vec![0, 65, 0],
            vec![4, 1],
            vec![5, 1, 0],
            vec![9, 1, 0, 1, 0, 2, 0, 1],
            vec![11, 65],
            vec![12, 1, 1, 0, 0, 0, 1, 0],
            vec![13, 0, 30, 0, 65, 0],
            vec![7, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0],
            vec![7, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0],
        ] {
            assert!(
                super::prepared_drop_input_invalidates(&packet),
                "invalidating {packet:?}"
            );
        }
    }
    fn preview_control() -> super::signaling::FileDragPreviewControl {
        super::signaling::FileDragPreviewControl {
            protocol: 1,
            epoch: "0123456789abcdef0123456789abcdef".into(),
            drag_id: 7,
            action: "update".into(),
            drop: Some(bubbleplay_host::file_transfer::DropPoint {
                x: 123,
                y: 456,
                generation: 3,
                viewport_revision: 2,
            }),
            types: Some(vec!["image/png".into()]),
        }
    }
    #[test]
    fn native_preview_stale_binding_and_missing_heartbeat_never_start_hover() {
        let now = std::time::Instant::now();
        for (ready, epoch, generation, revision, age) in [
            (false, Some("0123456789abcdef0123456789abcdef"), 3, 2, 0),
            (true, None, 3, 2, 0),
            (true, Some("fedcba9876543210fedcba9876543210"), 3, 2, 0),
            (true, Some("0123456789abcdef0123456789abcdef"), 4, 2, 0),
            (true, Some("0123456789abcdef0123456789abcdef"), 3, 4, 0),
            (true, Some("0123456789abcdef0123456789abcdef"), 3, 2, 2),
        ] {
            let mut state = super::FileDragPreviewState::default();
            state.control(preview_control(), now);
            state.tick(
                super::NativeDropView {
                    ready,
                    epoch,
                    generation,
                    revision,
                },
                super::DesktopRegion::new(0, 0, 1920, 1080),
                now + std::time::Duration::from_secs(age),
            );
            assert!(state.desired.is_none());
            assert!(state.active.is_none());
            assert!(state.attempted.is_none());
        }
    }
    #[test]
    fn native_preview_cancel_is_scoped_and_failed_drag_is_not_retried() {
        let now = std::time::Instant::now();
        let mut state = super::FileDragPreviewState::default();
        let update = preview_control();
        state.control(update.clone(), now);
        let mut cancel = update.clone();
        cancel.action = "cancel".into();
        cancel.drop = None;
        cancel.types = None;
        cancel.drag_id += 1;
        state.control(cancel.clone(), now);
        assert!(
            state.desired.is_some(),
            "unrelated cancel must not clear another drag"
        );
        state.attempted = Some((update.drag_id, update.epoch.clone()));
        state.tick(
            super::NativeDropView {
                ready: true,
                epoch: Some(&update.epoch),
                generation: 3,
                revision: 2,
            },
            super::DesktopRegion::new(0, 0, 1920, 1080),
            now,
        );
        assert!(
            state.active.is_none(),
            "same refused/expired drag must not repeatedly attach queues"
        );
        cancel.drag_id = update.drag_id;
        state.control(cancel, now);
        assert!(state.desired.is_none());
    }
    #[test]
    fn native_drop_uses_full_monitor_bounds_including_negative_origins() {
        let region = super::DesktopRegion::new(-1920, -100, 1920, 1080);
        let first = super::native_drop_pixel(0, 0, region);
        let last = super::native_drop_pixel(u16::MAX, u16::MAX, region);
        assert_eq!((first.x, first.y), (-1920, -100));
        assert_eq!((last.x, last.y), (-1, 979));
        let tiny =
            super::native_drop_pixel(u16::MAX, u16::MAX, super::DesktopRegion::new(3, 4, 1, 1));
        assert_eq!((tiny.x, tiny.y), (3, 4));
    }
    #[test]
    fn host_ip_probe_skips_ipv6_resolution_results() {
        let v6: std::net::SocketAddr = "[2001:4860:4864:5:8000::1]:19302".parse().unwrap();
        let v4: std::net::SocketAddr = "74.125.250.129:19302".parse().unwrap();
        assert_eq!(super::ipv4_probe_target([v6, v4]), Some(v4));
        assert_eq!(super::ipv4_probe_target([v6]), None);
        assert_eq!(super::ipv4_probe_target([]), None);
    }

    #[test]
    fn wss_v4_refine_bitrate_is_bounded_and_v4_has_no_periodic_idr() {
        assert_eq!(super::wss_v4_refine_bitrate(3_000_000), 12_000_000);
        assert_eq!(
            super::wss_v4_refine_bitrate(20_000_000),
            super::MAX_BITRATE_BPS
        );
        assert_eq!(
            super::wss_v4_refine_bitrate(u32::MAX),
            super::MAX_BITRATE_BPS
        );
        let now = std::time::Instant::now();
        assert!(super::periodic_idr_due(now, now, false));
        assert!(!super::periodic_idr_due(now, now, true));
        assert!(!super::periodic_idr_due(
            now + std::time::Duration::from_secs(86_400),
            now,
            true
        ));
        assert_eq!(super::SIGNALING_HEARTBEAT_INTERVAL.as_secs(), 15);
    }

    use super::{
        active_encoder_dimensions, advance_benchmark_profile_revision, advance_frame_deadline,
        benchmark_encoder_options, benchmark_mode_matches, benchmark_owned_encoder_bitrate,
        benchmark_status_retry_delay, cached_replay_transition, capture_admission_ready,
        coalesce_quiescent_cache_round_trips, commit_matching_transition,
        encoder_benchmark_contract_changed, encoder_mode_for_capture, fallback_mode,
        frame_admission_due, full_profile_uses_native_texture, idle_snapshot_completion_order,
        input_ready_after_presentation, input_ready_on_offer, lab_quality_keyframe_request,
        producer_wait_duration, quiescent_damage_rects, quiescent_immediate_damage_rects,
        quiescent_transition_followup_needed, quiescent_transition_upsert_allowed,
        quiescent_validation_requests, record_benchmark_encode_stages,
        record_benchmark_feedback_plan, record_benchmark_feedback_submission,
        record_benchmark_hybrid_validation, record_benchmark_roi_activity,
        record_benchmark_temporal_activity, recover_quiescent_after_unsubmitted_frame,
        rejected_transition_requires_recovery, remembered_monitor_attempt_exhausted,
        same_viewport_geometry, session_capture_selector, should_forward_offer,
        should_replace_wss_media_session, should_reset_lab_encoder_for_keyframe,
        should_snapshot_idle_desktop, take_matching_transition, temporal_readback_policy,
        valid_lab_quality_request_id, valid_monitor_refresh_control, viewport_from_q16, CachedIdr,
        CachedIdrReplay, CaptureSelection, DesktopState, FirstFrameRecovery,
        IdleSnapshotCompletionOrder, PendingFirstFrame, PendingSubmittedIdr, ProfileTransition,
        TemporalReadbackPolicy, VideoMode, ViewportCrop, REMEMBERED_MONITOR_MAX_ATTEMPTS,
        STATIC_REPLAY_DELAY,
    };
    use crate::encode::{
        AqMode, EncodeStageTimings, Multipass, Preset, RateControlMode, ResourceMode, RoiMapReason,
        RoiMapTelemetry, RoiMode, Tuning,
    };
    use crate::feedback_roi::{
        FeedbackBudgetPolicy, FeedbackPlan, FeedbackPlanReason, FeedbackWireDecision,
    };
    use crate::frame_damage::DamageRect;
    use crate::hybrid::{
        HybridTileOperation, HybridWireBudget, QuiescentTilePlanner, ViewportGeometry,
    };
    use crate::quality::{Profile, StreamMode};
    use crate::reconstruction_feedback::ReconstructionFeedbackSubmission;
    use crate::signaling::{BenchEncodeStages, BenchHybridActivity, BenchRoiActivity};
    use crate::temporal_diff::{
        DamageCoverageAudit, DeferredReadbackActivity, QuiescentValidationStatus,
        TemporalDiffResult, TemporalMaskDecision, TemporalMaskRejectReason,
    };
    use crate::{BenchmarkHybridCacheMode, BenchmarkStreamMode, BenchmarkTransport, VideoCodec};
    use std::time::{Duration, Instant};

    #[test]
    fn remembered_capture_selection_survives_a_session_restart() {
        let mut selection = CaptureSelection::from_launch(None);
        assert_eq!(selection.selector(), None);
        assert!(!selection.is_runtime());
        assert!(selection.remember("\\\\.\\DISPLAY5".into()));
        assert_eq!(selection.selector(), Some("\\\\.\\DISPLAY5"));
        assert!(selection.is_runtime());
        // A restarted session reopens the remembered display, not the launch
        // default that the candidate host was started with.
        assert_eq!(
            session_capture_selector(selection.selector(), None),
            Some("\\\\.\\DISPLAY5")
        );
    }

    #[test]
    fn launch_selector_only_applies_until_a_live_switch_is_remembered() {
        let mut selection = CaptureSelection::from_launch(Some("primary"));
        assert_eq!(
            session_capture_selector(selection.selector(), Some("primary")),
            Some("primary")
        );
        // The launch selector keeps the original unbounded serve retry; only a
        // runtime selection is bounded before the host refuses to substitute.
        assert!(!selection.is_runtime());
        assert!(selection.remember("\\\\.\\DISPLAY5".into()));
        assert!(selection.is_runtime());
        assert_eq!(
            session_capture_selector(selection.selector(), Some("primary")),
            Some("\\\\.\\DISPLAY5")
        );
    }

    #[test]
    fn an_empty_selector_is_never_remembered() {
        let mut selection = CaptureSelection::from_launch(None);
        assert!(!selection.remember(String::new()));
        assert_eq!(selection.selector(), None);
        assert!(!selection.is_runtime());
    }

    #[test]
    fn a_vanished_remembered_monitor_is_retried_boundedly_without_substitution() {
        assert!(!remembered_monitor_attempt_exhausted(0));
        assert!(!remembered_monitor_attempt_exhausted(
            REMEMBERED_MONITOR_MAX_ATTEMPTS - 1
        ));
        assert!(remembered_monitor_attempt_exhausted(
            REMEMBERED_MONITOR_MAX_ATTEMPTS
        ));
        // Precedence stays with the remembered selector, so the launch default
        // can never become an unintended capture target.
        assert_eq!(
            session_capture_selector(Some("\\\\.\\DISPLAY5"), Some("primary")),
            Some("\\\\.\\DISPLAY5")
        );
    }

    #[test]
    fn new_browser_epoch_replaces_the_active_wss_media_session() {
        let old_epoch = "0123456789abcdef0123456789abcdef";
        let new_epoch = "fedcba9876543210fedcba9876543210";
        assert!(!should_replace_wss_media_session(false, None, new_epoch));
        assert!(!should_replace_wss_media_session(
            true,
            Some(old_epoch),
            old_epoch
        ));
        assert!(should_replace_wss_media_session(
            true,
            Some(old_epoch),
            new_epoch
        ));
    }

    #[test]
    fn benchmark_profile_revision_ignores_renewal_but_advances_for_effective_change() {
        let profile: crate::benchmark::BenchmarkProfile =
            serde_json::from_value(serde_json::json!({
                "schemaVersion": 1,
                "runId": "123e4567-e89b-42d3-a456-426614174000",
                "expiresAtMs": 10_000,
                "transport": "wss",
                "codec": "hevc",
                "streamMode": "work",
                "adaptive": false,
                "width": 1920,
                "height": 1080,
                "fps": 30,
                "bitrateBps": 4_000_000,
                "gopSeconds": 2,
                "encoder": {
                    "preset": "p5",
                    "tuning": "low-latency",
                    "multipass": "quarter",
                    "aqStrength": 10,
                    "temporalAq": true,
                    "vbvFrames": 2
                }
            }))
            .unwrap();
        let mut renewal = profile.clone();
        renewal.expires_at_ms += 1_000;
        let mut changed = profile.clone();
        changed.bitrate_bps += 500_000;

        assert_eq!(profile, renewal);
        assert_ne!(profile, changed);
        assert_eq!(advance_benchmark_profile_revision(7, profile != renewal), 7);
        assert_eq!(advance_benchmark_profile_revision(7, profile != changed), 8);
    }

    #[test]
    fn lab_encoder_reset_is_scoped_to_diagnostic_quality_keyframes() {
        assert_eq!(
            lab_quality_keyframe_request(
                "keyframe",
                Some("stream-lab-quality-capture"),
                Some("quality-01234567-89ab-cdef-0123-456789abcdef"),
            ),
            Some("quality-01234567-89ab-cdef-0123-456789abcdef")
        );
        assert_eq!(
            lab_quality_keyframe_request(
                "keyframe",
                Some("stream-lab-quality-capture"),
                Some("quality request"),
            ),
            None
        );
        assert_eq!(
            lab_quality_keyframe_request(
                "keyframe",
                Some("diagnostic-injected-fault"),
                Some("quality-01234567-89ab-cdef-0123-456789abcdef"),
            ),
            None
        );
        assert!(should_reset_lab_encoder_for_keyframe(
            true,
            "keyframe",
            Some("stream-lab-quality-capture")
        ));
        assert!(!should_reset_lab_encoder_for_keyframe(
            false,
            "keyframe",
            Some("stream-lab-quality-capture")
        ));
        assert!(!should_reset_lab_encoder_for_keyframe(
            true,
            "start",
            Some("stream-lab-quality-capture")
        ));
        assert!(!should_reset_lab_encoder_for_keyframe(
            true,
            "keyframe",
            Some("diagnostic-injected-fault")
        ));
        assert!(valid_lab_quality_request_id(
            "quality-01234567-89ab-cdef-0123-456789abcdef"
        ));
        assert!(!valid_lab_quality_request_id("quality request"));
    }

    #[test]
    fn benchmark_status_remains_due_until_transport_and_profile_match() {
        assert_eq!(benchmark_status_retry_delay(false), Duration::ZERO);
        assert_eq!(benchmark_status_retry_delay(true), Duration::from_secs(1));
    }

    #[test]
    fn local_candidate_maps_every_requested_nvenc_option() {
        let profile: crate::benchmark::BenchmarkProfile =
            serde_json::from_value(serde_json::json!({
                "schemaVersion": 2,
                "runId": "123e4567-e89b-42d3-a456-426614174000",
                "expiresAtMs": 10_000,
                "implementationVariant": "candidate",
                "experimentId": "full-option-proof",
                "transport": "wss",
                "codec": "hevc",
                "streamMode": "work",
                "adaptive": false,
                "width": 1920,
                "height": 1080,
                "fps": 30,
                "bitrateBps": 8_000_000,
                "gopSeconds": 4,
                "encoder": {
                    "preset": "p7",
                    "tuning": "low-latency",
                    "multipass": "full",
                    "aqMode": "temporal",
                    "aqStrength": 15,
                    "vbvFrames": 4,
                    "lookaheadDepth": 2,
                    "weightedPrediction": true,
                    "dirtyQpMap": true
                }
            }))
            .unwrap();

        let options = benchmark_encoder_options(&profile);
        assert_eq!(options.preset, Preset::P7);
        assert_eq!(options.tuning, Tuning::LowLatency);
        assert_eq!(options.multipass, Multipass::Full);
        assert_eq!(options.aq_mode, AqMode::Temporal);
        assert_eq!(options.aq_strength, 15);
        assert_eq!(options.vbv_frames, 4);
        assert_eq!(options.lookahead_depth, 2);
        assert!(options.weighted_prediction);
        assert_eq!(options.gop_seconds, 4);
        assert_eq!(options.roi_mode, RoiMode::Damage);
        assert_eq!(options.resource_mode, ResourceMode::PerFrame);
        assert_eq!(options.async_depth, 0);
        assert_eq!(options.rate_control, RateControlMode::Cbr);

        let mut vbr_profile = profile;
        vbr_profile.encoder.dirty_qp_map = false;
        vbr_profile.encoder.roi_mode = crate::benchmark::BenchmarkRoiMode::Off;
        vbr_profile.encoder.rate_control_mode =
            crate::benchmark::BenchmarkRateControlMode::ExternalBudgetVbr;
        vbr_profile.encoder.target_quality = Some(23);
        vbr_profile.encoder.peak_bitrate_multiplier = Some(4);
        let vbr_options = benchmark_encoder_options(&vbr_profile);
        assert_eq!(
            vbr_options.rate_control,
            RateControlMode::ExternalBudgetVbr {
                target_quality: 23,
                peak_bitrate_multiplier: 4,
            }
        );
    }

    #[test]
    fn benchmark_roi_activity_keeps_reason_counts_and_the_exact_media_sequence() {
        let mut activity = Some(BenchRoiActivity {
            mode: "damage".into(),
            ..BenchRoiActivity::default()
        });
        record_benchmark_roi_activity(
            &mut activity,
            RoiMapTelemetry {
                reason: RoiMapReason::Applied,
                changed_ctbs: 120,
                total_ctbs: 2_040,
                min_delta: Some(-3),
                max_delta: Some(1),
            },
            Some(44),
        );
        record_benchmark_roi_activity(
            &mut activity,
            RoiMapTelemetry {
                reason: RoiMapReason::MissingDamage,
                changed_ctbs: 0,
                total_ctbs: 0,
                min_delta: None,
                max_delta: None,
            },
            Some(45),
        );
        let activity = activity.unwrap();
        assert_eq!(activity.evaluated_frames, 2);
        assert_eq!(activity.mapped_frames, 1);
        assert_eq!(activity.missing_damage_frames, 1);
        assert_eq!(activity.last_sequence, Some(45));
        assert_eq!(activity.last_reason.as_deref(), Some("missing-damage"));
    }

    #[test]
    fn benchmark_temporal_activity_records_source_timing_priming_and_ctb_evidence() {
        let changed = crate::frame_damage::DamageRect {
            left: 32,
            top: 0,
            right: 64,
            bottom: 32,
        };
        let mut activity = Some(BenchRoiActivity {
            mode: "damage".into(),
            ..BenchRoiActivity::default()
        });
        let mut samples = Vec::new();
        record_benchmark_temporal_activity(
            &mut activity,
            &mut samples,
            &TemporalDiffResult {
                decision: TemporalMaskDecision::Rejected {
                    reason: TemporalMaskRejectReason::EmptyDamage,
                    changed_ctbs: 0,
                    total_ctbs: 2_040,
                },
                damage_rects: Vec::new(),
                immediate_damage_rects: Vec::new(),
                edge_scores: Vec::new(),
                analysis_us: 120,
                priming: true,
                damage_audit: None,
            },
            Some(43),
        );
        record_benchmark_temporal_activity(
            &mut activity,
            &mut samples,
            &TemporalDiffResult {
                decision: TemporalMaskDecision::Applied {
                    rects: vec![changed],
                    changed_ctbs: 1,
                    total_ctbs: 2_040,
                },
                damage_rects: vec![changed],
                immediate_damage_rects: Vec::new(),
                edge_scores: vec![0; 2_040],
                analysis_us: 1_800,
                priming: false,
                damage_audit: None,
            },
            Some(44),
        );

        let activity = activity.unwrap();
        assert_eq!(activity.source.as_deref(), Some("gpu-temporal"));
        assert_eq!(activity.temporal_frames, 2);
        assert_eq!(activity.temporal_priming_frames, 1);
        assert_eq!(activity.analysis_p95_us, Some(1_800));
        assert_eq!(activity.analysis_max_us, Some(1_800));
        assert_eq!(activity.last_sequence, Some(44));
        assert_eq!(activity.last_changed_ctbs, Some(1));
        assert_eq!(activity.last_total_ctbs, Some(2_040));
    }

    #[test]
    fn benchmark_feedback_activity_keeps_funded_plan_and_gpu_timing_evidence() {
        let mut activity = Some(BenchRoiActivity::feedback());
        let mut wire_samples = Vec::new();
        record_benchmark_feedback_plan(
            &mut activity,
            &mut wire_samples,
            &FeedbackPlan {
                reason: FeedbackPlanReason::Applied,
                map: None,
                eligible_ctbs: 12,
                refined_ctbs: 2,
                funded_ctbs: 4,
                feedback_frames: 10,
            },
            None,
        );
        let mut samples = Vec::new();
        record_benchmark_feedback_submission(
            &mut activity,
            &mut samples,
            ReconstructionFeedbackSubmission {
                submitted: true,
                submit_us: 80,
            },
        );
        record_benchmark_feedback_submission(
            &mut activity,
            &mut samples,
            ReconstructionFeedbackSubmission {
                submitted: true,
                submit_us: 120,
            },
        );
        record_benchmark_feedback_submission(
            &mut activity,
            &mut samples,
            ReconstructionFeedbackSubmission {
                submitted: false,
                submit_us: 0,
            },
        );

        let activity = activity.unwrap();
        assert_eq!(activity.feedback_adjusted_frames, Some(1));
        assert_eq!(activity.feedback_eligible_ctbs, Some(12));
        assert_eq!(activity.feedback_refined_ctbs, Some(2));
        assert_eq!(activity.feedback_funded_ctbs, Some(4));
        assert_eq!(activity.feedback_submitted_frames, Some(2));
        assert_eq!(activity.feedback_skipped_frames, Some(1));
        assert_eq!(activity.feedback_submit_p95_us, Some(120));
        assert_eq!(activity.feedback_submit_max_us, Some(120));
    }

    #[test]
    fn benchmark_feedback_headroom_records_exact_policy_and_wire_occupancy() {
        let mut activity = Some(BenchRoiActivity::feedback_headroom());
        let mut wire_samples = Vec::new();
        record_benchmark_feedback_plan(
            &mut activity,
            &mut wire_samples,
            &FeedbackPlan {
                reason: FeedbackPlanReason::Applied,
                map: None,
                eligible_ctbs: 12,
                refined_ctbs: 2,
                funded_ctbs: 0,
                feedback_frames: 10,
            },
            Some(FeedbackWireDecision {
                policy: FeedbackBudgetPolicy::SpendHeadroom,
                occupancy_bps: 1_900_000,
            }),
        );

        let activity = activity.unwrap();
        assert_eq!(activity.feedback_spend_headroom_frames, Some(1));
        assert_eq!(activity.feedback_net_zero_frames, Some(0));
        assert_eq!(activity.feedback_wire_occupancy_p95_bps, Some(1_900_000));
        assert_eq!(activity.feedback_wire_occupancy_max_bps, Some(1_900_000));
        assert_eq!(
            activity.last_feedback_budget_policy.as_deref(),
            Some("spend-headroom")
        );
        assert_eq!(activity.last_feedback_wire_occupancy_bps, Some(1_900_000));
    }

    #[test]
    fn feedback_temporal_analysis_is_deferred_while_hybrid_validation_stays_aligned() {
        assert_eq!(
            temporal_readback_policy(false, true),
            TemporalReadbackPolicy::Deferred
        );
        assert_eq!(
            temporal_readback_policy(true, false),
            TemporalReadbackPolicy::Aligned
        );
        assert_eq!(
            temporal_readback_policy(false, false),
            TemporalReadbackPolicy::Deferred
        );
    }

    #[test]
    fn benchmark_hybrid_validation_records_deferred_age_and_aligned_damage_audit() {
        let mut activity = Some(BenchHybridActivity {
            mode: "png-quiescent-detail".into(),
            ..BenchHybridActivity::default()
        });
        let mut submit_samples = Vec::new();
        let mut collect_samples = Vec::new();
        let mut age_samples = Vec::new();

        record_benchmark_hybrid_validation(
            &mut activity,
            &mut submit_samples,
            &mut collect_samples,
            &mut age_samples,
            140,
            8,
            QuiescentValidationStatus::Unavailable,
            &DeferredReadbackActivity {
                ready_ages_frames: vec![1, 2],
                pending_frames: 1,
                max_pending_age_frames: Some(3),
                verdicts: Vec::new(),
            },
            Some(DamageCoverageAudit {
                metadata_available: true,
                changed_ctbs: 5,
                covered_ctbs: 4,
                missed_ctbs: 1,
            }),
            44,
        );

        let activity = activity.unwrap();
        assert_eq!(activity.deferred_validation_ready_frames, 2);
        assert_eq!(activity.deferred_validation_pending_frames, 1);
        assert_eq!(activity.deferred_validation_age_p95_frames, Some(2));
        assert_eq!(activity.deferred_validation_age_max_frames, Some(2));
        assert_eq!(activity.deferred_validation_pending_max_age_frames, Some(3));
        assert_eq!(activity.damage_audit_frames, 1);
        assert_eq!(activity.damage_audit_metadata_unavailable_frames, 0);
        assert_eq!(activity.damage_audit_changed_ctbs, 5);
        assert_eq!(activity.damage_audit_covered_ctbs, 4);
        assert_eq!(activity.damage_audit_missed_ctbs, 1);
    }

    #[test]
    fn quiescent_transition_requests_one_followup_for_activation_or_budget_retry() {
        assert!(quiescent_transition_followup_needed(true, 1, 0, 0));
        assert!(quiescent_transition_followup_needed(true, 0, 1, 0));
        assert!(quiescent_transition_followup_needed(false, 0, 0, 1));
        assert!(!quiescent_transition_followup_needed(true, 0, 0, 0));
        assert!(!quiescent_transition_followup_needed(false, 1, 1, 0));
    }

    #[test]
    fn same_frame_unchanged_validation_emits_no_browser_cache_round_trip() {
        let mut operations = vec![
            HybridTileOperation::Suppress {
                tile_id: 7,
                content_revision: 41,
                validation_sequence: 52,
            },
            HybridTileOperation::Reactivate {
                tile_id: 7,
                content_revision: 41,
                validation_sequence: 52,
            },
        ];

        coalesce_quiescent_cache_round_trips(&mut operations);

        assert!(operations.is_empty());
    }

    #[test]
    fn suppress_reactivate_validation_is_gated_by_current_dda_damage() {
        let viewport = ViewportGeometry {
            generation: 1,
            crop_revision: 0,
            source: DamageRect {
                left: 0,
                top: 0,
                right: 256,
                bottom: 128,
            },
            output_width: 256,
            output_height: 128,
        };
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        let mut candidate_tile = None;
        for _ in 0..3 {
            candidate_tile = planner
                .plan(256, 128, viewport, &[], &edges)
                .candidates
                .first()
                .map(|candidate| candidate.tile_id)
                .or(candidate_tile);
        }
        let tile_id = candidate_tile.expect("stable detail candidate");
        assert!(planner.commit_candidate(tile_id, 10, 12_345));

        assert!(
            quiescent_validation_requests(&planner, viewport, 11, Some(&[]), false,).is_empty()
        );

        let damage = [DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 128,
        }];
        assert_eq!(
            quiescent_validation_requests(&planner, viewport, 12, Some(&damage), false,).len(),
            1
        );
    }

    #[test]
    fn quiescent_transition_sends_one_progressive_upsert_before_followup_fill() {
        assert!(quiescent_transition_upsert_allowed(true, 0));
        assert!(!quiescent_transition_upsert_allowed(true, 1));
        assert!(quiescent_transition_upsert_allowed(false, 0));
        assert!(quiescent_transition_upsert_allowed(false, 3));
    }

    #[test]
    fn unsubmitted_quiescent_frame_forgets_mutations_and_clears_the_next_delivery() {
        let viewport = ViewportGeometry {
            generation: 7,
            crop_revision: 3,
            source: DamageRect {
                left: 0,
                top: 0,
                right: 256,
                bottom: 128,
            },
            output_width: 256,
            output_height: 128,
        };
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, viewport, &[], &edges);
        }
        assert!(planner.commit_candidate(1, 10, 12_345));
        let requests = planner.validation_requests(
            viewport,
            11,
            Some(&[DamageRect {
                left: 0,
                top: 0,
                right: 128,
                bottom: 128,
            }]),
            false,
        );
        assert_eq!(planner.suppress_requests(&requests).len(), 1);

        let mut budget = HybridWireBudget::default();
        assert!(budget.begin_frame(6_000_000, 30, 16, 15, 1_000) > 0);
        recover_quiescent_after_unsubmitted_frame(&mut planner, &mut budget);

        let next = planner.plan(256, 128, viewport, &[], &edges);
        assert!(next.clear);
        assert_eq!(next.active_before_planning, 0);
        assert_eq!(planner.suppressed_tile_count(), 0);
        assert_eq!(planner.retained_png_bytes(), 0);
        assert_eq!(budget.carried_bytes(), 0);
    }

    #[test]
    fn quiescent_damage_uses_aligned_pixel_changes_and_keeps_idle_snapshots_neutral() {
        let aligned_pixel_diff = [crate::frame_damage::DamageRect {
            left: 64,
            top: 64,
            right: 96,
            bottom: 96,
        }];
        let broad_surface_repaint = [crate::frame_damage::DamageRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        }];

        assert_eq!(
            quiescent_damage_rects(Some(&broad_surface_repaint), &aligned_pixel_diff, false),
            aligned_pixel_diff
        );
        assert!(quiescent_damage_rects(None, &aligned_pixel_diff, true).is_empty());
        assert_eq!(
            quiescent_damage_rects(None, &aligned_pixel_diff, false),
            aligned_pixel_diff
        );
    }

    #[test]
    fn quiescent_cache_policy_selects_dda_or_verified_immediate_damage() {
        let capture_damage = [DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 128,
        }];
        let verified_damage = [DamageRect {
            left: 32,
            top: 32,
            right: 64,
            bottom: 64,
        }];

        assert_eq!(
            quiescent_immediate_damage_rects(
                BenchmarkHybridCacheMode::DdaGated,
                Some(&capture_damage),
                &verified_damage,
                false,
            ),
            capture_damage
        );
        assert_eq!(
            quiescent_immediate_damage_rects(
                BenchmarkHybridCacheMode::SuppressReactivate,
                Some(&capture_damage),
                &verified_damage,
                false,
            ),
            verified_damage
        );
        assert!(quiescent_immediate_damage_rects(
            BenchmarkHybridCacheMode::DdaGated,
            Some(&capture_damage),
            &verified_damage,
            true,
        )
        .is_empty());
    }

    #[test]
    fn quiescent_followup_is_rearmed_after_acknowledging_the_current_idle_idr() {
        assert_eq!(
            idle_snapshot_completion_order(true, true),
            IdleSnapshotCompletionOrder::AcknowledgeThenRequest
        );
    }

    #[test]
    fn benchmark_encode_stages_accumulate_call_counts_totals_and_maxima() {
        let mut stages = BenchEncodeStages::default();
        record_benchmark_encode_stages(
            &mut stages,
            EncodeStageTimings {
                register_calls: 1,
                register_us: 120,
                map_us: 40,
                submit_us: 1_100,
                lock_us: 500,
                copy_us: 20,
                unmap_us: 30,
                unregister_calls: 1,
                unregister_us: 80,
            },
        );
        record_benchmark_encode_stages(
            &mut stages,
            EncodeStageTimings {
                register_calls: 0,
                register_us: 0,
                map_us: 50,
                submit_us: 900,
                lock_us: 700,
                copy_us: 25,
                unmap_us: 35,
                unregister_calls: 0,
                unregister_us: 0,
            },
        );

        assert_eq!(stages.frames, 2);
        assert_eq!(stages.register.count, 1);
        assert_eq!(stages.register.total_us, 120);
        assert_eq!(stages.map.count, 2);
        assert_eq!(stages.map.total_us, 90);
        assert_eq!(stages.map.max_us, 50);
        assert_eq!(stages.submit.total_us, 2_000);
        assert_eq!(stages.submit.max_us, 1_100);
        assert_eq!(stages.lock.total_us, 1_200);
        assert_eq!(stages.lock.max_us, 700);
        assert_eq!(stages.unregister.count, 1);
        assert_eq!(stages.unregister.total_us, 80);
    }

    #[test]
    fn benchmark_profile_only_matches_its_current_stream_mode() {
        assert!(benchmark_mode_matches(
            BenchmarkStreamMode::Work,
            StreamMode::Work
        ));
        assert!(benchmark_mode_matches(
            BenchmarkStreamMode::Game,
            StreamMode::Game
        ));
        assert!(!benchmark_mode_matches(
            BenchmarkStreamMode::Work,
            StreamMode::Game
        ));
        assert!(!benchmark_mode_matches(
            BenchmarkStreamMode::Game,
            StreamMode::Work
        ));
    }

    #[test]
    fn active_benchmark_lease_owns_encoder_bitrate_across_client_quality_controls() {
        let profile: crate::benchmark::BenchmarkProfile =
            serde_json::from_value(serde_json::json!({
                "schemaVersion": 2,
                "implementationVariant": "candidate",
                "experimentId": "wss-bitrate-owner",
                "runId": "123e4567-e89b-42d3-a456-426614174000",
                "expiresAtMs": 10_000,
                "transport": "wss",
                "codec": "hevc",
                "streamMode": "work",
                "adaptive": false,
                "width": 1920,
                "height": 1080,
                "fps": 30,
                "bitrateBps": 2_800_000,
                "gopSeconds": 2,
                "encoder": {
                    "preset": "p3",
                    "tuning": "ultra-low-latency",
                    "multipass": "disabled",
                    "aqMode": "spatial",
                    "aqStrength": 8,
                    "vbvFrames": 1,
                    "lookaheadDepth": 0,
                    "weightedPrediction": false
                }
            }))
            .unwrap();

        assert_eq!(
            benchmark_owned_encoder_bitrate(
                6_000_000,
                Some(&profile),
                BenchmarkTransport::Wss,
                StreamMode::Work,
                VideoCodec::Hevc,
            ),
            2_800_000
        );
        assert_eq!(
            benchmark_owned_encoder_bitrate(
                6_000_000,
                Some(&profile),
                BenchmarkTransport::Webrtc,
                StreamMode::Work,
                VideoCodec::Hevc,
            ),
            6_000_000
        );
    }

    #[test]
    fn failed_switch_restores_retained_active_source_in_the_new_generation() {
        let state = DesktopState {
            available: true,
            desktop: "default",
            input: "enabled",
        };
        assert!(super::retained_capture_after_failed_switch(state));
        // The retained source is active, but the old-generation frame cannot
        // reopen input; presentation of the fresh frame does.
        assert!(!input_ready_after_presentation(true, 8, 9));
        assert!(input_ready_after_presentation(true, 9, 9));
        assert!(!super::retained_capture_after_failed_switch(DesktopState {
            available: false,
            ..state
        }));
        assert!(!super::retained_capture_after_failed_switch(DesktopState {
            desktop: "secure",
            ..state
        }));
    }

    #[test]
    fn recovery_refreshes_input_and_cursor_geometry_together() {
        let now = Instant::now();
        let mut input = super::InputSink::new();
        let mut cursor = super::CursorTracker::new(1920, 1080, now);
        let recovered = super::DesktopRegion::new(-2560, 120, 2560, 1440);
        super::refresh_input_geometry(&mut input, &mut cursor, recovered, now);
        assert_eq!(input.absolute_region(), recovered);
        assert_eq!(cursor.region(), recovered);
    }

    #[test]
    fn foreground_sampling_parks_with_retained_wss_and_excludes_secure_sources() {
        assert!(super::foreground_sample_live(true, true, "default", true));
        assert!(!super::foreground_sample_live(false, true, "default", true));
        assert!(!super::foreground_sample_live(true, false, "default", true));
        assert!(!super::foreground_sample_live(true, true, "secure", true));
        assert!(!super::foreground_sample_live(true, true, "default", false));
    }

    #[test]
    fn viewport_geometry_is_idempotent_across_revisions() {
        let first = ViewportCrop {
            revision: 1,
            x: 320,
            y: 180,
            width: 960,
            height: 540,
        };
        let retry = ViewportCrop {
            revision: 9,
            ..first
        };
        let moved = ViewportCrop {
            revision: 10,
            x: 336,
            ..first
        };
        assert!(same_viewport_geometry(first, retry));
        assert!(!same_viewport_geometry(first, moved));
    }

    #[test]
    fn disabled_viewport_is_exact_full_frame_and_enabled_crop_is_even_and_bounded() {
        assert_eq!(
            viewport_from_q16(2, false, 1, 1, 1, 1, 1920, 1080),
            Some(ViewportCrop {
                revision: 2,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            })
        );
        let crop = viewport_from_q16(3, true, 60_000, 60_000, 40_000, 40_000, 1920, 1080).unwrap();
        assert_eq!(crop.x % 2, 0);
        assert_eq!(crop.y % 2, 0);
        assert_eq!(crop.width % 2, 0);
        assert_eq!(crop.height % 2, 0);
        assert!(crop.x + crop.width <= 1920);
        assert!(crop.y + crop.height <= 1080);
    }

    #[test]
    fn only_matching_admitted_idr_consumes_a_pending_transition() {
        let mode = VideoMode::rung(Profile::Balanced);
        let mut pending = Some(PendingSubmittedIdr {
            id: 7,
            mode,
            reason: "transition",
        });
        assert!(take_matching_transition(&mut pending, 6).is_none()); // stale/ordinary IDR
        assert!(pending.is_some());
        assert_eq!(
            take_matching_transition(&mut pending, 7).unwrap().mode,
            mode
        );
        assert!(pending.is_none()); // failed/unsubmitted work never commits
    }

    #[test]
    fn fallback_plan_preserves_old_exact_dimensions_until_matching_admission() {
        let old = VideoMode {
            profile: Profile::Balanced,
            label: "custom",
            width: 1184,
            height: 666,
            fps: 27,
        };
        assert_eq!(fallback_mode(old, 2560, 1440, true), old);
        assert_eq!(
            fallback_mode(old, 2560, 1440, false),
            VideoMode::native(2560, 1440, 27)
        );

        let mut state = ProfileTransition::new(Profile::Balanced, Profile::Full);
        let pending = PendingSubmittedIdr {
            id: 9,
            mode: old,
            reason: "encoder-fallback",
        };
        assert!(!commit_matching_transition(&mut state, pending, 8));
        assert_eq!(state.active(), Profile::Balanced);
        assert!(!state.is_pinned());
    }

    #[test]
    fn matching_admitted_fallback_commits_and_pins_once() {
        let old = VideoMode::rung(Profile::Balanced);
        let mut state = ProfileTransition::new(Profile::Balanced, Profile::Full);
        let pending = PendingSubmittedIdr {
            id: 9,
            mode: old,
            reason: "encoder-fallback",
        };
        assert!(commit_matching_transition(&mut state, pending, 9));
        assert!(state.is_pinned());
        assert_eq!(state.active, old);
        assert!(!commit_matching_transition(
            &mut state,
            PendingSubmittedIdr {
                id: 9,
                mode: old,
                reason: "encoder-fallback"
            },
            8
        ));
    }

    #[test]
    fn cached_startup_replay_requires_exact_staged_mode() {
        let state = ProfileTransition::new(Profile::Full, Profile::Balanced);
        assert_eq!(
            cached_replay_transition(&state, VideoMode::rung(Profile::Balanced), 3),
            Some(PendingSubmittedIdr {
                id: 3,
                mode: VideoMode::rung(Profile::Balanced),
                reason: "startup"
            })
        );
        assert_eq!(
            cached_replay_transition(&state, VideoMode::rung(Profile::Full), 3),
            None
        );
    }

    #[test]
    fn duplicate_offer_is_not_forwarded_while_in_flight() {
        assert!(should_forward_offer(false, false, false));
        assert!(!should_forward_offer(false, false, true));
        assert!(!should_forward_offer(true, false, false));
    }

    #[test]
    fn monitor_refresh_is_v4_epoch_scoped_and_accepts_pre_wss_refresh() {
        let epoch = "0123456789abcdef0123456789abcdef";
        assert!(valid_monitor_refresh_control(Some(4), Some(epoch), None));
        assert!(!valid_monitor_refresh_control(Some(2), Some(epoch), None));
        assert!(!valid_monitor_refresh_control(Some(3), Some(epoch), None));
        assert!(!valid_monitor_refresh_control(Some(5), Some(epoch), None));
        assert!(valid_monitor_refresh_control(
            Some(4),
            Some(epoch),
            Some(epoch)
        ));
        assert!(!valid_monitor_refresh_control(Some(1), Some(epoch), None));
        assert!(!valid_monitor_refresh_control(Some(4), None, None));
        assert!(!valid_monitor_refresh_control(
            Some(4),
            Some("A123456789abcdef0123456789abcdef"),
            None
        ));
        assert!(valid_monitor_refresh_control(
            Some(4),
            Some(epoch),
            Some("ffffffffffffffffffffffffffffffff")
        ));
    }

    #[test]
    fn wss_presets_are_explicit_stable_modes_and_respect_the_ceiling() {
        let high = super::wss_preset(None, 40_000_000, StreamMode::Work);
        assert_eq!(
            (
                high.mode.label,
                high.mode.width,
                high.mode.height,
                high.mode.fps
            ),
            ("1080p30", 1920, 1080, 30)
        );
        assert_eq!(high.target_bps, 6_000_000);

        let balanced = super::wss_preset(Some("balanced"), 40_000_000, StreamMode::Work);
        assert_eq!(
            (
                balanced.mode.label,
                balanced.mode.width,
                balanced.mode.height,
                balanced.mode.fps
            ),
            ("720p30", 1280, 720, 30)
        );
        assert_eq!(balanced.target_bps, 3_000_000);

        let saver = super::wss_preset(Some("saver"), 40_000_000, StreamMode::Work);
        assert_eq!(
            (
                saver.mode.label,
                saver.mode.width,
                saver.mode.height,
                saver.mode.fps
            ),
            ("540p24", 960, 540, 24)
        );
        assert_eq!(saver.target_bps, 1_500_000);

        assert_eq!(
            super::wss_preset(Some("invalid"), 4_000_000, StreamMode::Work).target_bps,
            4_000_000
        );

        let game_high = super::wss_preset(Some("high"), 40_000_000, StreamMode::Game);
        assert_eq!(
            (
                game_high.mode.label,
                game_high.mode.fps,
                game_high.target_bps
            ),
            ("1080p60", 60, 8_000_000)
        );
        let game_balanced = super::wss_preset(Some("balanced"), 40_000_000, StreamMode::Game);
        assert_eq!(
            (
                game_balanced.mode.label,
                game_balanced.mode.fps,
                game_balanced.target_bps
            ),
            ("720p60", 60, 4_500_000)
        );
        let game_saver = super::wss_preset(Some("saver"), 40_000_000, StreamMode::Game);
        assert_eq!(
            (
                game_saver.mode.label,
                game_saver.mode.fps,
                game_saver.target_bps
            ),
            ("540p30", 30, 1_500_000)
        );
    }

    #[test]
    fn pending_wss_first_frame_rebinds_to_the_queued_idr_generation_once() {
        let mut pending = PendingFirstFrame {
            request_id: "firstFrame_1".into(),
            transport: "wss".into(),
            epoch: Some("a".repeat(32)),
            generation: 1,
            admitted: false,
            rtc_idr_id: None,
            rtc_rtp_timestamp: None,
            armed_at: Instant::now(),
            recovery: FirstFrameRecovery::default(),
        };
        assert!(!pending.admit_wss_access_unit(4, false));
        assert_eq!((pending.generation, pending.admitted), (1, false));
        assert!(pending.admit_wss_access_unit(4, true));
        assert_eq!((pending.generation, pending.admitted), (4, true));
        assert!(!pending.admit_wss_access_unit(5, true));
        assert_eq!(pending.generation, 4);
    }

    #[test]
    fn webrtc_first_frame_is_not_admitted_by_the_wss_boundary() {
        let mut pending = PendingFirstFrame {
            request_id: "firstFrame_2".into(),
            transport: "webrtc".into(),
            epoch: None,
            generation: 2,
            admitted: false,
            rtc_idr_id: None,
            rtc_rtp_timestamp: None,
            armed_at: Instant::now(),
            recovery: FirstFrameRecovery::default(),
        };
        assert!(!pending.admit_wss_access_unit(3, true));
        assert_eq!((pending.generation, pending.admitted), (2, false));
    }

    #[test]
    fn rtc_first_frame_requires_its_submitted_idr_and_current_generation_once() {
        let mut pending = PendingFirstFrame {
            request_id: "firstFrame_rtc".into(),
            transport: "webrtc".into(),
            epoch: None,
            generation: 3,
            admitted: false,
            rtc_idr_id: Some(42),
            rtc_rtp_timestamp: None,
            armed_at: Instant::now(),
            recovery: FirstFrameRecovery::default(),
        };
        assert!(!pending.admit_rtc_access_unit(2, 42));
        assert!(!pending.admit_rtc_access_unit(3, 41));
        pending.reject_rtc_access_unit(41);
        assert_eq!(pending.rtc_idr_id, Some(42));
        pending.reject_rtc_access_unit(42);
        assert_eq!(pending.rtc_idr_id, None);
        pending.rtc_idr_id = Some(43);
        assert!(!pending.admit_rtc_access_unit(3, 42));
        assert!(pending.admit_rtc_access_unit(3, 43));
        assert!(!pending.admit_rtc_access_unit(3, 43));
    }

    #[test]
    fn wss_first_frame_cannot_claim_an_rtc_receipt() {
        let mut pending = PendingFirstFrame {
            request_id: "firstFrame_wss".into(),
            transport: "wss".into(),
            epoch: Some("b".repeat(32)),
            generation: 3,
            admitted: false,
            rtc_idr_id: Some(42),
            rtc_rtp_timestamp: None,
            armed_at: Instant::now(),
            recovery: FirstFrameRecovery::default(),
        };
        assert!(!pending.admit_rtc_access_unit(3, 42));
    }

    #[test]
    fn rejected_matching_transition_invalidates_cache_and_requests_recovery() {
        let mode = VideoMode::rung(Profile::Balanced);
        let pending = Some(PendingSubmittedIdr {
            id: 7,
            mode,
            reason: "transition",
        });
        assert!(rejected_transition_requires_recovery(&pending, 7));
        assert!(!rejected_transition_requires_recovery(&pending, 6));
    }

    #[test]
    fn cached_idr_replay_is_immediate_for_first_frame_and_delayed_once_for_pli() {
        let mut replay = CachedIdrReplay::default();
        let start = Instant::now();
        replay.on_ice_connected();
        assert!(replay.is_due(start, true));
        replay.on_replayed();
        assert!(!replay.is_due(start, true));

        replay.on_keyframe_requested(start);
        assert!(!replay.is_due(start, true));
        assert!(!replay.is_due(start + STATIC_REPLAY_DELAY - Duration::from_millis(1), true));
        assert!(replay.is_due(start + STATIC_REPLAY_DELAY, true));
        replay.on_replayed();
        assert!(!replay.is_due(start + STATIC_REPLAY_DELAY * 2, true));
    }

    #[test]
    fn fresh_admitted_idr_cancels_a_pending_static_replay() {
        let mut replay = CachedIdrReplay::default();
        let start = Instant::now();
        replay.on_ice_connected();
        replay.on_keyframe_requested(start);
        replay.on_fresh_idr_admitted();
        assert!(!replay.is_due(start + STATIC_REPLAY_DELAY, true));
    }

    #[test]
    fn unadmitted_cached_idr_retries_with_backoff_instead_of_spinning() {
        let mut replay = CachedIdrReplay::default();
        let start = Instant::now();
        replay.on_ice_connected();
        assert!(replay.is_due(start, true));
        replay.on_not_admitted(start);
        assert!(!replay.is_due(start, true));
        assert!(replay.is_due(start + STATIC_REPLAY_DELAY, true));
    }
    #[test]
    fn idle_desktop_snapshot_polling_supports_first_prewarm_and_live_retry() {
        assert!(should_snapshot_idle_desktop(false, true));
        assert!(!should_snapshot_idle_desktop(false, false));
        assert!(should_snapshot_idle_desktop(true, false));
        assert!(should_snapshot_idle_desktop(true, true));
    }

    #[test]
    fn signaling_without_a_client_does_not_admit_capture_or_encode() {
        assert!(!capture_admission_ready(
            true, false, false, false, false, false
        ));
    }

    #[test]
    fn unavailable_capture_does_not_admit_wss_encode() {
        assert!(!capture_admission_ready(
            false, true, true, true, false, false
        ));
    }

    #[test]
    fn unavailable_local_capture_probes_without_media_admission_every_500ms() {
        let now = Instant::now();
        assert!(!capture_admission_ready(
            false, true, true, false, false, false
        ));
        assert!(super::local_recovery_probe_due(false, now, now));
        let deadline = now + super::capture::CAPTURE_RECOVERY_INTERVAL;
        assert!(!super::local_recovery_probe_due(false, now, deadline));
        assert!(super::local_recovery_probe_due(false, deadline, deadline));
        assert!(!super::local_recovery_probe_due(true, deadline, deadline));
    }

    #[test]
    fn producer_wait_is_bounded_by_housekeeping_and_due_work() {
        let now = Instant::now();
        assert_eq!(producer_wait_duration(now, []), Duration::from_millis(100));
        assert_eq!(
            producer_wait_duration(now, [now + Duration::from_millis(17)]),
            Duration::from_millis(17)
        );
        assert_eq!(
            producer_wait_duration(now, [now - Duration::from_millis(1)]),
            Duration::ZERO
        );
    }

    #[test]
    fn capture_resampling_selects_nearest_source_frames_without_deadline_drift() {
        let start = Instant::now();
        let interval = Duration::from_millis(33);
        let first_deadline = start + interval;
        assert!(frame_admission_due(
            start + Duration::from_millis(25),
            first_deadline,
            interval,
            false,
        ));
        let second_deadline = advance_frame_deadline(first_deadline, start, interval);
        assert!(frame_admission_due(
            start + Duration::from_millis(50),
            second_deadline,
            interval,
            false,
        ));
        let third_deadline = advance_frame_deadline(second_deadline, start, interval);
        assert!(!frame_admission_due(
            start + Duration::from_millis(75),
            third_deadline,
            interval,
            false,
        ));
        assert!(frame_admission_due(
            start + Duration::from_millis(100),
            third_deadline,
            interval,
            false,
        ));
        assert!(frame_admission_due(
            start,
            start + Duration::from_secs(1),
            interval,
            true,
        ));
    }

    #[test]
    fn negotiation_prewarms_only_while_a_first_idr_is_needed() {
        assert!(capture_admission_ready(
            true, true, false, false, false, true
        ));
        assert!(!capture_admission_ready(
            true, true, false, false, false, false
        ));
    }

    #[test]
    fn active_wss_does_not_require_a_dormant_rtc_reservation() {
        assert!(capture_admission_ready(
            true, true, true, true, false, false
        ));
        assert!(!capture_admission_ready(
            true, true, true, false, false, false
        ));
        assert!(capture_admission_ready(
            true, true, true, false, true, false
        ));
    }

    #[test]
    fn wss_admission_rebuilds_encoder_when_benchmark_contract_becomes_effective() {
        assert!(encoder_benchmark_contract_changed(true, None, Some(7)));
        assert!(!encoder_benchmark_contract_changed(true, Some(7), Some(7)));
        assert!(encoder_benchmark_contract_changed(true, Some(7), None));
        assert!(!encoder_benchmark_contract_changed(false, None, Some(7)));
    }

    #[test]
    fn profile_change_is_staged_until_encoder_commit_and_idr_admission() {
        let mut state = ProfileTransition::new(Profile::Full, Profile::Balanced);
        assert_eq!(state.active(), Profile::Full);
        assert_eq!(state.staged(), Some(Profile::Balanced));
        assert_eq!(state.announced(), None);

        state.commit(Profile::Balanced, 30, "bwe");
        assert_eq!(state.active(), Profile::Balanced);
        assert_eq!(state.staged(), None);
        assert_eq!(state.announced(), None);
        assert_eq!(
            state.pending_announcement(),
            Some((VideoMode::rung(Profile::Balanced), "bwe"))
        );

        state.on_idr_admitted();
        assert_eq!(state.announced(), Some(Profile::Balanced));
        assert_eq!(state.pending_announcement(), None);
    }

    #[test]
    fn pinned_profile_ignores_later_controller_dimension_changes() {
        let mut state = ProfileTransition::new(Profile::Full, Profile::Balanced);
        state.pin_native(2560, 1440, 30, "scaler-fallback");
        assert!(state.is_pinned());
        assert!(!state.stage(Profile::Low, "qos"));
        assert_eq!(state.active(), Profile::Full);
        assert_eq!(state.staged(), None);
        assert_eq!(
            state.pending_announcement(),
            Some((VideoMode::native(2560, 1440, 30), "scaler-fallback"))
        );
    }

    #[test]
    fn native_fallback_remains_pinned_under_later_controller_decisions() {
        let mut state = ProfileTransition::new(Profile::Full, Profile::Balanced);
        state.pin_native(2560, 1440, 30, "scaler-fallback");
        state.on_idr_admitted();
        assert!(!state.stage(Profile::Full, "tick"));
        assert_eq!(state.staged(), None);
        assert_eq!(active_encoder_dimensions(&state), (2560, 1440, 30));
        let cached = CachedIdr {
            data: vec![1, 2, 3],
            mode: state.active,
            requires_default: true,
            generation: 1,
        };
        assert_eq!(cached.mode, VideoMode::native(2560, 1440, 30));
    }

    #[test]
    fn cached_idr_never_crosses_a_media_generation() {
        let cached = CachedIdr {
            data: vec![1, 2, 3],
            mode: VideoMode::native(1920, 1080, 30),
            requires_default: false,
            generation: 7,
        };
        assert!(cached.belongs_to(7));
        assert!(
            !cached.belongs_to(8),
            "a desktop transition bumps the generation"
        );
        assert!(!cached.belongs_to(6));
    }

    #[test]
    fn new_peer_offer_keeps_host_input_off_until_presentation() {
        // A forwarded offer starts a peer and waits for "presented".
        assert!(!input_ready_on_offer(true, true));
        // An offer the host ignores keeps the previous rule.
        assert!(input_ready_on_offer(true, false));
        assert!(!input_ready_on_offer(false, false));
        // Presentation of the current generation then enables input.
        assert!(input_ready_after_presentation(true, 4, 4));
        assert!(!input_ready_after_presentation(true, 3, 4));
    }

    #[test]
    fn cached_default_replay_rechecks_authorization_after_desktop_transition() {
        let mut cached = CachedIdr {
            data: vec![1, 2, 3],
            mode: VideoMode::native(1920, 1080, 30),
            requires_default: true,
            generation: 1,
        };
        let default_desktop = std::cell::Cell::new(true);
        assert!(cached.replay_authorized(|| default_desktop.get()));
        default_desktop.set(false);
        assert!(!cached.replay_authorized(|| default_desktop.get()));
        cached.requires_default = false;
        assert!(cached.replay_authorized(|| {
            panic!("helper and Agent caches retain their source authorization")
        }));
    }

    #[test]
    fn same_profile_fps_change_is_not_restaged_after_idr_commit() {
        let mut state = ProfileTransition::new(Profile::Balanced, Profile::Balanced);
        // A controller profile carries its nominal 30 fps, so model the
        // independently admitted 60 -> 30 transition directly.
        state.active.fps = 60;
        assert!(state.stage(Profile::Balanced, "bitrate"));
        state.commit(Profile::Balanced, 30, "bitrate");
        assert_eq!(state.staged(), None);
        assert!(!state.stage(Profile::Balanced, "bitrate"));
    }

    #[test]
    fn device_replacement_reuses_active_scaled_dimensions() {
        let mut state = ProfileTransition::new(Profile::Full, Profile::Low);
        state.commit(Profile::Low, 30, "bwe-down");
        assert_eq!(active_encoder_dimensions(&state), (960, 540, 30));
        state.commit(Profile::Balanced, 30, "stable-up");
        assert_eq!(active_encoder_dimensions(&state), (1280, 720, 30));
    }

    #[test]
    fn startup_uses_staged_balanced_encoder_mode_when_capture_is_1600x900() {
        let mut state = ProfileTransition::new(Profile::Full, Profile::Balanced);
        assert_eq!(
            encoder_mode_for_capture(&mut state, 1600, 900),
            VideoMode::rung(Profile::Balanced)
        );
        assert_eq!(state.staged(), Some(Profile::Balanced));
        assert!(!state.is_pinned());
    }

    #[test]
    fn persistent_resource_mode_forces_the_stable_full_scaler_target() {
        assert!(full_profile_uses_native_texture(
            Profile::Full,
            1920,
            1080,
            ResourceMode::PerFrame
        ));
        assert!(!full_profile_uses_native_texture(
            Profile::Full,
            1920,
            1080,
            ResourceMode::Persistent
        ));
    }

    #[test]
    fn device_replacement_below_active_rung_stages_exact_native_until_admitted() {
        let mut state = ProfileTransition::new(Profile::Full, Profile::Full);
        let native = VideoMode::native(1600, 900, 60);
        assert_eq!(encoder_mode_for_capture(&mut state, 1600, 900), native);
        assert_eq!(state.active(), Profile::Full);
        assert!(!state.is_pinned());
        assert_eq!(state.staged, Some((native, "native-size-fallback")));
        assert!(commit_matching_transition(
            &mut state,
            PendingSubmittedIdr {
                id: 11,
                mode: native,
                reason: "native-size-fallback"
            },
            11
        ));
        assert!(state.is_pinned());
        assert_eq!(state.active, native);
        assert_eq!(
            state.pending_announcement(),
            Some((native, "native-size-fallback"))
        );
    }

    #[test]
    fn cached_native_startup_replay_preserves_fallback_reason_for_admission() {
        let mut state = ProfileTransition::new(Profile::Full, Profile::Full);
        let native = encoder_mode_for_capture(&mut state, 1600, 900);
        let pending = cached_replay_transition(&state, native, 12).unwrap();
        assert_eq!(pending.reason, "native-size-fallback");
        assert!(commit_matching_transition(&mut state, pending, 12));
        assert!(state.is_pinned());
        assert_eq!(state.active, native);
    }

    #[test]
    fn failed_target_keeps_fallback_profile_for_cache_and_announcement() {
        let mut state = ProfileTransition::new(Profile::Balanced, Profile::Full);
        state.pin(Profile::Balanced, 30, "encoder-fallback");
        assert_eq!(state.active(), Profile::Balanced);
        assert_eq!(state.active_fps(), 30);
        assert_eq!(
            state.pending_announcement(),
            Some((VideoMode::rung(Profile::Balanced), "encoder-fallback"))
        );
    }
}
