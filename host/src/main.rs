//! BubblePlay host: DXGI capture -> NVENC H.264 -> str0m WebRTC -> browser.
//!
//! Modes:
//! - `spike`: M1 verification. Capture primary monitor, encode with NVENC,
//!   write Annex B to `test.h264`, report encode latency. No network.
//! - `serve` (default): full pipeline. Connect the signaling worker, wait for
//!   a browser offer, answer it, trickle srflx, stream video, apply input.

mod capture;
mod encode;
mod input;
mod quality;
mod rtc;
mod scale;
mod signaling;
mod stun;
mod vdd;

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use str0m::net::{Protocol, Receive};
use str0m::{Input, Output};
use tracing::{debug, error, info, warn};
use windows::core::Interface;

use capture::{pick_monitor, texture_ptr, Capture};
use encode::{EncodedFrame, Nvenc, Preset};
use input::InputSink;
use quality::{Controller, Decision, Profile, Qos};
use rtc::{RtcAction, RtcSession};
use scale::{ScaleOutput, Scaler};
use signaling::{SignalEvent, Signaling};

const STUN_SERVER: &str = "stun.l.google.com:19302";
const DEFAULT_BITRATE_BPS: u32 = 1_500_000;
const MAX_BITRATE_BPS: u32 = 40_000_000;
const IDR_INTERVAL: Duration = Duration::from_secs(2);
const STATIC_REPLAY_DELAY: Duration = Duration::from_millis(250);
// Chromium may repeat PLI while a large recovery IDR is still in flight.
// Forcing a new IDR for every RTCP packet floods the link and prevents any
// one keyframe from completing, so coalesce requests at the sender boundary.
const KEYFRAME_REQUEST_COOLDOWN: Duration = Duration::from_secs(2);

/// Bounded policy for replaying the latest IDR when desktop duplication has
/// no new presents. It never schedules a stream of static frames.
#[derive(Default)]
struct CachedIdrReplay {
    initial_frame_pending: bool,
    recovery_requested_at: Option<Instant>,
}

/// Keeps controller intent separate from dimensions actually configured in
/// NVENC and from dimensions already admitted by the RTC sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VideoMode {
    profile: Profile,
    label: &'static str,
    width: u32,
    height: u32,
    fps: u32,
}
impl VideoMode {
    fn rung(profile: Profile) -> Self {
        let (width, height, fps) = profile.dimensions();
        Self {
            profile,
            label: profile.name(),
            width,
            height,
            fps,
        }
    }
    fn native(width: u32, height: u32, fps: u32) -> Self {
        Self {
            profile: Profile::Full,
            label: "native",
            width,
            height,
            fps,
        }
    }
}
struct ProfileTransition {
    active: VideoMode,
    staged: Option<(VideoMode, &'static str)>,
    announced: Option<VideoMode>,
    announcement_reason: &'static str,
    pinned: bool,
}

struct CachedIdr {
    data: Vec<u8>,
    mode: VideoMode,
}

impl ProfileTransition {
    fn new(active: Profile, desired: Profile) -> Self {
        let active = VideoMode::rung(active);
        let desired = VideoMode::rung(desired);
        Self {
            active,
            staged: (desired != active).then_some((desired, "startup")),
            announced: None,
            announcement_reason: "startup",
            pinned: false,
        }
    }

