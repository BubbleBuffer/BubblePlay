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
mod rtc;
mod signaling;
mod stun;
mod vdd;

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use str0m::net::{Protocol, Receive};
use str0m::{Input, Output};
use tracing::{debug, error, info, warn};
use windows::core::Interface;

use capture::{pick_monitor, texture_ptr, Capture};
use encode::Nvenc;
use input::InputSink;
use rtc::{RtcAction, RtcSession};
use signaling::{SignalEvent, Signaling};

const STUN_SERVER: &str = "stun.l.google.com:19302";
const DEFAULT_BITRATE_BPS: u32 = 20_000_000;
const MIN_BITRATE_BPS: u32 = 1_000_000;
const MAX_BITRATE_BPS: u32 = 40_000_000;
const IDR_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct Args {
    mode: String,      // "spike" | "serve"
    worker: Option<String>,
    token: Option<String>,
    duration_secs: u32,
    bitrate_bps: u32,
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
            "--out" => args.out = it.next().context("--out value")?,
            "--monitor" => args.monitor = Some(it.next().context("--monitor value")?),
            "--vdd" => args.vdd = true,
            "-h" | "--help" => {
                println!("bubbleplay-host --mode spike|serve [--worker wss://...] [--token ...] [--duration secs] [--bitrate bps] [--out file] [--monitor primary|<index>|<name-substr>] [--vdd]");
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}"),
        }
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
    loop {
        match run_serve(args) {
            Ok(()) => {
                info!("session ended — waiting for next client");
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(e) => {
                error!(?e, "serve session failed fatally — exiting");
                return Err(e);
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
    let worker = args.worker.clone().context("--worker wss://... is required")?;
    let token = args.token.clone().context("--token is required")?;

    // 1. Media UDP socket. Bind wildcard; discover the routed local IP for the
    //    host candidate via a throwaway connected socket.
    let socket = UdpSocket::bind("0.0.0.0:0").context("bind media socket")?;
    let local_port = socket.local_addr()?.port();
    let host_ip = pick_host_ip().context("discover local host IP")?;
    let host_addr: SocketAddr = SocketAddr::new(host_ip, local_port);
    info!(%host_addr, "media socket bound");

    // 2. Signaling.
    let mut signaling = Signaling::connect(&worker, &token)?;

    // 3. RTC session.
    let mut rtc = RtcSession::new(Instant::now());
    rtc.add_host_candidate(host_addr)?;

    // 4. Capture (+ warm up encoder from the first frame).
    let _vdd = start_vdd_keepalive(args)?;
    let mut capture = Capture::new(args.monitor.as_deref()).context("capture init")?;
    let mut encoder: Option<Nvenc> = None;
    let mut input_sink = InputSink::new();

    let mut srflx_done = false;
    let mut srflx_failures = 0u32;
    let mut next_idr = Instant::now() + IDR_INTERVAL;
    let mut next_signal_ping = Instant::now() + Duration::from_secs(20);
    let mut offer_accepted = false;
    let mut connected_since: Option<Instant> = None;
    let mut frames_sent = 0u64;
    let mut input_packets = 0u64;
    let mut input_bytes = 0u64;
    let mut buf = vec![0u8; 2000];

    'run: loop {
        let _now = Instant::now();

        // -- Signaling events ----------------------------------------------
        if Instant::now() >= next_signal_ping {
            signaling.send_ping();
            next_signal_ping = Instant::now() + Duration::from_secs(20);
        }
        let signal_events = match signaling.poll() {
            Ok(evs) => evs,
            Err(e) => {
                // Transient signaling drops (edge recycling, DO eviction,
                // network blips) must end the session so the outer loop can
                // reconnect — they are NOT fatal.
                warn!(?e, "signaling read failed — ending session to reconnect");
                break 'run;
            }
        };
        for ev in signal_events {
            match ev {
                SignalEvent::Offer(sdp) => {
                    info!("received SDP offer");
                    match rtc.accept_offer(&sdp) {
                        Ok(answer) => {
                            if let Err(e) = signaling.send_answer(&answer) {
                                warn!(?e, "send answer failed — ending session");
                                break 'run;
                            }
                            offer_accepted = true;
                            info!("SDP answer sent");
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
                SignalEvent::RemoteCandidate(sdp) => rtc.add_remote_candidate(&sdp),
                SignalEvent::PeerLeft => {
                    info!("peer left — exiting");
                    break 'run;
                }
                SignalEvent::Registered => {}
            }
        }

        // -- srflx discovery (once, from the media socket) -------------------
        if offer_accepted && !srflx_done {
            match stun::discover_mapped_address(&socket, STUN_SERVER) {
                Ok(mapped) => {
                    let cand = rtc.add_srflx_candidate(mapped, host_addr)?;
                    signaling.send_candidate(&cand.to_sdp_string())?;
                    srflx_done = true;
                    info!(%mapped, "srflx candidate trickled");
                }
                Err(e) => {
                    srflx_failures += 1;
                    warn!(?e, srflx_failures, "STUN discovery failed (will retry)");
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }

        // -- str0m outputs ----------------------------------------------------
        let timeout;
        loop {
            match rtc.rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    if let Err(e) = socket.send_to(&t.contents, t.destination) {
                        warn!(?e, dest = %t.destination, "UDP send failed");
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
                                connected_since = Some(Instant::now());
                                next_idr = Instant::now(); // immediate IDR
                            }
                            RtcAction::IceDisconnected | RtcAction::Closed => {
                                info!("ICE gone — session closed");
                                break 'run;
                            }
                            RtcAction::KeyframeRequested => {
                                if let Some(enc) = encoder.as_mut() {
                                    enc.force_idr();
                                }
                            }
                            RtcAction::Bwe(bps) => {
                                if let Some(enc) = encoder.as_mut() {
                                    let clamped =
                                        bps.clamp(MIN_BITRATE_BPS as u64, MAX_BITRATE_BPS as u64)
                                            as u32;
                                    let _ = enc.set_bitrate(clamped);
                                }
                            }
                            RtcAction::InputData(d) => {
                                input_packets += 1;
                                input_bytes += d.len() as u64;
                                input_sink.handle(&d);
                                if input_packets <= 20 || input_packets % 500 == 0 {
                                    info!(
                                        input_packets,
                                        input_bytes,
                                        "input packets applied via SendInput"
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

        // -- UDP ingress ------------------------------------------------------
        let wait = timeout.saturating_duration_since(Instant::now());
        let read_wait = wait.min(Duration::from_millis(16)).max(Duration::from_millis(1));
        socket.set_read_timeout(Some(read_wait)).ok();
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                let Ok(contents) = buf[..n].try_into() else { continue };
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
                } else {
                    debug!(%source, "ignoring UDP packet (no accepting session)");
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => warn!(?e, "UDP recv failed"),
        }
        let _ = rtc.rtc.handle_input(Input::Timeout(Instant::now()));

        // -- Media pump: capture -> encode -> write ---------------------------
        let Some((frame, is_new)) = capture.acquire(Duration::from_millis(4))? else {
            continue;
        };

        if encoder.is_none() {
            let dev = frame.device().as_raw() as *mut core::ffi::c_void;
            match Nvenc::new(dev, frame.width(), frame.height(), 60, args.bitrate_bps) {
                Ok(enc) => {
                    encoder = Some(enc);
                    info!("encoder ready");
                }
                Err(e) => {
                    error!(?e, "encoder init failed");
                    break 'run;
                }
            }
        }
        if !is_new {
            continue;
        }

        // Periodic IDR.
        if Instant::now() >= next_idr {
            if let Some(enc) = encoder.as_mut() {
                enc.force_idr();
            }
            next_idr = Instant::now() + IDR_INTERVAL;
        }

        let enc = match encoder.as_mut() {
            Some(e) => e,
            None => continue,
        };
        match enc.encode_texture(texture_ptr(&frame)) {
            Ok(encoded) => {
                if rtc.ice_connected {
                    if let Err(e) = rtc.write_video(encoded.data) {
                        warn!(?e, "write_video failed");
                    } else {
                        frames_sent += 1;
                        if frames_sent % 300 == 0 {
                            info!(frames_sent, encode_ms = format!("{:.2}", encoded.encode_us as f64 / 1000.0), idr = encoded.is_idr, "stream stats");
                        }
                    }
                }
            }
            Err(e) => {
                warn!(?e, "encode failed; forcing IDR on next frame");
                if let Some(enc) = encoder.as_mut() {
                    enc.force_idr();
                }
            }
        }
    }

    signaling.send_bye();
    let secs = connected_since.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
    info!(frames_sent, connected_secs = format!("{secs:.1}"), "host exiting");
    Ok(())
}

/// Discover the local interface IP used for the default route (UDP-connect trick;
/// no packets are sent).
fn pick_host_ip() -> Result<std::net::IpAddr> {
    let probe = UdpSocket::bind("0.0.0.0:0")?;
    let stun_addr: SocketAddr = format!("{STUN_SERVER}")
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
            warn!(?e, "VDD keepalive unavailable — physical-monitor capture only");
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
