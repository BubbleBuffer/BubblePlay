//! BubblePlay host: DXGI capture -> NVENC H.264 -> str0m WebRTC -> browser.
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

use bubbleplay_host::agent_client;
use bubbleplay_host::benchmark;
use bubbleplay_host::capture;
use bubbleplay_host::encode;
use bubbleplay_host::feedback_roi;
use bubbleplay_host::frame_damage;
use bubbleplay_host::hevc_tiles;
use bubbleplay_host::hybrid;
use bubbleplay_host::input;
use bubbleplay_host::privileged_ipc;
use bubbleplay_host::privileged_pipe;
use bubbleplay_host::quality;
use bubbleplay_host::reconstruction_feedback;
use bubbleplay_host::rtc;
use bubbleplay_host::scale;
use bubbleplay_host::signaling;
use bubbleplay_host::stun;
use bubbleplay_host::temporal_diff;
use bubbleplay_host::transport;
use bubbleplay_host::vdd;
use bubbleplay_host::wss_media;

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use tracing::{error, info, warn};
use windows::core::Interface;
use windows::Win32::Foundation::RECT;

use agent_client::AgentClient;
use benchmark::{
    BenchmarkAqMode, BenchmarkHybridCacheMode, BenchmarkHybridMode, BenchmarkMultipass,
    BenchmarkPreset, BenchmarkResourceMode, BenchmarkRoiMode, BenchmarkStreamMode,
    BenchmarkTransport, BenchmarkTuning,
};
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
use input::{CursorTracker, DesktopRegion, InputSink};
use privileged_ipc::{
    Codec as AgentCodec, CropRect as AgentCrop, EncoderAqMode as AgentAqMode,
    EncoderMultipass as AgentMultipass, EncoderPreset as AgentPreset, EncoderTuning as AgentTuning,
    Message as AgentMessage, StreamMode as AgentStreamMode,
};
use privileged_pipe::RetryPolicy;
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
use transport::{AccessUnit, TransportCommand, TransportEvent, TransportPump};
use wss_media::{
    AccessUnitMeta as WssAccessUnitMeta, MediaCodec, ReserveOutcome as WssReserveOutcome,
    SubmitOutcome as WssSubmitOutcome, WssMediaEgress,
};

const STUN_SERVER: &str = "stun.l.google.com:19302";
const DEFAULT_BITRATE_BPS: u32 = 1_500_000;
const MAX_BITRATE_BPS: u32 = 40_000_000;
const IDR_INTERVAL: Duration = Duration::from_secs(2);
const KEYFRAME_REQUEST_COOLDOWN: Duration = Duration::from_secs(2);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(8);
const STATIC_REPLAY_DELAY: Duration = Duration::from_millis(250);
const WSS_GOP_INTERVAL: Duration = Duration::from_secs(1);
const WSS_HIGH_BITRATE_BPS: u32 = 6_000_000;
const WSS_BALANCED_BITRATE_BPS: u32 = 3_000_000;
const WSS_SAVER_BITRATE_BPS: u32 = 1_500_000;
/// Idle heartbeat on the signaling socket (the browser uses the same period).
const SIGNALING_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// v4 static-screen refine: one high-quality P frame this long after the
/// last damage-driven frame.
const WSS_V4_REFINE_DELAY: Duration = Duration::from_millis(300);
/// v4 refine frames are encoded at this multiple of the live bitrate.
const WSS_V4_REFINE_BITRATE_FACTOR: u32 = 4;
/// v4 keyframes are owned by the media link (start / request / 30 s safety),
/// so the producer's own periodic-IDR clock is parked this far ahead.
const WSS_V4_PRODUCER_IDR_PARK: Duration = Duration::from_secs(3_600);

fn wss_gop_interval(mode: StreamMode) -> Duration {
    if mode == StreamMode::Work {
        IDR_INTERVAL
    } else {
        WSS_GOP_INTERVAL
    }
}