    fn active(&self) -> Profile {
        self.active.profile
    }
    fn staged(&self) -> Option<Profile> {
        self.staged.map(|(mode, _)| mode.profile)
    }
    fn active_fps(&self) -> u32 {
        self.active.fps
    }
    fn staged_reason(&self) -> Option<&'static str> {
        self.staged.map(|(_, reason)| reason)
    }
    fn announced(&self) -> Option<Profile> {
        self.announced.map(|mode| mode.profile)
    }
    fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Returns true when an unapplied dimension change is now staged. The
    /// caller uses this to invalidate any IDR cached at the old dimensions.
    fn stage(&mut self, desired: Profile, reason: &'static str) -> bool {
        let desired_mode = VideoMode::rung(desired);
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

    fn commit(&mut self, profile: Profile, fps: u32, reason: &'static str) {
        self.commit_mode(
            VideoMode {
                fps,
                ..VideoMode::rung(profile)
            },
            reason,
        );
    }
    fn commit_mode(&mut self, mode: VideoMode, reason: &'static str) {
        self.active = mode;
        self.staged = None;
        self.announcement_reason = reason;
    }

    fn pin(&mut self, profile: Profile, fps: u32, reason: &'static str) {
        self.pinned = true;
        self.commit(profile, fps, reason);
    }

    fn pin_native(&mut self, width: u32, height: u32, fps: u32, reason: &'static str) {
        self.pinned = true;
        self.commit_mode(VideoMode::native(width, height, fps), reason);
    }

    fn pending_announcement(&self) -> Option<(VideoMode, &'static str)> {
        (self.announced != Some(self.active)).then_some((self.active, self.announcement_reason))
    }

    fn on_idr_admitted(&mut self) {
        self.announced = Some(self.active);
    }
}

fn active_encoder_dimensions(profiles: &ProfileTransition) -> (u32, u32, u32) {
    (
        profiles.active.width,
        profiles.active.height,
        profiles.active.fps,
    )
}

fn encoder_mode_for_capture(
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
    profiles.pin_native(
        native.width,
        native.height,
        native.fps,
        "native-size-fallback",
    );
    native
}

fn create_encoder_with_fallback(
    device: *mut core::ffi::c_void,
    width: u32,
    height: u32,
    native_width: u32,
    native_height: u32,
    fps: u32,
    bitrate_bps: u32,
    requested: Preset,
) -> Result<Nvenc> {
    Nvenc::new_with_preset_native(
        device,
        width,
        height,
        native_width,
        native_height,
        fps,
        bitrate_bps,
        requested,
    )
    .or_else(|first| {
        if matches!(requested, Preset::P3) {
            warn!(
                ?first,
                width, height, fps, "P3 initialization failed; retrying P1"
            );
            Nvenc::new_with_preset_native(
                device,
                width,
                height,
                native_width,
                native_height,
                fps,
                bitrate_bps,
                Preset::P1,
            )
        } else {
            Err(first)
        }
    })
}

fn configure_encoder_profile(
    encoder: &mut Option<Nvenc>,
    device: *mut core::ffi::c_void,
    mode: VideoMode,
    native_width: u32,
    native_height: u32,
    bitrate_bps: u32,
    requested: Preset,
) -> Result<()> {
    let reconfigure = encoder
        .as_mut()
        .context("encoder unavailable")?
        .reconfigure_profile(mode.width, mode.height, mode.fps);
    if let Err(e) = reconfigure {
        warn!(
            ?e,
            profile = mode.label,
            "NVENC profile reconfigure failed; recreating once"
        );
        *encoder = None;
        *encoder = Some(
            create_encoder_with_fallback(
                device,
                mode.width,
                mode.height,
                native_width,
                native_height,
                mode.fps,
                bitrate_bps,
                requested,
            )
            .with_context(|| format!("recreate NVENC for {}", mode.label))?,
        );
    }
    encoder.as_mut().expect("configured above").force_idr();
    Ok(())
}

fn recreate_and_encode_profile(
    encoder: &mut Option<Nvenc>,
    device: *mut core::ffi::c_void,
    mode: VideoMode,
    native_width: u32,
    native_height: u32,
    bitrate_bps: u32,
    requested: Preset,
    texture: *mut core::ffi::c_void,
) -> Result<EncodedFrame> {
    *encoder = None;
    let mut replacement = create_encoder_with_fallback(
        device,
        mode.width,
        mode.height,
        native_width,
        native_height,
        mode.fps,
        bitrate_bps,
        requested,
    )
    .with_context(|| format!("restore NVENC at {}", mode.label))?;
    replacement.force_idr();
    let encoded = replacement
        .encode_texture(texture)
        .context("encode fallback IDR")?;
    if !encoded.is_idr {
        bail!("fallback encoder did not produce the forced IDR");
    }
    *encoder = Some(replacement);
    Ok(encoded)
}

fn prepare_profile_texture(
    scaler: &mut Option<Scaler>,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    input_width: u32,
    input_height: u32,
    profile: Profile,
) -> Result<ScaleOutput> {
    if profile == Profile::Full {
        let texture_device = unsafe { texture.GetDevice() }.context("Full texture GetDevice")?;
        if texture_device.as_raw() != device.as_raw() {
            bail!("Full texture is not on the capture D3D11 device");
        }
        let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        ensure!(
            desc.Width == input_width && desc.Height == input_height,
            "Full texture dimensions {}x{} do not match native capture {}x{}",
            desc.Width,
            desc.Height,
            input_width,
            input_height
        );
        if desc.Width == 1920 && desc.Height == 1080 {
            return Ok(ScaleOutput {
                texture: texture.as_raw() as *mut core::ffi::c_void,
                scale_us: 0,
                width: desc.Width,
                height: desc.Height,
            });
        }
    }
    if scaler
        .as_ref()
        .map_or(true, |current| !current.is_for_device(device))
    {
        *scaler = Some(
            Scaler::new(device, input_width, input_height)
                .context("initialize D3D11 video scaler")?,
        );
    }
    scaler
        .as_mut()
        .expect("initialized above")
        .scale(texture, profile)
        .context("D3D11 video scale")
}

fn prepare_video_mode_texture(
    scaler: &mut Option<Scaler>,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    input_width: u32,
    input_height: u32,
    mode: VideoMode,
) -> Result<ScaleOutput> {
    if mode.label == "native" {
        let texture_device = unsafe { texture.GetDevice() }.context("native texture GetDevice")?;
        ensure!(
            texture_device.as_raw() == device.as_raw(),
            "native texture is not on the capture D3D11 device"
        );
        let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        ensure!(
            desc.Width == input_width
                && desc.Height == input_height
                && desc.Width == mode.width
                && desc.Height == mode.height,
            "native texture {}x{} does not match capture {}x{} or encoder {}x{}",
            desc.Width,
            desc.Height,
            input_width,
            input_height,
            mode.width,
            mode.height
        );
        return Ok(ScaleOutput {
            texture: texture.as_raw() as *mut core::ffi::c_void,
            scale_us: 0,
            width: desc.Width,
            height: desc.Height,
        });
    }
    let output = prepare_profile_texture(
        scaler,
        device,
        texture,
        input_width,
        input_height,
        mode.profile,
    )?;
    ensure!(
        output.width == mode.width && output.height == mode.height,
        "scaled {} output {}x{} does not match encoder {}x{}",
        mode.label,
        output.width,
        output.height,
        mode.width,
        mode.height
    );
    Ok(output)
}

fn apply_quality_decision(
    quality: &mut Controller,
    decision: Decision,
    profiles: &mut ProfileTransition,
    latest_idr: &mut Option<CachedIdr>,
    encoder: &mut Option<Nvenc>,
    signaling: &mut Signaling,
) {
    let now = Instant::now();
    let mut applied = true;
    if let Some(enc) = encoder.as_mut() {
        if let Err(e) = enc.set_bitrate(decision.target_bps) {
            warn!(
                ?e,
                bitrate_bps = decision.target_bps,
                "NVENC bitrate update failed"
            );
            applied = false;
        }
    }
    if !applied {
        return;
    }
    quality.apply(now, decision);
    let reason = decision.reason.label();
    let applied_bitrate = encoder
        .as_ref()
        .map(Nvenc::bitrate)
        .unwrap_or(decision.target_bps);
    let staged = profiles.stage(decision.profile, reason);
    if staged {
        // Never replay an IDR whose SPS/PPS describes dimensions the
        // controller has already asked us to leave.
        *latest_idr = None;
        return;
    }
    // A pinned fallback still follows bitrate decisions, but reports the real
    // encoder dimensions rather than the controller's unavailable profile.
    if profiles.announced == Some(profiles.active) {
        let active = profiles.active;
        signaling.send_quality(
            active.label,
            active.width,
            active.height,
            active.fps,
            applied_bitrate,
            quality.bwe(),
            reason,
        );
    }
}

fn announce_admitted_profile(
    profiles: &mut ProfileTransition,
    signaling: &mut Signaling,
    bitrate_bps: u32,
    bwe_bps: Option<u32>,
) {
    let Some((mode, reason)) = profiles.pending_announcement() else {
        return;
    };
    signaling.send_quality(
        mode.label,
        mode.width,
        mode.height,
        mode.fps,
        bitrate_bps,
        bwe_bps,
        reason,
    );
    profiles.on_idr_admitted();
}

impl CachedIdrReplay {
    fn on_ice_connected(&mut self) {
        self.initial_frame_pending = true;
    }

