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

use std::collections::VecDeque;
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
const DEFAULT_BITRATE_BPS: u32 = 8_000_000;
const MIN_BITRATE_BPS: u32 = 1_000_000;
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
}

#[derive(Debug)]
struct Args {
    mode: String, // "spike" | "serve"
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
    let mut rtc = RtcSession::new(Instant::now());

    // 4. Capture (+ warm up encoder from the first frame).
    let _vdd = start_vdd_keepalive(args)?;
    let mut capture = Capture::new(args.monitor.as_deref()).context("capture init")?;
    let mut encoder: Option<Nvenc> = None;
    let mut input_sink = InputSink::new();
    let mut latest_idr: Option<Vec<u8>> = None;
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
    let mut tx_packets_window = 0u64;
    let mut tx_bytes_window = 0u64;
    let mut admitted_frames_window = 0u64;
    let mut admitted_bytes_window = 0u64;
    let mut overdue_deadlines_window = 0u64;
    let mut max_deadline_overdue_us = 0u64;
    let mut buf = vec![0u8; 2000];

    'run: loop {
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
                                if let Some(enc) = encoder.as_mut() {
                                    let clamped = bps
                                        .clamp(MIN_BITRATE_BPS as u64, MAX_BITRATE_BPS as u64)
                                        as u32;
                                    let _ = enc.set_bitrate(clamped);
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
                    rtc = fresh_rtc_epoch(&socket, host_addr, mapped_addr)?;
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
            let encoded_bytes = cached.len();
            match rtc.write_video(cached.clone()) {
                Err(e) => warn!(?e, "cached IDR replay failed"),
                Ok(false) => debug!("cached IDR replay was not admitted"),
                Ok(true) => {
                    frames_sent += 1;
                    admitted_frames_window += 1;
                    admitted_bytes_window += encoded_bytes as u64;
                    last_idr_sent = now;
                    next_idr = now + IDR_INTERVAL;
                    info!(frames_sent, encoded_bytes, "cached IDR replayed for static desktop");
                }
            }
            cached_idr_replay.on_replayed();
            continue 'run;
        }

        // -- Media pump: capture -> encode -> write ---------------------------
        // Poll DDA instead of waiting in it. str0m owns the next wake-up; a
        // new desktop present will be picked up on the next sub-millisecond
        // pump iteration.
        let captured = capture.acquire(Duration::ZERO)?;

        if let Some((frame, is_new)) = captured {
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
            // Bootstrap once even if DDA's first acquired texture is labelled
            // duplicate, then retain normal duplicate skipping after an IDR is
            // cached for static-first-frame and recovery replay.
            if is_new || latest_idr.is_none() {
                let bootstrap_encode = latest_idr.is_none();
                // Periodic IDR.
                if bootstrap_encode || Instant::now() >= next_idr {
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
                        let is_idr = encoded.is_idr;
                        if is_idr {
                            latest_idr = Some(encoded.data.clone());
                        }
                        if rtc.ice_connected {
                            let encoded_bytes = encoded.data.len();
                            let encode_us = encoded.encode_us;
                            match rtc.write_video(encoded.data) {
                                Err(e) => warn!(?e, "write_video failed"),
                                Ok(false) => {}
                                Ok(true) => {
                                    frames_sent += 1;
                                    admitted_frames_window += 1;
                                    admitted_bytes_window += encoded_bytes as u64;
                                    if is_idr {
                                        last_idr_sent = Instant::now();
                                        next_idr = last_idr_sent + IDR_INTERVAL;
                                        cached_idr_replay.on_fresh_idr_admitted();
                                        info!(frames_sent, encoded_bytes, "IDR sent");
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
                    }
                    Err(e) => {
                        warn!(?e, "encode failed; forcing IDR on next frame");
                        if let Some(enc) = encoder.as_mut() {
                            enc.force_idr();
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
) -> Result<RtcSession> {
    let mut rtc = RtcSession::new(Instant::now());
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
        should_start_fresh_rtc_epoch, take_remote_candidate_after_answer, CachedIdrReplay,
        STATIC_REPLAY_DELAY,
    };
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
}