/// Periodic IDR interval for the WSS producer; v4 has none.
fn wss_periodic_idr_interval(mode: StreamMode, egress: Option<&WssMediaEgress>) -> Duration {
    if egress.is_some() {
        WSS_V4_PRODUCER_IDR_PARK
    } else {
        wss_gop_interval(mode)
    }
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
    token: Option<String>,
    host_gate_token: Option<String>,
    duration_secs: u32,
    bitrate_bps: u32,
    max_bitrate_bps: u32,
    fixed_bitrate_bps: Option<u32>,
    quality: String,
    preset: String,
    out: String,
    monitor: Option<String>,
    vdd: bool,
    privileged_agent: bool,
    benchmark_profile: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        mode: "serve".to_string(),
        worker: None,
        token: std::env::var("BUBBLEPLAY_TOKEN").ok(),
        host_gate_token: std::env::var("BUBBLEPLAY_HOST_GATE_TOKEN").ok(),
        duration_secs: 10,
        bitrate_bps: DEFAULT_BITRATE_BPS,
        max_bitrate_bps: 12_000_000,
        fixed_bitrate_bps: None,
        quality: "auto".to_string(),
        preset: "p3".to_string(),
        out: "test.h264".to_string(),
        monitor: None,
        vdd: false,
        privileged_agent: false,
        benchmark_profile: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" => args.mode = it.next().context("--mode value")?,
            "--worker" => args.worker = Some(it.next().context("--worker value")?),
            "--token" => args.token = Some(it.next().context("--token value")?),
            "--host-gate-token" => {
                args.host_gate_token = Some(it.next().context("--host-gate-token value")?)
            }
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
            "--privileged-agent" => args.privileged_agent = true,
            "--benchmark-profile" => {
                args.benchmark_profile = Some(PathBuf::from(
                    it.next().context("--benchmark-profile value")?,
                ))
            }
            "-h" | "--help" => {
                println!("bubbleplay-host --mode spike|serve [--worker wss://...] [--token ...] [--host-gate-token ...] [--duration secs] [--bitrate bps] [--max-bitrate bps] [--fixed-bitrate bps] [--quality auto|480p24|540p30|720p30|1080p60] [--preset p1|p3] [--out file] [--monitor primary|<index>|<name-substr>] [--vdd] [--privileged-agent] [--benchmark-profile absolute-path]");
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

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(|| std::io::LineWriter::new(std::io::stderr()))
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = parse_args()?;
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
/// The privileged Agent keeps its own selection; the host never records one
/// for it.
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

    /// Record a live switch. `None` (privileged source) leaves the selection
    /// untouched so privileged behavior is preserved.
    fn remember(&mut self, selector: Option<String>) -> bool {
        match selector.filter(|value| !value.is_empty()) {
            Some(selector) => {
                self.selector = Some(selector);
                self.runtime = true;
                true
            }
            None => false,
        }
    }
}

/// Bounded retries before the host refuses to serve rather than capture an
/// unintended display after a remembered monitor disappears.
const REMEMBERED_MONITOR_MAX_ATTEMPTS: u32 = 3;

fn remembered_monitor_attempt_exhausted(attempts: u32) -> bool {
    attempts >= REMEMBERED_MONITOR_MAX_ATTEMPTS
}

/// Serve mode wrapper: a session ends when the peer leaves or ICE dies, but
/// the host keeps running and accepts the next client. Fatal errors (bad
/// token, capture failure) still exit — only session teardown retries.
fn run_serve_forever(args: &Args) -> Result<()> {
    // The inline VDD keepalive outlives a single session: dropping it between
    // sessions would unplug a remembered virtual display.
    let _vdd = if args.privileged_agent {
        None
    } else {
        start_vdd_keepalive(args)?
    };
    let mut selection = CaptureSelection::from_launch(args.monitor.as_deref());
    let mut backoff = Duration::from_secs(1);
    let mut unavailable_attempts = 0u32;
    loop {
        if !args.privileged_agent && selection.is_runtime() {
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
        match run_serve(args, &mut selection) {
            Ok(()) => {
                info!("session ended — waiting for next client");
                backoff = Duration::from_secs(1);
                std::thread::sleep(backoff);
            }
            Err(e) => {
                if !should_retry_serve_error(args.privileged_agent) {
                    return Err(e).context(
                        "privileged serve session failed; returning to the external service supervisor",
                    );
                }
                // Worker deploys reset the Durable Object, killing our WS
                // and briefly 500ing new connections. That's transient —
                // retry with backoff instead of taking the host down.
                error!(
                    ?e,
                    backoff_secs = backoff.as_secs(),
                    "serve session failed — retrying"
                );
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

fn should_retry_serve_error(privileged_agent: bool) -> bool {
    !privileged_agent
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
    let mut encoder: Option<Nvenc> = None;
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
            let dev = frame.device().as_raw() as *mut core::ffi::c_void;
            encoder = Some(Nvenc::new(
                dev,
                frame.width(),
                frame.height(),
                60,
                args.bitrate_bps,
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
        if frames % 60 == 0 {
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
        let chunks = bubbleplay_host::clipboard::chunk_text(
            &text,
            bubbleplay_host::clipboard::HOST_CHUNK_BYTES,
        );
        let count = chunks.len() as u16;
        for (index, chunk) in chunks.into_iter().enumerate() {
            signaling.send_clipboard_chunk(self.id, index as u16, count, chunk);
        }
    }
}

/// Full pipeline: signaling + WebRTC + media + input.
/// Live producer/session loop.  The transport pump exclusively owns str0m and
/// the media UDP socket; this thread owns capture, D3D scaling and NVENC.
fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn run_serve(args: &Args, selection: &mut CaptureSelection) -> Result<()> {
    let worker = args
        .worker
        .clone()
        .context("--worker wss://... is required")?;
    let token = args.token.clone().context("--token is required")?;
    let reset_lab_encoder_on_diagnostic_keyframe =
        std::env::var("BUBBLEPLAY_LAB_RESET_ENCODER_ON_DIAGNOSTIC_KEYFRAME")
            .is_ok_and(|value| value == "1");
    let mut capture = CaptureSource::open(args, selection.selector())?;
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
    let mut signaling = Signaling::connect(&worker, &token, args.host_gate_token.as_deref())?;
    signaling.send_host_capabilities(
        args.privileged_agent,
        args.privileged_agent,
        args.privileged_agent,
        false,
    );
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
    let pump = TransportPump::spawn(socket, host_addr, mapped_addr, quality.target());
    let mut encoder: Option<Nvenc> = None;
    let mut active_codec = VideoCodec::preferred();
    let mut wss_media: Option<WssMediaEgress> = None;
    let mut active_wss_epoch: Option<String> = None;
    let mut media_generation = 1u32;
    let mut media_sequence = 0u32;
    let media_clock_start = Instant::now();
    let mut scaler: Option<Scaler> = None;
    let preset = if args.preset == "p1" {
        Preset::P1
    } else {
        Preset::P3
    };
    let mut profiles =
        ProfileTransition::new_for_mode(Profile::Full, quality.profile(), stream_mode);
    // Only an unprivileged local host runs as the interactive user; the
    // service path syncs clipboard inside the input proxy instead.
    let mut input_sink = InputSink::new().with_clipboard(!capture.is_privileged());
    input_sink.set_absolute_region(capture.region());
    let mut local_clipboard = LocalClipboardPoll::default();
    let mut cursor_tracker = CursorTracker::for_region(capture.region(), Instant::now());
    let mut next_cursor_poll = Instant::now();
    let mut next_input_log = Instant::now() + Duration::from_secs(1);
    let mut latest_idr: Option<CachedIdr> = None;
    let mut replay = CachedIdrReplay::default();
    let mut ice_connected = false;
    let mut offer_accepted = false;
    let mut offer_in_flight = false;
    let mut pending_candidates = VecDeque::new();
    let mut next_admission = Instant::now();
    let mut next_idr = Instant::now();
    let mut next_ping = Instant::now() + SIGNALING_HEARTBEAT_INTERVAL;
    // v4: a static-screen refine frame is pending after damage-driven frames.
    let mut wss_v4_refine_armed = false;
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
    let mut capture_available = !args.privileged_agent;
    // The privileged path stays fail-closed across desktop/monitor/source
    // transitions until the browser acknowledges presentation of a frame from
    // the newly announced media generation.
    let mut remote_input_ready = !args.privileged_agent;
    let mut last_privileged_desktop_state: Option<PrivilegedDesktopState> = None;
    let mut pending_transition_idr: Option<PendingSubmittedIdr> = None;
    let mut pending_wss_idr: Option<PendingWssIdr> = None;
    let mut pending_lab_quality_keyframe: Option<String> = None;
    let mut pending_first_frame: Option<PendingFirstFrame> = None;
    let mut pending_viewport_idr: Option<PendingViewportIdr> = None;
    let mut viewport_crop: Option<ViewportCrop> = None;
    let mut active_viewport: Option<(u32, u32)> = None;
    let mut viewport_revision = 0u32;
    let mut last_logged_bwe: Option<u32> = None;
    let mut producer_loop_iterations = 0u64;
    let mut last_capture_stats = (0u64, 0u64);
    'session: loop {
        producer_loop_iterations = producer_loop_iterations.saturating_add(1);
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
                if !args.privileged_agent {
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
                }
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
        capture.tick(loop_now)?;
        while let Some(event) = capture.try_privileged_event()? {
            match event {
                PrivilegedEvent::Desktop {
                    available,
                    desktop,
                    input,
                    generation: agent_generation,
                    reason,
                } => {
                    let announced = PrivilegedDesktopState {
                        available,
                        desktop,
                        input,
                        agent_generation,
                    };
                    if !record_privileged_desktop_state(
                        &mut last_privileged_desktop_state,
                        announced,
                    ) {
                        continue;
                    }
                    last_privileged_desktop_state = Some(announced);
                    remote_input_ready = false;
                    capture_available = available;
                    media_generation = media_generation.wrapping_add(1).max(1);
                    if !available {
                        pending_first_frame = None;
                        latest_idr = None;
                        pending_transition_idr = None;
                        pending_wss_idr = None;
                        pending_viewport_idr = None;
                    } else {
                        next_admission = Instant::now();
                        next_idr = Instant::now();
                    }
                    signaling.send_desktop_state_kind(
                        if available { "active" } else { "unavailable" },
                        Some(desktop),
                        Some(input),
                        media_generation,
                        reason,
                    );
                    info!(
                        agent_generation,
                        media_generation, desktop, input, "privileged desktop state"
                    );
                }
                PrivilegedEvent::Cursor(encoded) => {
                    signaling.send_binary(&encoded);
                    if ice_connected {
                        pump.publish_cursor_state(encoded);
                    }
                }
                PrivilegedEvent::InputStatus(stats) => signaling.send_input_status(
                    stats.session_generation,
                    stats.received_packets,
                    stats.accepted_packets,
                    stats.failed_packets,
                    stats.dropped_packets,
                    stats.released_inputs,
                    stats.requested_events,
                    stats.applied_events,
                    stats.last_error_code,
                    stats.last_sequence,
                ),
                PrivilegedEvent::Clipboard {
                    id,
                    index,
                    count,
                    text,
                } => signaling.send_clipboard_chunk(id, index, count, &text),
                PrivilegedEvent::Monitors(monitors) => signaling.send_monitor_list(monitors),
                PrivilegedEvent::MonitorSwitch {
                    accepted,
                    monitor,
                    error_code,
                } => {
                    if accepted {
                        media_generation = media_generation.wrapping_add(1).max(1);
                        let state = capture.desktop_state();
                        signaling.send_desktop_state_kind(
                            if state.available {
                                "active"
                            } else {
                                "unavailable"
                            },
                            Some(state.desktop),
                            Some(state.input),
                            media_generation,
                            "monitor-switched",
                        );
                        if let Ok(monitors) = capture.monitors() {
                            signaling.send_monitor_list(monitors);
                        }
                    } else {
                        signaling.send_monitor_error(
                            &monitor,
                            &format!("privileged Agent error {error_code}"),
                        );
                    }
                }
            }
        }
        if let CaptureSource::Local(source) = &mut capture {
            if let Some(state) = source
                .capture
                .as_mut()
                .and_then(Capture::take_availability_transition)
            {
                match state {
                    CaptureAvailability::Unavailable => {
                        capture_available = false;
                        pending_first_frame = None;
                        input_sink.release_all();
                        latest_idr = None;
                        pending_transition_idr = None;
                        pending_wss_idr = None;
                        pending_viewport_idr = None;
                        encoder = None;
                        scaler = None;
                        signaling.send_desktop_state(
                            "unavailable",
                            media_generation,
                            "capture-unavailable",
                        );
                        warn!(
                            generation = media_generation,
                            "desktop capture unavailable; input suspended"
                        );
                    }
                    CaptureAvailability::Active => {
                        capture_available = true;
                        pending_first_frame = None;
                        media_generation = media_generation.wrapping_add(1).max(1);
                        media_sequence = 0;
                        latest_idr = None;
                        pending_transition_idr = None;
                        pending_wss_idr = None;
                        pending_viewport_idr = None;
                        encoder = None;
                        scaler = None;
                        next_admission = Instant::now();
                        next_idr = Instant::now();
                        signaling.send_desktop_state(
                            "active",
                            media_generation,
                            "capture-recovered",
                        );
                        info!(
                            generation = media_generation,
                            "desktop capture recovered; waiting for fresh IDR"
                        );
                    }
                }
            }
        }
        while let Some(event) = pump.try_event() {
            match event {
                TransportEvent::Answer(answer) => {
                    signaling.send_answer(&answer)?;
                    offer_in_flight = false;
                    offer_accepted = true;
                }
                TransportEvent::OfferRejected => break 'session,
                TransportEvent::Action(RtcAction::IceConnected) => {
                    ice_connected = true;
                    replay.on_ice_connected();
                    next_idr = Instant::now();
                }
                TransportEvent::Action(RtcAction::IceDisconnected | RtcAction::Closed)
                | TransportEvent::Stopped => break 'session,
                TransportEvent::Action(RtcAction::VideoCodecSelected(codec)) => {
                    if codec != active_codec && accept_session_codec(active_codec, codec) {
                        info!(
                            from = active_codec.name(),
                            to = codec.name(),
                            "negotiated codec changed; recreating encoder on next frame"
                        );
                        active_codec = codec;
                        encoder = None;
                        latest_idr = None;
                        pending_first_frame = None;
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
                    if !capture_available || !remote_input_ready {
                        continue;
                    }
                    if capture.send_input(&data)? {
                        continue;
                    }
                    let outcome = input_sink.handle(&data);
                    if outcome.cursor_changed || outcome.sync_requested {
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
                TransportEvent::AccessUnitWritten { id, is_idr: true } => {
                    replay.on_fresh_idr_admitted();
                    if let Some(pending) = pending_first_frame
                        .as_mut()
                        .filter(|pending| pending.transport == "webrtc" && !pending.admitted)
                    {
                        pending.admitted = true;
                        signaling.send_first_frame(
                            "admitted",
                            &pending.request_id,
                            pending.generation,
                            Some("webrtc"),
                            pending.epoch.as_deref(),
                            Some(active_codec.name()),
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
                                .map(Nvenc::bitrate)
                                .unwrap_or(quality.target()),
                            quality.bwe(),
                            reason,
                            stream_mode.label(),
                        );
                        profiles.on_idr_admitted();
                    }
                }
                TransportEvent::AccessUnitNotAdmitted { id, is_idr } => {
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
        let cursor_now = Instant::now();
        if !capture.is_privileged() && cursor_now >= next_cursor_poll {
            if let Some(state) = cursor_tracker.sample(None, cursor_now) {
                pump.publish_cursor_state(state.encode());
            }
            next_cursor_poll = cursor_now + Duration::from_millis(16);
        }
        if !capture.is_privileged() {
            let viewer_live =
                capture_available && remote_input_ready && (ice_connected || wss_media.is_some());
            local_clipboard.poll(viewer_live, cursor_now, &mut input_sink, &mut signaling);
        }
        if cursor_now >= next_input_log {
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
        for event in signaling.poll()? {
            match event {
                SignalEvent::Offer(sdp) => {
                    if args.privileged_agent {
                        remote_input_ready = false;
                        capture.release_input();
                    }
                    signaling.send_host_capabilities(
                        args.privileged_agent,
                        args.privileged_agent,
                        args.privileged_agent,
                        false,
                    );
                    if let Ok(monitors) = capture.monitors() {
                        signaling.send_monitor_list(monitors);
                    }
                    let state = capture.desktop_state();
                    signaling.send_desktop_state_kind(
                        if state.available {
                            "active"
                        } else {
                            "unavailable"
                        },
                        Some(state.desktop),
                        Some(state.input),
                        media_generation,
                        "offer-state-replay",
                    );
                    if should_forward_offer(ice_connected, offer_accepted, offer_in_flight) {
                        if !pump.command(TransportCommand::Offer(sdp)) {
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
                            if (transport == "wss") != wss_media.is_some()
                                || (transport == "webrtc") != (wss_media.is_none() && ice_connected)
                            {
                                continue;
                            }
                            if transport == "wss" && epoch.as_deref() != active_wss_epoch.as_deref()
                            {
                                continue;
                            }
                            let now = Instant::now();
                            pending_first_frame = Some(PendingFirstFrame {
                                request_id,
                                transport,
                                epoch,
                                generation: media_generation,
                                admitted: false,
                                armed_at: now,
                            });
                            replay.on_keyframe_requested(now);
                            if let Some(enc) = encoder.as_mut() {
                                enc.force_idr();
                            }
                            next_idr = now;
                            next_admission = now;
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
                            let state = capture.desktop_state();
                            remote_input_ready = privileged_input_ready_after_presentation(
                                args.privileged_agent,
                                capture_available,
                                state,
                                generation,
                                media_generation,
                            );
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
                SignalEvent::RemoteCandidate(c) if offer_accepted => {
                    let _ = pump.command(TransportCommand::RemoteCandidate(c));
                }
                SignalEvent::RemoteCandidate(c) => pending_candidates.push_back(c),
                SignalEvent::PeerLeft => break 'session,
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
                    if !capture_available || !remote_input_ready {
                        continue;
                    }
                    if capture.send_input(&data)? {
                        continue;
                    }
                    let outcome = input_sink.handle(&data);
                    if outcome.cursor_changed || outcome.sync_requested {
                        if let Some(state) =
                            cursor_tracker.sample(outcome.accepted_sequence, Instant::now())
                        {
                            // WSS mode uses the authenticated signaling socket
                            // for cursor feedback; the Worker relays binary in
                            // the opposite direction without JSON wrapping.
                            signaling.send_binary(&state.encode());
                            if outcome.sync_requested {
                                info!(ack = ?state.acknowledged_sequence, in_bounds = state.in_bounds, os_visible = state.visible, "WSS cursor feedback sent");
                            }
                        }
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
                } => {
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
                        input_sink.release_all();
                        capture.release_input();
                        remote_input_ready = false;
                        signaling.send_desktop_state(
                            "unavailable",
                            media_generation,
                            "monitor-switch",
                        );
                        media_generation = media_generation.wrapping_add(1).max(1);
                        media_sequence = 0;
                        latest_idr = None;
                        pending_transition_idr = None;
                        pending_wss_idr = None;
                        pending_viewport_idr = None;
                        encoder = None;
                        scaler = None;
                        next_admission = Instant::now();
                        next_idr = Instant::now();
                        match capture.switch_monitor(selector) {
                            Ok(completed) => {
                                // Local capture completes synchronously. The
                                // privileged Agent returns false and must prove
                                // the newly selected monitor with
                                // CaptureHealth::Active before input/media are
                                // advertised ready.
                                capture_available = completed;
                                if completed {
                                    // The client owns the display choice for
                                    // the lifetime of the host process: a
                                    // later session reopens the same monitor.
                                    selection.remember(capture.monitor_selector());
                                    input_sink.set_absolute_region(capture.region());
                                    cursor_tracker =
                                        CursorTracker::for_region(capture.region(), Instant::now());
                                    signaling.send_desktop_state(
                                        "active",
                                        media_generation,
                                        "monitor-switched",
                                    );
                                }
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
                                capture_available = true;
                                signaling.send_monitor_error(selector, &error.to_string());
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
                        continue;
                    }
                    if action == "start" {
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
                                pending_first_frame = None;
                                pending_transition_idr = None;
                                pending_wss_idr = None;
                                pending_viewport_idr = None;
                                next_admission = Instant::now();
                                next_idr = Instant::now();
                            }
                        }
                    }
                    if action == "start" && wss_media.is_none() {
                        let Some(bearer) = args.host_gate_token.as_deref() else {
                            warn!("WSS media requested but --host-gate-token is missing");
                            continue;
                        };
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
                            WssMediaEgress::connect(
                                &worker,
                                bearer,
                                epoch,
                                u64::from(preset.target_bps),
                                u64::from(preset.target_bps),
                            )
                        } else {
                            Err(anyhow::anyhow!("unsupported WSS media protocol"))
                        };
                        match connect_result {
                            Ok(egress) => {
                                if args.privileged_agent {
                                    remote_input_ready = false;
                                    capture.release_input();
                                }
                                info!(codec = active_codec.name(), "WSS media egress connected");
                                wss_media = Some(egress);
                                active_wss_epoch = Some(epoch.to_owned());
                                // A browser can reconnect after a previous
                                // secure-desktop transition.  Announce the
                                // current capture state at every media start
                                // so the client cannot retain stale overlay
                                // state across WSS epochs.
                                let state = capture.desktop_state();
                                signaling.send_desktop_state_kind(
                                    if state.available {
                                        "active"
                                    } else {
                                        "unavailable"
                                    },
                                    Some(state.desktop),
                                    Some(state.input),
                                    media_generation,
                                    "wss-session-start",
                                );
                                signaling.send_host_capabilities(
                                    args.privileged_agent,
                                    args.privileged_agent,
                                    args.privileged_agent,
                                    false,
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
                                wss_v4_refine_armed = false;
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
                                    enc.reset_rate_control()
                                        .context("resetting Stream Lab NVENC rate-control state")?;
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
        if offer_accepted {
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
        if pending_first_frame
            .as_ref()
            .is_some_and(|pending| now.duration_since(pending.armed_at) >= FIRST_FRAME_TIMEOUT)
        {
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
                warn!(generation = pending.generation, transport = %pending.transport, reason, "first-frame handshake failed");
            }
        }
        let media_ready = ice_connected || wss_media.is_some();
        let wss_v4_events = wss_media
            .as_ref()
            .map(WssMediaEgress::take_events)
            .unwrap_or_default();
        if let Some(reason) = wss_v4_events.keyframe.as_deref() {
            // The receiver lost (or never had) its reference: encode a fresh
            // IDR even when the desktop is static. v4 never replays a cached
            // IDR, whose sequence would not match the receiver's chain.
            latest_idr = None;
            capture.request_idle_snapshot();
            if let Some(enc) = encoder.as_mut() {
                enc.force_idr();
            }
            next_idr = now;
            wss_v4_refine_armed = false;
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
                info!(bitrate_bps, "WSS v4 delay controller adjusted bitrate");
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
                            .map(Nvenc::bitrate)
                            .unwrap_or(quality.target()),
                        quality.bwe(),
                        reason,
                        stream_mode.label(),
                    );
                    profiles.on_idr_admitted();
                }
            }
        }
        if media_ready && replay.is_due(now, latest_idr.is_some()) {
            if let Some(cached) = latest_idr.as_ref() {
                let expected = profiles
                    .staged
                    .map(|(mode, _)| mode)
                    .unwrap_or(profiles.active);
                if cached.mode != expected {
                    latest_idr = None;
                    continue 'session;
                }
                if ice_connected && wss_media.is_none() {
                    if let Some(reservation) = pump.reserve_access_unit() {
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
                        }
                    }
                }
            }
        }
        // Reserve the RTC queue only when RTC is active. WSS owns a separately
        // bounded media socket, so it is never gated by an unused RTC slot.
        let rtc_reservation = if ice_connected && wss_media.is_none() {
            pump.reserve_access_unit()
        } else {
            None
        };
        let negotiation_active = offer_in_flight || offer_accepted;
        let media_demand_active = negotiation_active || ice_connected || wss_media.is_some();
        let prewarm_needed = negotiation_active && !ice_connected && latest_idr.is_none();
        if capture_admission_ready(
            capture_available,
            capture.is_privileged() && !capture_available,
            negotiation_active,
            ice_connected,
            wss_media.is_some(),
            rtc_reservation.is_some(),
            prewarm_needed,
        ) {
            let active_benchmark_profile = benchmark_lease.active().filter(|profile| {
                active_codec == VideoCodec::Hevc
                    && benchmark_mode_matches(profile.stream_mode, stream_mode)
                    && match profile.transport {
                        BenchmarkTransport::Wss => wss_media.is_some(),
                        BenchmarkTransport::Webrtc => wss_media.is_none() && ice_connected,
                    }
            });
            if let CaptureSource::Privileged(source) = &mut capture {
                let old_mode = profiles.active;
                let mode = profiles.staged.map(|(mode, _)| mode).unwrap_or(old_mode);
                let benchmark_profile = active_benchmark_profile;
                let transition = profiles.staged().is_some();
                if now < next_admission {
                    continue 'session;
                }
                let mut force_idr = now >= next_idr || latest_idr.is_none() || transition;
                let mut wss_reservation = None;
                if let Some(egress) = wss_media.as_ref() {
                    let base_meta = WssAccessUnitMeta {
                        codec: wss_codec(active_codec),
                        generation: media_generation,
                        sequence: media_sequence.wrapping_add(1),
                        timestamp90k: (media_clock_start.elapsed().as_secs_f64() * 90_000.0) as u32,
                        key: force_idr,
                    };
                    match egress.reserve(base_meta) {
                        Ok(reservation) => wss_reservation = Some(reservation),
                        Err(WssReserveOutcome::RequireKeyframe) if !force_idr => {
                            force_idr = true;
                            match egress.reserve(WssAccessUnitMeta {
                                key: true,
                                ..base_meta
                            }) {
                                Ok(reservation) => wss_reservation = Some(reservation),
                                Err(WssReserveOutcome::Stopped) => break 'session,
                                Err(_) => {
                                    wss_preencode_skips = wss_preencode_skips.saturating_add(1);
                                    benchmark_skips = benchmark_skips.saturating_add(1);
                                    wss_preencode_skips_total =
                                        wss_preencode_skips_total.saturating_add(1);
                                    continue 'session;
                                }
                            }
                        }
                        Err(WssReserveOutcome::Stopped) => break 'session,
                        Err(_) => {
                            wss_preencode_skips = wss_preencode_skips.saturating_add(1);
                            benchmark_skips = benchmark_skips.saturating_add(1);
                            wss_preencode_skips_total = wss_preencode_skips_total.saturating_add(1);
                            continue 'session;
                        }
                    }
                }
                let permit_id = next_au_id;
                next_au_id = next_au_id.wrapping_add(1).max(1);
                let crop = viewport_crop.map(|crop| AgentCrop::Pixels {
                    x: crop.x,
                    y: crop.y,
                    width: crop.width,
                    height: crop.height,
                });
                let result = source.encode_agent(AgentMessage::EncodePermit {
                    permit_id,
                    session_generation: media_generation,
                    config_revision: 1,
                    stream_mode: if stream_mode == StreamMode::Work {
                        AgentStreamMode::Work
                    } else {
                        AgentStreamMode::Gaming
                    },
                    width: benchmark_profile
                        .map_or(mode.width, |profile| u32::from(profile.width))
                        .min(u16::MAX as u32) as u16,
                    height: benchmark_profile
                        .map_or(mode.height, |profile| u32::from(profile.height))
                        .min(u16::MAX as u32) as u16,
                    fps: benchmark_profile
                        .map_or(mode.fps, |profile| u32::from(profile.fps))
                        .min(u16::MAX as u32) as u16,
                    bitrate_bps: benchmark_profile
                        .map_or_else(|| quality.target(), |profile| profile.bitrate_bps),
                    codec: if active_codec == VideoCodec::Hevc {
                        AgentCodec::Hevc
                    } else {
                        AgentCodec::H264
                    },
                    preset: benchmark_profile.map_or_else(
                        || {
                            if args.preset == "p1" {
                                AgentPreset::P1
                            } else {
                                AgentPreset::P3
                            }
                        },
                        |profile| match profile.encoder.preset {
                            BenchmarkPreset::P1 => AgentPreset::P1,
                            BenchmarkPreset::P2 => AgentPreset::P2,
                            BenchmarkPreset::P3 => AgentPreset::P3,
                            BenchmarkPreset::P4 => AgentPreset::P4,
                            BenchmarkPreset::P5 => AgentPreset::P5,
                            BenchmarkPreset::P6 => AgentPreset::P6,
                            BenchmarkPreset::P7 => AgentPreset::P7,
                        },
                    ),
                    tuning: benchmark_profile.map_or(AgentTuning::UltraLowLatency, |profile| {
                        match profile.encoder.tuning {
                            BenchmarkTuning::UltraLowLatency => AgentTuning::UltraLowLatency,
                            BenchmarkTuning::LowLatency => AgentTuning::LowLatency,
                        }
                    }),
                    multipass: benchmark_profile.map_or(AgentMultipass::Disabled, |profile| {
                        match profile.encoder.multipass {
                            BenchmarkMultipass::Disabled => AgentMultipass::Disabled,
                            BenchmarkMultipass::Quarter => AgentMultipass::Quarter,
                            BenchmarkMultipass::Full => AgentMultipass::Full,
                        }
                    }),
                    aq_mode: benchmark_profile.map_or(
                        AgentAqMode::Spatial,
                        |profile| match profile.effective_aq_mode() {
                            BenchmarkAqMode::None => AgentAqMode::None,
                            BenchmarkAqMode::Spatial => AgentAqMode::Spatial,
                            BenchmarkAqMode::Temporal => AgentAqMode::Temporal,
                        },
                    ),
                    aq_strength: benchmark_profile.map_or(8, |profile| profile.encoder.aq_strength),
                    vbv_frames: benchmark_profile.map_or(1, |profile| profile.encoder.vbv_frames),
                    lookahead_depth: benchmark_profile
                        .map_or(0, |profile| profile.encoder.lookahead_depth),
                    weighted_prediction: benchmark_profile
                        .is_some_and(|profile| profile.encoder.weighted_prediction),
                    gop_seconds: benchmark_profile.map_or_else(
                        || {
                            if stream_mode == StreamMode::Game {
                                1
                            } else {
                                2
                            }
                        },
                        |profile| profile.gop_seconds,
                    ),
                    crop,
                    force_idr,
                    allow_idle_snapshot: true,
                });
                let access = match result {
                    Ok(AgentMessage::EncodedAccessUnit(access)) => access,
                    Ok(AgentMessage::NoFrame { .. }) => {
                        next_admission = now + privileged_no_frame_retry_delay(capture_available);
                        continue 'session;
                    }
                    Ok(other) => {
                        warn!(kind = ?other.kind(), "unexpected privileged Agent media result");
                        continue 'session;
                    }
                    Err(error) => {
                        let failed_at = Instant::now();
                        let retry = source.note_encode_failure(failed_at, &error);
                        capture_available = false;
                        // Keep signaling and WSS/WebRTC control alive while
                        // the privileged process boundary reconnects. The
                        // queued disconnect transition announces Unavailable
                        // exactly once. The Agent must publish
                        // CaptureHealth::Active before another frame is
                        // admitted. Both the retry cadence and the report are
                        // bounded: an unconditional warn plus an immediate
                        // retry against the reconnecting client spins here and
                        // floods stderr until the service log-budget guard
                        // stops the transport.
                        next_admission = failed_at + retry;
                        continue 'session;
                    }
                };
                source.width = access.width as u32;
                source.height = access.height as u32;
                if benchmark_profile.is_some() {
                    benchmark_encoded_bytes =
                        benchmark_encoded_bytes.saturating_add(access.data.len() as u64);
                    benchmark_encode_count = benchmark_encode_count.saturating_add(1);
                    benchmark_encode_us_total =
                        benchmark_encode_us_total.saturating_add(u64::from(access.encode_us));
                    benchmark_encode_us_max = benchmark_encode_us_max.max(access.encode_us);
                    if access.keyframe {
                        benchmark_idr_count = benchmark_idr_count.saturating_add(1);
                    } else {
                        benchmark_delta_count = benchmark_delta_count.saturating_add(1);
                    }
                }
                next_admission = now + Duration::from_secs_f64(1.0 / (access.fps as f64).max(1.0));
                if force_idr {
                    next_idr = now
                        + if wss_media.is_some() {
                            WSS_V4_PRODUCER_IDR_PARK
                        } else {
                            effective_idr_interval(
                                stream_mode,
                                wss_media.is_some(),
                                benchmark_profile.map(|profile| profile.gop_seconds),
                            )
                        };
                }
                if access.keyframe {
                    latest_idr = Some(CachedIdr {
                        data: access.data.clone(),
                        mode,
                    });
                }
                let id = permit_id;
                if let Some(mut reservation) = wss_reservation {
                    media_sequence = media_sequence.wrapping_add(1);
                    let meta = WssAccessUnitMeta {
                        codec: wss_codec(active_codec),
                        generation: media_generation,
                        sequence: media_sequence,
                        timestamp90k: access.timestamp_90khz as u32,
                        key: access.keyframe,
                    };
                    reservation.set_meta(meta);
                    let encoded_len = access.data.len();
                    if matches!(
                        reservation.submit(access.data.clone()),
                        WssSubmitOutcome::Queued
                    ) {
                        if benchmark_profile.is_some() {
                            benchmark_wss_payload_bytes = benchmark_wss_payload_bytes
                                .saturating_add(
                                    wss_media
                                        .as_ref()
                                        .map_or(0, |egress| egress.wire_payload_bytes(encoded_len)),
                                );
                        }
                        if access.keyframe {
                            wss_idr_count = wss_idr_count.saturating_add(1);
                            if let Some(pending) = pending_first_frame.as_mut() {
                                if pending.admit_wss_access_unit(media_generation, true) {
                                    signaling.send_first_frame(
                                        "admitted",
                                        &pending.request_id,
                                        pending.generation,
                                        Some("wss"),
                                        pending.epoch.as_deref(),
                                        Some(active_codec.name()),
                                    );
                                }
                            }
                            let transition_reason = profiles
                                .staged
                                .filter(|(staged, _)| *staged == mode)
                                .map(|(_, reason)| reason);
                            if let Some(reason) = transition_reason {
                                pending_wss_idr = Some(PendingWssIdr {
                                    generation: media_generation,
                                    sequence: media_sequence,
                                    mode,
                                    reason,
                                });
                            }
                            if let Some(crop) = viewport_crop {
                                pending_viewport_idr = Some(PendingViewportIdr {
                                    generation: media_generation,
                                    sequence: media_sequence,
                                    crop,
                                });
                            }
                        } else {
                            wss_delta_count = wss_delta_count.saturating_add(1);
                        }
                        wss_max_access_unit_bytes = wss_max_access_unit_bytes.max(encoded_len);
                    } else {
                        wss_submission_failures = wss_submission_failures.saturating_add(1);
                        next_idr = now;
                    }
                }
                if ice_connected && wss_media.is_none() {
                    if let Some(reservation) = rtc_reservation {
                        if reservation.submit(AccessUnit {
                            id,
                            data: access.data,
                            produced_at: Instant::now(),
                            is_idr: access.keyframe,
                        }) && access.keyframe
                        {
                            if transition {
                                pending_transition_idr = Some(PendingSubmittedIdr {
                                    id,
                                    mode,
                                    reason: "transition",
                                });
                            }
                        }
                    }
                }
                continue 'session;
            }
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
            let captured = match capture.acquire(Duration::ZERO)? {
                Some(frame) => Some(frame),
                None if should_snapshot_idle_desktop(media_ready, latest_idr.is_none()) => capture
                    .take_idle_snapshot_frame()
                    .map(|frame| (frame, true, false)),
                None => None,
            };
            if let Some((mut frame, is_new, recovered)) = captured {
                if recovered {
                    capture_available = true;
                    pending_first_frame = None;
                    media_generation = media_generation.wrapping_add(1).max(1);
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
                        "desktop capture recovered from verified frame; waiting for fresh IDR"
                    );
                }
                let is_idle_snapshot = frame.is_idle_snapshot();
                let main10 = active_benchmark_profile.is_some_and(|profile| profile.encoder.main10);
                let dev = frame.device().as_raw() as *mut core::ffi::c_void;
                // WSS v4 owns keyframes (start / request / 30 s safety), so its
                // encoder runs without periodic IDRs. NVENC cannot change the
                // GOP in place: recreate the session when the mode differs.
                let want_infinite_gop = wss_media.is_some() && active_benchmark_profile.is_none();
                if encoder
                    .as_ref()
                    .is_some_and(|e| e.is_for_device(dev) && e.infinite_gop() != want_infinite_gop)
                {
                    info!(
                        infinite_gop = want_infinite_gop,
                        "recreating NVENC for the WSS v4 GOP mode"
                    );
                    encoder = None;
                    latest_idr = None;
                    pending_transition_idr = None;
                    next_idr = Instant::now();
                }
                if encoder.as_ref().map_or(true, |e| !e.is_for_device(dev)) {
                    if encoder.is_some() {
                        warn!("capture D3D11 device changed; invalidating scaler and IDR state");
                        scaler = None;
                        latest_idr = None;
                        pending_transition_idr = None;
                        next_idr = Instant::now();
                    }
                    let mode =
                        encoder_mode_for_capture(&mut profiles, frame.width(), frame.height());
                    encoder = Some(create_local_encoder(
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
                    )?);
                    encoder_benchmark_profile_revision = desired_encoder_benchmark_profile_revision;
                }
                let old_mode = profiles.active;
                let mut mode = profiles.staged.map(|(m, _)| m).unwrap_or(old_mode);
                let transition = profiles.staged().is_some();
                let encoder_resource_mode = active_benchmark_profile.map_or(
                    ResourceMode::PerFrame,
                    |profile| match profile.encoder.resource_mode {
                        BenchmarkResourceMode::PerFrame => ResourceMode::PerFrame,
                        BenchmarkResourceMode::Persistent => ResourceMode::Persistent,
                    },
                );
                let frame_interval = Duration::from_secs_f64(1.0 / (mode.fps as f64).max(1.0));
                let urgent = latest_idr.is_none() || transition;
                if (is_new && frame_admission_due(now, next_admission, frame_interval, urgent))
                    || (!is_new && urgent)
                {
                    if let Some(egress) = wss_media.as_ref() {
                        egress.set_frame_size(mode.width, mode.height);
                    }
                    let mut wss_reservation = None;
                    let mut wss_forced_idr = false;
                    if let Some(egress) = wss_media.as_ref() {
                        let mut wants_key = now >= next_idr || latest_idr.is_none() || transition;
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
                                encoder.as_mut().unwrap().force_idr();
                                next_idr =
                                    now + wss_periodic_idr_interval(stream_mode, Some(egress));
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
                                continue 'session;
                            }
                            Err(WssReserveOutcome::RequireKeyframe) => {
                                wss_preencode_skips = wss_preencode_skips.saturating_add(1);
                                benchmark_skips = benchmark_skips.saturating_add(1);
                                wss_preencode_skips_total =
                                    wss_preencode_skips_total.saturating_add(1);
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
                            next_idr = now + wss_periodic_idr_interval(stream_mode, Some(egress));
                            wss_forced_idr = true;
                        }
                        wss_reservation = Some(reservation);
                    }
                    // v4 static-screen refine: the delayed follow-up snapshot
                    // after the last damage frame is encoded once, as a
                    // high-bitrate P frame flagged `refine` on the wire.
                    let wss_v4_refine_frame = wss_v4_refine_armed
                        && is_idle_snapshot
                        && !wss_forced_idr
                        && latest_idr.is_some()
                        && !transition
                        && wss_media.is_some();
                    // A fallback is a separate transaction: first prepare a
                    // texture for its exact dimensions, then recreate NVENC
                    // for that same mode, and only commit after its IDR is
                    // admitted by the transport.
                    let mut fallback_reason = None;
                    let mut scaled = match if viewport_crop.is_some() && mode.label != "native" {
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
                    if fallback_reason.is_some() {
                        // The target texture is never passed to the fallback
                        // encoder, even if both textures happen to share a
                        // device.  That contract prevents dimension drift.
                        scaled = match prepare_video_mode_texture(
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
                            Err(error) => {
                                warn!(
                                    ?error,
                                    "scaled fallback unavailable; recovering exact native mode"
                                );
                                mode =
                                    fallback_mode(old_mode, frame.width(), frame.height(), false);
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
                        && (now >= next_idr || latest_idr.is_none() || transition)
                    {
                        encoder.as_mut().unwrap().force_idr();
                        next_idr = now
                            + if wss_media.is_some() {
                                wss_periodic_idr_interval(stream_mode, wss_media.as_ref())
                            } else {
                                IDR_INTERVAL
                            };
                    }
                    let static_hybrid_enabled = active_benchmark_profile.is_some_and(|profile| {
                        profile.schema_version == 2
                            && profile.implementation_variant
                                == benchmark::BenchmarkImplementationVariant::Candidate
                            && profile.hybrid_mode == BenchmarkHybridMode::PngStaticRoi
                            && !profile.encoder.main10
                            && wss_media.is_some()
                    });
                    let feedback_roi_enabled = active_benchmark_profile.is_some_and(|profile| {
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
                    let hevc_quiescent_tiles_enabled =
                        active_benchmark_profile.is_some_and(|profile| {
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
                    let encoded = if let Some(reason) = fallback_reason {
                        match recreate_and_encode_profile(
                            &mut encoder,
                            dev,
                            mode,
                            frame.width(),
                            frame.height(),
                            quality.target(),
                            preset,
                            active_codec,
                            active_benchmark_profile,
                            scaled.texture,
                            feedback_reconstruction_texture,
                        ) {
                            Ok(encoded) => encoded,
                            Err(error) => {
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
                        let temporal_roi_enabled =
                            active_benchmark_profile.is_some_and(|profile| {
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
                            encode_frame_damage(&frame, viewport_crop, mode)
                        };
                        let refine_restore_bps = if wss_v4_refine_frame {
                            let enc = encoder.as_mut().unwrap();
                            let base = enc.bitrate();
                            enc.set_bitrate(wss_v4_refine_bitrate(base))
                                .ok()
                                .map(|()| base)
                        } else {
                            None
                        };
                        let encode_result =
                            if let Some(reconstruction_texture) = feedback_reconstruction_texture {
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
                                let fallback_texture = match prepare_video_mode_texture(
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
                                    Err(error) => {
                                        warn!(?error, "scaled fallback unavailable; recovering exact native mode");
                                        mode = fallback_mode(
                                            old_mode,
                                            frame.width(),
                                            frame.height(),
                                            false,
                                        );
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
                                    dev,
                                    mode,
                                    frame.width(),
                                    frame.height(),
                                    quality.target(),
                                    preset,
                                    active_codec,
                                    active_benchmark_profile,
                                    fallback_texture.texture,
                                    None,
                                ) {
                                    Ok(encoded) => {
                                        fallback_reason.get_or_insert(reason);
                                        encoded
                                    }
                                    Err(error) => {
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
                        });
                    }
                    capture.cancel_idle_followup_snapshot();
                    let v4_session = wss_media.is_some();
                    match idle_snapshot_completion_order(
                        encoded.is_idr && (is_idle_snapshot || v4_session),
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
                    if wss_media.is_some() {
                        if wss_v4_refine_frame {
                            wss_v4_refine_armed = false;
                        } else if !is_idle_snapshot {
                            // Each damage frame pushes the refine out again,
                            // so it lands once the screen has settled.
                            wss_v4_refine_armed = true;
                            capture.request_idle_followup_snapshot(WSS_V4_REFINE_DELAY);
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
        } else if media_demand_active {
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
        if let Some(egress) = wss_media
            .as_ref()
            .filter(|_| Instant::now() >= next_wss_stats)
        {
            let encoded_total = wss_idr_count.saturating_add(wss_delta_count);
            let idr_percent = if encoded_total == 0 {
                0
            } else {
                wss_idr_count.saturating_mul(100) / encoded_total
            };
            let capture_stats = capture.local_capture_stats().unwrap_or(last_capture_stats);
            let capture_acquired = capture_stats.0.saturating_sub(last_capture_stats.0);
            let capture_new = capture_stats.1.saturating_sub(last_capture_stats.1);
            last_capture_stats = capture_stats;
            info!(
                wss_preencode_skips,
                wss_submission_failures,
                wss_idr_count,
                wss_delta_count,
                idr_percent,
                producer_loop_iterations,
                capture_acquired,
                capture_new,
                wss_max_access_unit_bytes,
                "WSS producer window"
            );
            let v4 = egress.stats();
            info!(
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
                "WSS v4 link totals"
            );
            next_wss_stats = Instant::now() + Duration::from_secs(1);
            producer_loop_iterations = 0;
            wss_preencode_skips = 0;
            wss_submission_failures = 0;
            wss_idr_count = 0;
            wss_delta_count = 0;
            wss_max_access_unit_bytes = 0;
        }
        // This bounds producer polling only; the RTC transport is a separate
        // thread and continues to wake on its own timers and channels.
        std::thread::sleep(producer_poll_interval(media_demand_active));
    }
    input_sink.release_all();
    capture.release_input();
    signaling.send_bye();
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
    fn host_ip_probe_skips_ipv6_resolution_results() {
        let v6: std::net::SocketAddr = "[2001:4860:4864:5:8000::1]:19302".parse().unwrap();
        let v4: std::net::SocketAddr = "74.125.250.129:19302".parse().unwrap();
        assert_eq!(super::ipv4_probe_target([v6, v4]), Some(v4));
        assert_eq!(super::ipv4_probe_target([v6]), None);
        assert_eq!(super::ipv4_probe_target([]), None);
    }

    #[test]
    fn wss_v4_refine_bitrate_is_bounded_and_parks_periodic_idr() {
        assert_eq!(super::wss_v4_refine_bitrate(3_000_000), 12_000_000);
        assert_eq!(
            super::wss_v4_refine_bitrate(20_000_000),
            super::MAX_BITRATE_BPS
        );
        assert_eq!(
            super::wss_v4_refine_bitrate(u32::MAX),
            super::MAX_BITRATE_BPS
        );
        assert_eq!(
            super::wss_periodic_idr_interval(super::StreamMode::Work, None),
            super::wss_gop_interval(super::StreamMode::Work)
        );
        assert!(super::WSS_V4_PRODUCER_IDR_PARK > std::time::Duration::from_secs(60));
        assert_eq!(super::SIGNALING_HEARTBEAT_INTERVAL.as_secs(), 15);
    }

    use super::{
        active_encoder_dimensions, advance_benchmark_profile_revision, advance_frame_deadline,
        agent_reconnect_delay, benchmark_encoder_options, benchmark_mode_matches,
        benchmark_owned_encoder_bitrate, benchmark_status_retry_delay, cached_replay_transition,
        capture_admission_ready, coalesce_quiescent_cache_round_trips, commit_matching_transition,
        effective_idr_interval, encoder_benchmark_contract_changed, encoder_mode_for_capture,
        fallback_mode, frame_admission_due, full_profile_uses_native_texture,
        idle_snapshot_completion_order, lab_quality_keyframe_request,
        privileged_input_ready_after_presentation, privileged_no_frame_retry_delay,
        producer_poll_interval, quiescent_damage_rects, quiescent_immediate_damage_rects,
        quiescent_transition_followup_needed, quiescent_transition_upsert_allowed,
        quiescent_validation_requests, record_benchmark_encode_stages,
        record_benchmark_feedback_plan, record_benchmark_feedback_submission,
        record_benchmark_hybrid_validation, record_benchmark_roi_activity,
        record_benchmark_temporal_activity, record_privileged_desktop_state,
        recover_quiescent_after_unsubmitted_frame, rejected_transition_requires_recovery,
        remembered_monitor_attempt_exhausted, same_viewport_geometry, session_capture_selector,
        should_forward_offer, should_replace_wss_media_session,
        should_reset_lab_encoder_for_keyframe, should_snapshot_idle_desktop,
        take_matching_transition, temporal_readback_policy, valid_lab_quality_request_id,
        valid_monitor_refresh_control, viewport_from_q16, CachedIdr, CachedIdrReplay,
        CaptureSelection, IdleSnapshotCompletionOrder, PendingFirstFrame, PendingSubmittedIdr,
        PrivilegedDesktopState, ProfileTransition, ReconnectLogGate, TemporalReadbackPolicy,
        VideoMode, ViewportCrop, AGENT_RECONNECT_LOG_INTERVAL, REMEMBERED_MONITOR_MAX_ATTEMPTS,
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
    fn privileged_agent_reconnect_backoff_is_bounded_and_resets() {
        assert_eq!(agent_reconnect_delay(0), Duration::from_millis(250));
        assert_eq!(agent_reconnect_delay(1), Duration::from_millis(500));
        assert_eq!(agent_reconnect_delay(2), Duration::from_secs(1));
        assert_eq!(agent_reconnect_delay(3), Duration::from_secs(2));
        assert_eq!(agent_reconnect_delay(30), Duration::from_secs(2));
    }

    #[test]
    fn remembered_capture_selection_survives_a_session_restart() {
        let mut selection = CaptureSelection::from_launch(None);
        assert_eq!(selection.selector(), None);
        assert!(!selection.is_runtime());
        assert!(selection.remember(Some("\\\\.\\DISPLAY5".into())));
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
        assert!(selection.remember(Some("\\\\.\\DISPLAY5".into())));
        assert!(selection.is_runtime());
        assert_eq!(
            session_capture_selector(selection.selector(), Some("primary")),
            Some("\\\\.\\DISPLAY5")
        );
    }

    #[test]
    fn privileged_source_never_records_a_local_monitor_selection() {
        let mut selection = CaptureSelection::from_launch(None);
        // CaptureSource::monitor_selector() returns None for the privileged
        // source, so the remembered selection is left untouched.
        assert!(!selection.remember(None));
        assert_eq!(selection.selector(), None);
        assert!(!selection.is_runtime());
        assert!(!selection.remember(Some(String::new())));
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
    fn privileged_service_mode_errors_exit_for_external_supervision() {
        assert!(!super::should_retry_serve_error(true));
        assert!(super::should_retry_serve_error(false));
    }

    #[test]
    fn privileged_agent_reconnect_logs_are_rate_limited_with_a_suppressed_count() {
        let start = Instant::now();
        let mut gate = ReconnectLogGate::default();
        assert_eq!(gate.admit(start), Some(0));
        assert_eq!(gate.admit(start + Duration::from_secs(1)), None);
        assert_eq!(gate.admit(start + Duration::from_secs(2)), None);
        assert_eq!(gate.admit(start + AGENT_RECONNECT_LOG_INTERVAL), Some(2));
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
    fn active_benchmark_gop_overrides_production_idr_cadence() {
        assert_eq!(
            effective_idr_interval(StreamMode::Work, true, Some(4)),
            Duration::from_secs(4)
        );
        assert_eq!(
            effective_idr_interval(StreamMode::Game, false, Some(1)),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn production_idr_cadence_is_unchanged_without_a_matching_benchmark() {
        assert_eq!(
            effective_idr_interval(StreamMode::Work, true, None),
            Duration::from_secs(2)
        );
        assert_eq!(
            effective_idr_interval(StreamMode::Game, true, None),
            Duration::from_secs(1)
        );
        assert_eq!(
            effective_idr_interval(StreamMode::Game, false, None),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn repeated_privileged_desktop_state_does_not_churn_media_generation() {
        let state = PrivilegedDesktopState {
            available: true,
            desktop: "secure",
            input: "enabled",
            agent_generation: 7,
        };
        let mut last = None;
        assert!(record_privileged_desktop_state(&mut last, state));
        assert!(!record_privileged_desktop_state(&mut last, state));
        assert!(record_privileged_desktop_state(
            &mut last,
            PrivilegedDesktopState {
                input: "suspended",
                ..state
            }
        ));
    }

    #[test]
    fn privileged_input_requires_presented_current_generation() {
        let state = PrivilegedDesktopState {
            available: true,
            desktop: "secure",
            input: "enabled",
            agent_generation: 2,
        };
        assert!(!privileged_input_ready_after_presentation(
            true, true, state, 8, 9
        ));
        assert!(!privileged_input_ready_after_presentation(
            true, false, state, 9, 9
        ));
        assert!(privileged_input_ready_after_presentation(
            true, true, state, 9, 9
        ));
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
            armed_at: Instant::now(),
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
            armed_at: Instant::now(),
        };
        assert!(!pending.admit_wss_access_unit(3, true));
        assert_eq!((pending.generation, pending.admitted), (2, false));
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
    fn idle_desktop_snapshot_polling_only_needs_a_connected_peer() {
        assert!(!should_snapshot_idle_desktop(false, true));
        assert!(should_snapshot_idle_desktop(true, false));
        assert!(should_snapshot_idle_desktop(true, true));
    }

    #[test]
    fn signaling_without_a_client_does_not_admit_capture_or_encode() {
        assert!(!capture_admission_ready(
            true, false, false, false, false, false, false
        ));
    }

    #[test]
    fn unavailable_capture_without_a_privileged_probe_does_not_admit_wss_encode() {
        assert!(!capture_admission_ready(
            false, false, true, true, true, false, false
        ));
    }

    #[test]
    fn unverified_privileged_capture_admits_a_bootstrap_probe_for_active_wss() {
        assert!(capture_admission_ready(
            false, true, true, true, true, false, false
        ));
    }

    #[test]
    fn unverified_privileged_capture_probe_retries_are_bounded() {
        assert_eq!(
            privileged_no_frame_retry_delay(false),
            Duration::from_millis(250)
        );
        assert_eq!(
            privileged_no_frame_retry_delay(true),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn idle_producer_uses_a_coarser_poll_interval() {
        assert_eq!(producer_poll_interval(false), Duration::from_secs(1));
        assert_eq!(producer_poll_interval(true), Duration::from_millis(1));
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
            true, false, true, false, false, false, true
        ));
        assert!(!capture_admission_ready(
            true, false, true, false, false, false, false
        ));
    }

    #[test]
    fn active_wss_does_not_require_a_dormant_rtc_reservation() {
        assert!(capture_admission_ready(
            true, false, true, true, true, false, false
        ));
        assert!(!capture_admission_ready(
            true, false, true, true, false, false, false
        ));
        assert!(capture_admission_ready(
            true, false, true, true, false, true, false
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
        };
        assert_eq!(cached.mode, VideoMode::native(2560, 1440, 30));
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