    fn on_keyframe_requested(&mut self, now: Instant) {
        self.recovery_requested_at.get_or_insert(now);
    }

    fn on_fresh_idr_admitted(&mut self) {
        self.initial_frame_pending = false;
        self.recovery_requested_at = None;
    }

    fn is_due(&self, now: Instant, cached_idr_available: bool) -> bool {
        cached_idr_available
            && (self.initial_frame_pending
                || self
                    .recovery_requested_at
                    .is_some_and(|requested| now.duration_since(requested) >= STATIC_REPLAY_DELAY))
    }

    fn on_replayed(&mut self) {
        self.initial_frame_pending = false;
        self.recovery_requested_at = None;
    }

    fn on_not_admitted(&mut self, now: Instant) {
        self.initial_frame_pending = false;
        self.recovery_requested_at = Some(now);
    }
}

#[derive(Debug)]
struct Args {
    mode: String, // "spike" | "serve"
    worker: Option<String>,
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
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        mode: "serve".to_string(),
        worker: None,
        token: None,
        duration_secs: 10,
        bitrate_bps: DEFAULT_BITRATE_BPS,
        max_bitrate_bps: 12_000_000,
        fixed_bitrate_bps: None,
        quality: "auto".to_string(),
        preset: "p3".to_string(),
        out: "test.h264".to_string(),
        monitor: None,
        vdd: false,
    };
    let mut it = std::env::args().skip(1);
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
            "-h" | "--help" => {
                println!("bubbleplay-host --mode spike|serve [--worker wss://...] [--token ...] [--duration secs] [--bitrate bps] [--max-bitrate bps] [--fixed-bitrate bps] [--quality auto|540p30|720p30|1080p60] [--preset p1|p3] [--out file] [--monitor primary|<index>|<name-substr>] [--vdd]");
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
        "auto" | "540p30" | "720p30" | "1080p60"
    ) {
        bail!("--quality must be auto|540p30|720p30|1080p60");
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
    Ok(args)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = parse_args()?;
    match args.mode.as_str() {
        "spike" => run_spike(&args),
        "serve" => run_serve_forever(&args),
        other => bail!("unknown mode {other} (expected spike|serve)"),
    }
}

/// Serve mode wrapper: a session ends when the peer leaves or ICE dies, but
/// the host keeps running and accepts the next client. Fatal errors (bad
/// token, capture failure) still exit — only session teardown retries.
fn run_serve_forever(args: &Args) -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    loop {
        match run_serve(args) {
            Ok(()) => {
                info!("session ended — waiting for next client");
                backoff = Duration::from_secs(1);
                std::thread::sleep(backoff);
            }
            Err(e) => {
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
        let Some((frame, is_new)) = capture.acquire(Duration::from_millis(100))? else {
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

/// Full pipeline: signaling + WebRTC + media + input.
fn run_serve(args: &Args) -> Result<()> {
    let worker = args
        .worker
        .clone()
        .context("--worker wss://... is required")?;
    let token = args.token.clone().context("--token is required")?;

    // 1. Media UDP socket. Bind wildcard; discover the routed local IP for the
    //    host candidate via a throwaway connected socket.
    let socket = UdpSocket::bind("0.0.0.0:0").context("bind media socket")?;
    // The RTC pump must never wait in recv_from: str0m's leaky-bucket pacer
    // commonly asks to be called again in less than a millisecond. A blocking
    // receive here used to cap us at roughly one RTP packet per loop and built
    // a multi-second sender queue even on a lossless LAN.
    socket
        .set_nonblocking(true)
        .context("make media socket nonblocking")?;
    let local_port = socket.local_addr()?.port();
    let host_ip = pick_host_ip().context("discover local host IP")?;
    let host_addr: SocketAddr = SocketAddr::new(host_ip, local_port);
    info!(%host_addr, "media socket bound");

    // Cache STUN while startup is otherwise idle: a fresh RTC epoch must not
    // put an unreachable STUN round-trip on the offer-to-answer path.
    socket
        .set_nonblocking(false)
        .context("make media socket blocking for startup STUN")?;
    let stun_result = stun::discover_mapped_address(&socket, STUN_SERVER);
    socket
        .set_nonblocking(true)
        .context("restore nonblocking media socket after startup STUN")?;
    let mapped_addr = match stun_result {
        Ok(mapped) => {
            info!(%mapped, "cached srflx candidate ready for RTC epochs");
            Some(mapped)
        }
        Err(e) => {
            warn!(
                ?e,
                "startup STUN failed; RTC epochs will use the host candidate only"
            );
            None
        }
    };

    // 2. Signaling.
    let mut signaling = Signaling::connect(&worker, &token)?;

    // 3. Placeholder RTC epoch. str0m instances have negotiation timers, so
    // the real epoch is always constructed immediately before an unconnected
    // browser offer is accepted (rather than while the host waits idle).
    let pinned_profile = match args.quality.as_str() {
        "540p30" => Some(Profile::Low),
        "720p30" => Some(Profile::Balanced),
        "1080p60" => Some(Profile::Full),
        _ => None,
    };
    let mut quality = pinned_profile
        .map(|profile| {
            Controller::fixed(
                profile,
                args.max_bitrate_bps,
                args.fixed_bitrate_bps.unwrap_or(args.bitrate_bps),
            )
        })
        .unwrap_or_else(|| Controller::new(args.max_bitrate_bps, args.bitrate_bps));
    // RTC's initial BWE and NVENC's initial CBR must start at the policy's
    // effective (bounded) bitrate, including fixed-profile mode.
    let initial_bitrate = quality.target();
    let mut rtc = RtcSession::new(Instant::now(), initial_bitrate);

    // 4. Capture (+ warm up encoder from the first frame).
    let _vdd = start_vdd_keepalive(args)?;
    let mut capture = Capture::new(args.monitor.as_deref()).context("capture init")?;
    let mut encoder: Option<Nvenc> = None;
    let mut scaler: Option<Scaler> = None;
    let requested_preset = if args.preset == "p1" {
        Preset::P1
    } else {
        Preset::P3
    };
    // NVENC is first opened at the native maximum. Auto starts by staging its
    // Balanced controller intent for the first captured frame.
    let mut profiles = ProfileTransition::new(Profile::Full, quality.profile());
    let mut input_sink = InputSink::new();
    let mut latest_idr: Option<CachedIdr> = None;
    let mut cached_idr_replay = CachedIdrReplay::default();

    let mut next_idr = Instant::now() + IDR_INTERVAL;
    let mut last_idr_sent = Instant::now()
        .checked_sub(KEYFRAME_REQUEST_COOLDOWN)
        .unwrap_or_else(Instant::now);
    let mut next_signal_ping = Instant::now() + Duration::from_secs(20);
    let mut last_logged_bwe: Option<u64> = None;
    let mut pending_signal_events = VecDeque::new();
    let mut pending_remote_candidates: VecDeque<String> = VecDeque::new();
    let mut offer_accepted = false;
    let mut connected_since: Option<Instant> = None;
    let mut frames_sent = 0u64;
    let mut input_packets = 0u64;
    let mut input_bytes = 0u64;
    let mut transport_window_started = Instant::now();
    let mut next_transport_log = transport_window_started + Duration::from_secs(1);
    let mut next_quality_tick = Instant::now() + Duration::from_secs(1);
    let mut next_frame_admission = Instant::now();
    let mut tx_packets_window = 0u64;
    let mut tx_bytes_window = 0u64;
    let mut admitted_frames_window = 0u64;
    let mut admitted_bytes_window = 0u64;
    let mut overdue_deadlines_window = 0u64;
    let mut max_deadline_overdue_us = 0u64;
    let mut scale_sum_us = 0u64;
    let mut scale_max_us = 0u64;
    let mut scale_count = 0u64;
    let mut encode_sum_us = 0u64;
    let mut encode_max_us = 0u64;
    let mut encode_count = 0u64;
    let mut buf = vec![0u8; 2000];

    'run: loop {
        if args.fixed_bitrate_bps.is_none() && Instant::now() >= next_quality_tick {
            let now = Instant::now();
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
            next_quality_tick = now + Duration::from_secs(1);
        }
        // -- str0m outputs ----------------------------------------------------
        let timeout;
        loop {
            match rtc.rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    match socket.send_to(&t.contents, t.destination) {
                        Ok(n) => {
                            tx_packets_window += 1;
                            tx_bytes_window += n as u64;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            // UDP backpressure is unusual, but blocking the
                            // pacer would be worse. Congestion control will
                            // react to the resulting loss.
                            debug!(dest = %t.destination, "UDP send would block; dropping datagram");
                        }
                        Err(e) => warn!(?e, dest = %t.destination, "UDP send failed"),
                    }
                }
                Ok(Output::Timeout(t)) => {
                    timeout = t;
                    break;
                }
                Ok(Output::Event(e)) => {
                    if let Some(action) = rtc.on_event(e) {
                        match action {
                            RtcAction::IceConnected => {
                                let now = Instant::now();
                                cached_idr_replay.on_ice_connected();
                                connected_since = Some(now);
                                next_idr = now; // immediate IDR
                                transport_window_started = now;
                                next_transport_log = now + Duration::from_secs(1);
                                tx_packets_window = 0;
                                tx_bytes_window = 0;
                                admitted_frames_window = 0;
                                admitted_bytes_window = 0;
                                overdue_deadlines_window = 0;
                                max_deadline_overdue_us = 0;
                                scale_sum_us = 0;
                                scale_max_us = 0;
                                scale_count = 0;
                                encode_sum_us = 0;
                                encode_max_us = 0;
                                encode_count = 0;
                            }
                            RtcAction::IceDisconnected | RtcAction::Closed => {
                                info!("ICE gone — session closed");
                                break 'run;
                            }
                            RtcAction::KeyframeRequested => {
                                let now = Instant::now();
                                if now.duration_since(last_idr_sent) >= KEYFRAME_REQUEST_COOLDOWN {
                                    cached_idr_replay.on_keyframe_requested(now);
                                    if let Some(enc) = encoder.as_mut() {
                                        enc.force_idr();
                                        info!("forcing IDR after receiver keyframe request");
                                    }
                                } else {
                                    debug!("coalescing repeated receiver keyframe request");
                                }
                            }
                            RtcAction::Bwe(bps) => {
                                if args.fixed_bitrate_bps.is_none() {
                                    if let Some(decision) = quality.bwe_sample(
                                        Instant::now(),
                                        bps.min(args.max_bitrate_bps as u64) as u32,
                                    ) {
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
                                {
                                    let clamped = bps.min(args.max_bitrate_bps as u64) as u32;
                                    // Log the first estimate and any step of
                                    // >= 1 Mbps so the host log shows
                                    // congestion control working in a test.
                                    let log_it = match last_logged_bwe {
                                        None => true,
                                        Some(prev) => {
                                            (clamped as i64 - prev as i64).abs() >= 1_000_000
                                        }
                                    };
                                    if log_it {
                                        info!(bwe_bps = clamped, "BWE applied");
                                        last_logged_bwe = Some(clamped as u64);
                                    }
                                }
                            }
                            RtcAction::InputData(d) => {
                                input_packets += 1;
                                input_bytes += d.len() as u64;
                                input_sink.handle(&d);
                                if input_packets <= 20 || input_packets % 500 == 0 {
                                    info!(
                                        input_packets,
                                        input_bytes, "input packets applied via SendInput"
                                    );
                                }
                            }
                            RtcAction::ChannelOpen(_) => {}
                        }
                    }
                }
                Err(e) => {
                    error!(?e, "poll_output failed");
                    break 'run;
                }
            }
        }

        // -- Signaling events ----------------------------------------------
        // str0m requires one mutation followed immediately by a complete
        // poll_output drain. Retain batches from the WebSocket, but apply at
        // most one RTC-mutating signal per outer iteration.
        if Instant::now() >= next_signal_ping {
            signaling.send_ping();
            next_signal_ping = Instant::now() + Duration::from_secs(20);
        }
        if pending_signal_events.is_empty() {
            match signaling.poll() {
                Ok(events) => pending_signal_events.extend(events),
                Err(e) => {
                    // Transient signaling drops (edge recycling, DO eviction,
                    // network blips) must end the session so the outer loop can
                    // reconnect — they are NOT fatal.
                    warn!(?e, "signaling read failed — ending session to reconnect");
                    break 'run;
                }
            }
        }
        if let Some(candidate) =
            take_remote_candidate_after_answer(offer_accepted, &mut pending_remote_candidates)
        {
            rtc.add_remote_candidate(&candidate);
            // One mutation per iteration; drain str0m before the next signal.
            continue 'run;
        }
        if let Some(ev) = pending_signal_events.pop_front() {
            match ev {
                SignalEvent::Offer(sdp) => {
                    info!("received SDP offer");
                    if !should_start_fresh_rtc_epoch(rtc.ice_connected) {
                        debug!("ignoring duplicate offer for connected RTC epoch");
                        continue 'run;
                    }
                    rtc = fresh_rtc_epoch(&socket, host_addr, mapped_addr, initial_bitrate)?;
                    match rtc.accept_offer(&sdp) {
                        Ok(answer) => {
                            offer_accepted = true;
                            if let Err(e) = signaling.send_answer(&answer) {
                                warn!(?e, "send answer failed — ending session");
                                break 'run;
                            }
                            info!("SDP answer sent");
                            continue 'run;
                        }
                        Err(e) => {
                            // A bad or stale offer (client vanished before DTLS,
                            // ICE never established) must not kill the host —
                            // end this session and wait for the next client.
                            warn!(?e, "accept_offer failed — ending session");
                            break 'run;
                        }
                    }
                }
                SignalEvent::RemoteCandidate(sdp) => {
                    if offer_accepted {
                        rtc.add_remote_candidate(&sdp);
                    } else {
                        pending_remote_candidates.push_back(sdp);
                    }
                    continue 'run;
                }
                SignalEvent::ClientDiagnostics {
                    seq,
                    at_ms,
                    profile,
                } => {
                    info!(
                        client_diag_seq = seq,
                        client_at_ms = at_ms,
                        client_profile = %profile,
                        "client diagnostics"
                    );
                }
                SignalEvent::Qos {
                    seq,
                    at_ms,
                    rx,
                    presentation,
                } => {
                    let backlog = match (rx.frames_received_delta, rx.frames_decoded_delta) {
                        (Some(r), Some(d)) => Some(r as i64 - d as i64),
                        _ => None,
                    };
                    if args.fixed_bitrate_bps.is_none() {
                        let admitted_fps = if transport_window_started.elapsed().is_zero() {
                            presentation.fps
                        } else {
                            Some(
                                admitted_frames_window as f64
                                    / transport_window_started.elapsed().as_secs_f64().max(0.001),
                            )
                        };
                        if let Some(d) = quality.qos_sample(
                            Instant::now(),
                            Qos {
                                loss_pct: rx.loss_pct,
                                packets: rx.packets_received_delta,
                                decode_backlog_growth: backlog,
                                freeze_delta: rx.freeze_delta,
                                admitted_fps,
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
                    debug!(seq, at_ms, "qos accepted");
                }
                SignalEvent::PeerLeft => {
                    info!("peer left — exiting");
                    break 'run;
                }
                SignalEvent::Registered => {}
            }
        }

        // -- UDP ingress ------------------------------------------------------
        // Consume at most one datagram per iteration. An accepted packet is
        // one str0m mutation, so immediately return to the output drain. This
        // also prevents a busy or hostile source from starving media/timeouts.
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                let Ok(contents) = buf[..n].try_into() else {
                    continue 'run;
                };
                let input = Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: host_addr,
                        contents,
                    },
                );
                if rtc.rtc.accepts(&input) {
                    if let Err(e) = rtc.rtc.handle_input(input) {
                        warn!(?e, "handle_input failed");
                    }
                    continue 'run;
                } else {
                    debug!(%source, "ignoring UDP packet (no accepting session)");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => warn!(?e, "UDP recv failed"),
        }

        // Only advance str0m's clock when its advertised deadline is due.
        // The old loop called Timeout unconditionally after a blocking read,
        // then blocked in capture for another 4 ms. That starved the pacer.
        let now = Instant::now();
        if now >= timeout {
            let overdue = now.duration_since(timeout);
            if !overdue.is_zero() {
                overdue_deadlines_window += 1;
                max_deadline_overdue_us =
                    max_deadline_overdue_us.max(overdue.as_micros().min(u64::MAX as u128) as u64);
            }
            if let Err(e) = rtc.rtc.handle_input(Input::Timeout(now)) {
                warn!(?e, "timeout input failed");
            }
            continue 'run;
        }

        // A static desktop can leave DDA with no new presents indefinitely.
        // Replaying the cached IDR uses a new RTP timestamp in write_video and
        // is one bounded RTC mutation followed by the next loop's full drain.
        if rtc.ice_connected && cached_idr_replay.is_due(now, latest_idr.is_some()) {
            let cached = latest_idr.as_ref().expect("checked above");
            // A staged profile invalidates the cache immediately, and the tag
            // is a second guard against cross-dimension SPS/PPS replay.
            if cached.mode != profiles.active || profiles.staged().is_some() {
                latest_idr = None;
                continue 'run;
            }
            let encoded_bytes = cached.data.len();
            match rtc.write_video(cached.data.clone()) {
                Err(e) => {
                    warn!(?e, "cached IDR replay failed");
                    cached_idr_replay.on_not_admitted(now);
                }
                Ok(false) => {
                    debug!("cached IDR replay was not admitted");
                    cached_idr_replay.on_not_admitted(now);
                }
                Ok(true) => {
                    frames_sent += 1;
                    admitted_frames_window += 1;
                    admitted_bytes_window += encoded_bytes as u64;
                    last_idr_sent = now;
                    next_idr = now + IDR_INTERVAL;
                    cached_idr_replay.on_replayed();
                    announce_admitted_profile(
                        &mut profiles,
                        &mut signaling,
                        encoder
                            .as_ref()
                            .map(Nvenc::bitrate)
                            .unwrap_or(quality.target()),
                        quality.bwe(),
                    );
                    info!(
                        frames_sent,
                        encoded_bytes, "cached IDR replayed for static desktop"
                    );
                }
            }
            continue 'run;
        }

        // -- Media pump: capture -> encode -> write ---------------------------
        // Poll DDA instead of waiting in it. str0m owns the next wake-up; a
        // new desktop present will be picked up on the next sub-millisecond
        // pump iteration.
        let captured = capture.acquire(Duration::ZERO)?;

        if let Some((frame, is_new)) = captured {
            let dev = frame.device().as_raw() as *mut core::ffi::c_void;
            if encoder.as_ref().map_or(true, |enc| !enc.is_for_device(dev)) {
                if encoder.is_some() {
                    warn!("capture D3D11 device changed; recreating scaler and encoder");
                    scaler = None;
                    latest_idr = None;
                }
                // Bootstrap for the texture path that will run immediately
                // below. If this capture device cannot support that rung, the
                // selector pins a truthful native contract before NVENC opens.
                let init_mode =
                    encoder_mode_for_capture(&mut profiles, frame.width(), frame.height());
                match create_encoder_with_fallback(
                    dev,
                    init_mode.width,
                    init_mode.height,
                    frame.width(),
                    frame.height(),
                    init_mode.fps,
                    quality.target(),
                    requested_preset,
                ) {
                    Ok(enc) => {
                        encoder = Some(enc);
                        info!(
                            width = init_mode.width,
                            height = init_mode.height,
                            fps = init_mode.fps,
                            "encoder ready"
                        );
                    }
                    Err(e) => {
                        error!(?e, "encoder init failed");
                        break 'run;
                    }
                }
            }
            // A staged profile bypasses normal admission so it is applied on
            // the very next acquired texture, including a duplicate wake.
            let staged = profiles.staged();
            let now = Instant::now();
            if staged.is_some() || ((is_new || latest_idr.is_none()) && now >= next_frame_admission)
            {
                let old_mode = profiles.active;
                let mut target_mode = profiles.staged.map(|(mode, _)| mode).unwrap_or(old_mode);
                let mut transition_reason = profiles.staged_reason().unwrap_or("startup");
                let mut is_transition = target_mode != old_mode;
                let mut scaler_failed = false;
                let device_ptr = frame.device().as_raw() as *mut core::ffi::c_void;

                let scaled = match prepare_video_mode_texture(
                    &mut scaler,
                    frame.device(),
                    frame.texture(),
                    frame.width(),
                    frame.height(),
                    target_mode,
                ) {
                    Ok(output) => output,
                    Err(e) => {
                        warn!(
                            ?e,
                            "GPU scaler failed; pinning native output for this session"
                        );
                        target_mode =
                            VideoMode::native(frame.width(), frame.height(), target_mode.fps);
                        transition_reason = "scaler-fallback";
                        is_transition = target_mode != old_mode;
                        scaler_failed = true;
                        profiles.pin_native(
                            frame.width(),
                            frame.height(),
                            target_mode.fps,
                            transition_reason,
                        );
                        latest_idr = None;
                        prepare_video_mode_texture(
                            &mut scaler,
                            frame.device(),
                            frame.texture(),
                            frame.width(),
                            frame.height(),
                            target_mode,
                        )?
                    }
                };
                scale_sum_us = scale_sum_us.saturating_add(scaled.scale_us);
                scale_max_us = scale_max_us.max(scaled.scale_us);
                scale_count += 1;

                if is_transition {
                    latest_idr = None;
                    if let Err(target_error) = configure_encoder_profile(
                        &mut encoder,
                        device_ptr,
                        target_mode,
                        frame.width(),
                        frame.height(),
                        quality.target(),
                        requested_preset,
                    ) {
                        warn!(
                            ?target_error,
                            target = target_mode.label,
                            "target profile unavailable; restoring last working profile"
                        );
                        let mut fallback_mode = if scaler_failed { target_mode } else { old_mode };
                        let fallback_texture = match prepare_video_mode_texture(
                            &mut scaler,
                            frame.device(),
                            frame.texture(),
                            frame.width(),
                            frame.height(),
                            fallback_mode,
                        ) {
                            Ok(output) => output,
                            Err(scale_error) => {
                                warn!(
                                    ?scale_error,
                                    "last scaled profile unavailable; falling back to native"
                                );
                                fallback_mode = VideoMode::native(
                                    frame.width(),
                                    frame.height(),
                                    target_mode.fps,
                                );
                                prepare_video_mode_texture(
                                    &mut scaler,
                                    frame.device(),
                                    frame.texture(),
                                    frame.width(),
                                    frame.height(),
                                    fallback_mode,
                                )?
                            }
                        };
                        scale_sum_us = scale_sum_us.saturating_add(fallback_texture.scale_us);
                        scale_max_us = scale_max_us.max(fallback_texture.scale_us);
                        scale_count += 1;
                        let fallback_reason = if fallback_mode.label == "native" {
                            "scaler-fallback"
                        } else {
                            "encoder-fallback"
                        };
                        if fallback_mode.label == "native" {
                            profiles.pin_native(
                                fallback_mode.width,
                                fallback_mode.height,
                                fallback_mode.fps,
                                fallback_reason,
                            );
                        } else {
                            profiles.pin(fallback_mode.profile, fallback_mode.fps, fallback_reason);
                        }
                        match recreate_and_encode_profile(
                            &mut encoder,
                            device_ptr,
                            fallback_mode,
                            frame.width(),
                            frame.height(),
                            quality.target(),
                            requested_preset,
                            fallback_texture.texture,
                        ) {
                            Ok(encoded) => {
                                encode_sum_us = encode_sum_us.saturating_add(encoded.encode_us);
                                encode_max_us = encode_max_us.max(encoded.encode_us);
                                encode_count += 1;
                                latest_idr = Some(CachedIdr {
                                    data: encoded.data.clone(),
                                    mode: profiles.active,
                                });
                                if rtc.ice_connected {
                                    let encoded_bytes = encoded.data.len();
                                    match rtc.write_video(encoded.data) {
                                        Err(e) => {
                                            warn!(?e, "fallback IDR write failed");
                                            cached_idr_replay.on_not_admitted(Instant::now());
                                        }
                                        Ok(false) => {
                                            debug!("fallback IDR was not admitted");
                                            cached_idr_replay.on_not_admitted(Instant::now());
                                        }
                                        Ok(true) => {
                                            frames_sent += 1;
                                            admitted_frames_window += 1;
                                            admitted_bytes_window += encoded_bytes as u64;
                                            last_idr_sent = Instant::now();
                                            next_idr = last_idr_sent + IDR_INTERVAL;
                                            cached_idr_replay.on_fresh_idr_admitted();
                                            announce_admitted_profile(
                                                &mut profiles,
                                                &mut signaling,
                                                encoder
                                                    .as_ref()
                                                    .map(Nvenc::bitrate)
                                                    .unwrap_or(quality.target()),
                                                quality.bwe(),
                                            );
                                            info!(
                                                frames_sent,
                                                encoded_bytes,
                                                profile = fallback_mode.label,
                                                "fallback IDR sent"
                                            );
                                        }
                                    }
                                }
                            }
                            Err(fallback_error) => {
                                error!(
                                    ?fallback_error,
                                    "failed to restore last working encoder profile"
                                );
                                encoder = None;
                            }
                        }
                        continue 'run;
                    }
                }

                let bootstrap_encode = latest_idr.is_none();
                if is_transition || bootstrap_encode || now >= next_idr {
                    if let Some(enc) = encoder.as_mut() {
                        enc.force_idr();
                    }
                    next_idr = now + IDR_INTERVAL;
                }
                let encoded_attempt = encoder
                    .as_mut()
                    .context("encoder unavailable")?
                    .encode_texture(scaled.texture);
                let transition_failed = is_transition
                    && encoded_attempt
                        .as_ref()
                        .map_or(true, |encoded| !encoded.is_idr);
                let encoded = if transition_failed {
                    let target_error = match encoded_attempt {
                        Ok(_) => anyhow::anyhow!("target encoder did not return the forced IDR"),
                        Err(e) => e,
                    };
                    warn!(
                        ?target_error,
                        target = target_mode.label,
                        "profile transition encode failed; restoring last working profile"
                    );
                    let mut fallback_mode = old_mode;
                    let fallback_texture = match prepare_video_mode_texture(
                        &mut scaler,
                        frame.device(),
                        frame.texture(),
                        frame.width(),
                        frame.height(),
                        fallback_mode,
                    ) {
                        Ok(output) => output,
                        Err(scale_error) => {
                            warn!(?scale_error, "fallback scale failed; pinning native output");
                            fallback_mode =
                                VideoMode::native(frame.width(), frame.height(), target_mode.fps);
                            prepare_video_mode_texture(
                                &mut scaler,
                                frame.device(),
                                frame.texture(),
                                frame.width(),
                                frame.height(),
                                fallback_mode,
                            )?
                        }
                    };
                    scale_sum_us = scale_sum_us.saturating_add(fallback_texture.scale_us);
                    scale_max_us = scale_max_us.max(fallback_texture.scale_us);
                    scale_count += 1;
                    let fallback_reason = if fallback_mode.label == "native" {
                        "scaler-fallback"
                    } else {
                        "encoder-fallback"
                    };
                    if fallback_mode.label == "native" {
                        profiles.pin_native(
                            fallback_mode.width,
                            fallback_mode.height,
                            fallback_mode.fps,
                            fallback_reason,
                        );
                    } else {
                        profiles.pin(fallback_mode.profile, fallback_mode.fps, fallback_reason);
                    }
                    // The replacement encoder is now the actual applied
                    // profile. Do not relabel its IDR as the failed target.
                    is_transition = false;
                    match recreate_and_encode_profile(
                        &mut encoder,
                        device_ptr,
                        fallback_mode,
                        frame.width(),
                        frame.height(),
                        quality.target(),
                        requested_preset,
                        fallback_texture.texture,
                    ) {
                        Ok(encoded) => encoded,
                        Err(fallback_error) => {
                            error!(
                                ?fallback_error,
                                "failed to restore and encode last working profile"
                            );
                            encoder = None;
                            continue 'run;
                        }
                    }
                } else {
                    match encoded_attempt {
                        Ok(encoded) => encoded,
                        Err(e) => {
                            warn!(?e, "encode failed; forcing IDR on next frame");
                            if let Some(enc) = encoder.as_mut() {
                                enc.force_idr();
                            }
                            continue 'run;
                        }
                    }
                };

                if is_transition && encoded.is_idr {
                    profiles.commit_mode(target_mode, transition_reason);
                }
                let active_profile = profiles.active();
                let active_fps = profiles.active_fps();
                next_frame_admission =
                    Instant::now() + Duration::from_secs_f64(1.0 / active_fps as f64);
                encode_sum_us = encode_sum_us.saturating_add(encoded.encode_us);
                encode_max_us = encode_max_us.max(encoded.encode_us);
                encode_count += 1;
                let is_idr = encoded.is_idr;
                if is_idr {
                    latest_idr = Some(CachedIdr {
                        data: encoded.data.clone(),
                        mode: profiles.active,
                    });
                }
                if rtc.ice_connected {
                    let encoded_bytes = encoded.data.len();
                    let encode_us = encoded.encode_us;
                    match rtc.write_video(encoded.data) {
                        Err(e) => {
                            warn!(?e, "write_video failed");
                            if is_idr {
                                cached_idr_replay.on_not_admitted(Instant::now());
                            }
                        }
                        Ok(false) => {
                            if is_idr {
                                cached_idr_replay.on_not_admitted(Instant::now());
                            }
                        }
                        Ok(true) => {
                            frames_sent += 1;
                            admitted_frames_window += 1;
                            admitted_bytes_window += encoded_bytes as u64;
                            if is_idr {
                                last_idr_sent = Instant::now();
                                next_idr = last_idr_sent + IDR_INTERVAL;
                                cached_idr_replay.on_fresh_idr_admitted();
                                announce_admitted_profile(
                                    &mut profiles,
                                    &mut signaling,
                                    encoder
                                        .as_ref()
                                        .map(Nvenc::bitrate)
                                        .unwrap_or(quality.target()),
                                    quality.bwe(),
                                );
                                info!(
                                    frames_sent,
                                    encoded_bytes,
                                    profile = active_profile.name(),
                                    "IDR sent"
                                );
                            }
                            if frames_sent % 300 == 0 {
                                info!(
                                    frames_sent,
                                    encode_ms = format!("{:.2}", encode_us as f64 / 1000.0),
                                    idr = is_idr,
                                    "stream stats"
                                );
                            }
                        }
                    }
                }

                // write_video queued packets; drain them before any wait.
                continue 'run;
            }
        }

        let now = Instant::now();
        if connected_since.is_some() && now >= next_transport_log {
            let elapsed = now
                .duration_since(transport_window_started)
                .as_secs_f64()
                .max(0.001);
            info!(
                tx_mbps = format!(
                    "{:.2}",
                    tx_bytes_window as f64 * 8.0 / elapsed / 1_000_000.0
                ),
                tx_pps = format!("{:.0}", tx_packets_window as f64 / elapsed),
                admitted_mbps = format!(
                    "{:.2}",
                    admitted_bytes_window as f64 * 8.0 / elapsed / 1_000_000.0
                ),
                admitted_fps = format!("{:.1}", admitted_frames_window as f64 / elapsed),
                overdue_deadlines = overdue_deadlines_window,
                max_overdue_us = max_deadline_overdue_us,
                scale_us_sum = scale_sum_us,
                scale_us_max = scale_max_us,
                scale_count,
                encode_us_sum = encode_sum_us,
                encode_us_max = encode_max_us,
                encode_count,
                "RTC transport"
            );
            transport_window_started = now;
            next_transport_log = now + Duration::from_secs(1);
            tx_packets_window = 0;
            tx_bytes_window = 0;
            admitted_frames_window = 0;
            admitted_bytes_window = 0;
            overdue_deadlines_window = 0;
            max_deadline_overdue_us = 0;
            scale_sum_us = 0;
            scale_max_us = 0;
            scale_count = 0;
            encode_sum_us = 0;
            encode_max_us = 0;
            encode_count = 0;
        }

        // Stay responsive to new desktop presents while respecting str0m's
        // next deadline. A one-millisecond cap avoids the old 16 ms blind
        // spot without burning an entire CPU core in a spin loop.
        let wait = timeout.saturating_duration_since(Instant::now());
        if !wait.is_zero() {
            std::thread::sleep(wait.min(Duration::from_millis(1)));
        }
    }

    input_sink.release_all();
    signaling.send_bye();
    let secs = connected_since
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    info!(
        frames_sent,
        connected_secs = format!("{secs:.1}"),
        "host exiting"
    );
    Ok(())
}

/// Complete str0m's required mutation -> drain cycle while local candidates
/// are configured before SDP negotiation. No application-level events are
/// expected yet, but handling them keeps this helper correct if str0m adds one.
fn drain_setup_outputs(rtc: &mut RtcSession, socket: &UdpSocket) -> Result<()> {
    loop {
        match rtc.rtc.poll_output() {
            Ok(Output::Transmit(t)) => {
                socket
                    .send_to(&t.contents, t.destination)
                    .with_context(|| format!("send setup datagram to {}", t.destination))?;
            }
            Ok(Output::Event(e)) => {
                let _ = rtc.on_event(e);
            }
            Ok(Output::Timeout(_)) => return Ok(()),
            Err(e) => return Err(e).context("drain RTC setup output"),
        }
    }
}

/// Construct an RTC epoch just in time for SDP negotiation. A str0m instance
/// can expire while the host waits for a browser, so never reuse an idle one
/// for its first offer. Every candidate mutation is followed by a full output
/// drain before the next mutation or SDP operation.
fn fresh_rtc_epoch(
    socket: &UdpSocket,
    host_addr: SocketAddr,
    mapped_addr: Option<SocketAddr>,
    initial_bitrate: u32,
) -> Result<RtcSession> {
    let mut rtc = RtcSession::new(Instant::now(), initial_bitrate);
    rtc.add_host_candidate(host_addr)?;
    drain_setup_outputs(&mut rtc, socket)?;

    if let Some(mapped) = mapped_addr {
        rtc.add_srflx_candidate(mapped, host_addr)?;
        drain_setup_outputs(&mut rtc, socket)?;
        info!(%mapped, "cached srflx candidate ready for SDP answer");
    }
    Ok(rtc)
}

/// A connected transport owns its current peer. Extra offer messages at that
/// point are duplicates, not a request to tear down live media/input.
fn should_start_fresh_rtc_epoch(ice_connected: bool) -> bool {
    !ice_connected
}

/// Pre-answer ICE stays outside RtcSession so accepting SDP remains one RTC
/// mutation. The caller applies one candidate only after an answer is sent.
fn take_remote_candidate_after_answer(
    offer_accepted: bool,
    pending: &mut VecDeque<String>,
) -> Option<String> {
    offer_accepted.then(|| pending.pop_front()).flatten()
}

/// Discover the local interface IP used for the default route (UDP-connect trick;
/// no packets are sent).
fn pick_host_ip() -> Result<std::net::IpAddr> {
    let probe = UdpSocket::bind("0.0.0.0:0")?;
    let stun_addr: SocketAddr =
        format!("{STUN_SERVER}")
            .parse()
            .or_else(|_| -> Result<SocketAddr> {
                Ok(std::net::ToSocketAddrs::to_socket_addrs(STUN_SERVER)?
                    .next()
                    .context("no stun addr")?)
            })?;
    probe.connect(stun_addr)?;
    Ok(probe.local_addr()?.ip())
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
    use super::{
        active_encoder_dimensions, encoder_mode_for_capture, should_start_fresh_rtc_epoch,
        take_remote_candidate_after_answer, CachedIdr, CachedIdrReplay, ProfileTransition,
        VideoMode, STATIC_REPLAY_DELAY,
    };
    use crate::quality::Profile;
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    #[test]
    fn unconnected_offer_replaces_the_idle_rtc_epoch_but_connected_duplicate_does_not() {
        assert!(should_start_fresh_rtc_epoch(false));
        assert!(!should_start_fresh_rtc_epoch(true));
    }

    #[test]
    fn early_remote_ice_stays_queued_until_after_the_answer_epoch() {
        let mut queued = VecDeque::from(["candidate-a".to_string()]);
        assert_eq!(take_remote_candidate_after_answer(false, &mut queued), None);
        assert_eq!(queued.len(), 1);
        assert_eq!(
            take_remote_candidate_after_answer(true, &mut queued).as_deref(),
            Some("candidate-a")
        );
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
    fn device_replacement_below_active_rung_pins_exact_native_mode() {
        let mut state = ProfileTransition::new(Profile::Full, Profile::Full);
        assert_eq!(
            encoder_mode_for_capture(&mut state, 1600, 900),
            VideoMode::native(1600, 900, 60)
        );
        assert!(state.is_pinned());
        assert_eq!(state.staged(), None);
        assert_eq!(
            state.pending_announcement(),
            Some((VideoMode::native(1600, 900, 60), "native-size-fallback"))
        );
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
