//! Headless qualification: a real D3D11 device and Media Foundation decode with
//! no window, no presentation and the fake audio sink. Decode-free runs are
//! explicit (`--dev-count-only`) or follow a typed `NoHardwareVideoDecoder`
//! preflight, and each reports its own outcome. Nothing here touches OS input.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use windows::{
    core::Interface,
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
                D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
            },
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                D3D11_SDK_VERSION,
            },
        },
    },
};

use super::{
    decode::{self, DecodeCallbacks, FrameMeta},
    metrics::percentile,
    options::{require_loopback_websocket, CodecChoice, InputScript},
    viewer_auth, write_dev_report, DevOptions, Diagnostics, Running,
};
use crate::{
    audio::{AudioPlayer, OutputMode},
    input::{
        keyboard::PhysicalKey,
        protocol::{self, Sequence},
    },
    media::{Codec, DecodedFrame, MediaRuntime, NoHardwareVideoDecoder},
    net::{self, InputLink, NetConfig, NetEvent, TransportStats},
};

const DEFAULT_DURATION: Duration = Duration::from_secs(20);
const TICK: Duration = Duration::from_millis(10);
const INPUT_INTERVAL: Duration = Duration::from_millis(50);
/// Retained samples per series; a 20 s run at 60 fps fits with room to spare.
const SAMPLE_CAP: usize = 8192;

const VK_A: u16 = 0x41;
const KEY_A: PhysicalKey = PhysicalKey {
    scan: 0x1E,
    extended: false,
    vk: VK_A,
};

/// Synthetic input steps. Only the production encoders appear here; the
/// text (11) and clipboard (12) builders are test-only and are not used.
/// Release goes through `InputLink::release`, whose barrier sends type 6.
const WORK_STEPS: &[Step] = &[
    Step::Absolute {
        x: 32_768,
        y: 32_768,
    },
    Step::ButtonsAt {
        x: 32_768,
        y: 32_768,
        mask: 1,
    },
    Step::Buttons(0),
    Step::Key {
        vk: VK_A,
        up: false,
    },
    Step::ScanKey {
        key: KEY_A,
        up: false,
    },
    Step::ScanKey {
        key: KEY_A,
        up: true,
    },
    Step::Key { vk: VK_A, up: true },
    Step::CursorSync,
    Step::Absolute {
        x: 49_152,
        y: 16_384,
    },
    Step::Release,
];

const GAME_STEPS: &[Step] = &[
    Step::Motion { dx: 40, dy: 0 },
    Step::Motion { dx: -40, dy: 0 },
    Step::Buttons(1),
    Step::Buttons(0),
    Step::Key {
        vk: VK_A,
        up: false,
    },
    Step::Key { vk: VK_A, up: true },
    Step::ScanKey {
        key: KEY_A,
        up: false,
    },
    Step::ScanKey {
        key: KEY_A,
        up: true,
    },
    Step::Release,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Key { vk: u16, up: bool },
    ScanKey { key: PhysicalKey, up: bool },
    Buttons(u8),
    Motion { dx: i16, dy: i16 },
    Absolute { x: u16, y: u16 },
    ButtonsAt { x: u16, y: u16, mask: u8 },
    CursorSync,
    Release,
}

impl Step {
    fn sequenced(self) -> bool {
        matches!(
            self,
            Step::Motion { .. } | Step::Absolute { .. } | Step::ButtonsAt { .. } | Step::CursorSync
        )
    }

    /// `None` for `Release`, which the link turns into its own barrier.
    fn packet(self, seq: u16) -> Option<Vec<u8>> {
        Some(match self {
            Step::Key { vk, up } => protocol::key(vk, up).to_vec(),
            Step::ScanKey { key, up } => protocol::scan_key(key, up).to_vec(),
            Step::Buttons(mask) => protocol::buttons(mask).to_vec(),
            Step::Motion { dx, dy } => protocol::motion(seq, dx, dy, 0, 0).to_vec(),
            Step::Absolute { x, y } => protocol::absolute(seq, x, y).to_vec(),
            Step::ButtonsAt { x, y, mask } => protocol::buttons_at(seq, x, y, mask).to_vec(),
            Step::CursorSync => protocol::cursor_sync(seq).to_vec(),
            Step::Release => return None,
        })
    }
}

/// Plays one script on the InputLink, only after the net layer has admitted
/// it, which happens only once a frame has actually decoded.
struct InputDriver {
    steps: &'static [Step],
    next: usize,
    next_at: Instant,
    epoch: Option<u64>,
    sequence: Sequence,
    sent: u64,
    refused: u64,
}

impl InputDriver {
    fn new(script: InputScript) -> Self {
        Self {
            steps: match script {
                InputScript::Work => WORK_STEPS,
                InputScript::Game => GAME_STEPS,
            },
            next: 0,
            next_at: Instant::now(),
            epoch: None,
            sequence: Sequence::new(),
            sent: 0,
            refused: 0,
        }
    }

    fn tick(&mut self, link: &InputLink, now: Instant) {
        let snapshot = link.snapshot();
        if !snapshot.ready || now < self.next_at {
            return;
        }
        if self.epoch != Some(snapshot.epoch) {
            self.epoch = Some(snapshot.epoch);
            self.sequence.enter_epoch(&snapshot.epoch.to_string());
        }
        let step = self.steps[self.next];
        self.next = (self.next + 1) % self.steps.len();
        self.next_at = now + INPUT_INTERVAL;
        let seq = if step.sequenced() {
            self.sequence.alloc()
        } else {
            0
        };
        match step.packet(seq) {
            None => link.release(snapshot.epoch),
            Some(packet) => {
                if link.send(snapshot.epoch, packet) {
                    self.sent += 1;
                } else {
                    self.refused += 1;
                    if step.sequenced() {
                        self.sequence.release(seq);
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Decoded,
    CountOnly,
    CounterOnly,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Outcome::Decoded => "decoded",
            Outcome::CountOnly => "count-only",
            Outcome::CounterOnly => "counter-only",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Facts {
    errors: usize,
    received: u64,
    decoded: u64,
    decode_errors: u64,
    /// `None` when no script was requested.
    input_sent: Option<u64>,
}

/// Counter-only never passes: it proves frames arrived, not that they decoded.
fn passes(outcome: Outcome, facts: &Facts) -> bool {
    if facts.errors > 0 {
        return false;
    }
    match outcome {
        Outcome::Decoded => {
            facts.decoded > 0
                && facts.decode_errors == 0
                && facts.input_sent.is_none_or(|sent| sent > 0)
        }
        Outcome::CountOnly => facts.received > 0,
        Outcome::CounterOnly => false,
    }
}

/// Decode-side facts, written only on the decode thread and read after join.
#[derive(Default)]
struct Measure {
    decoded: u64,
    dropped: u64,
    decode_errors: u64,
    decoded_at: VecDeque<Instant>,
    receipt_to_decode_ms: VecDeque<f64>,
    codec: Option<Codec>,
    resolution: Option<(u32, u32)>,
    generation: Option<u32>,
}

impl Measure {
    fn note_decoded(&mut self, meta: FrameMeta, decoded_at: Instant) {
        self.generation = Some(meta.generation);
        self.decoded += 1;
        push_capped(&mut self.decoded_at, decoded_at);
        let ms = decoded_at
            .saturating_duration_since(meta.received_at)
            .as_secs_f64()
            * 1000.0;
        push_capped(&mut self.receipt_to_decode_ms, ms);
    }
}

fn push_capped<T>(samples: &mut VecDeque<T>, value: T) {
    if samples.len() == SAMPLE_CAP {
        samples.pop_front();
    }
    samples.push_back(value);
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Creates the D3D11 device the decoder shares: hardware first, WARP only as a
/// last resort. Shared by the headless session and the decoder probe.
pub(super) fn create_device() -> Result<ID3D11Device> {
    match create_device_on(D3D_DRIVER_TYPE_HARDWARE) {
        Ok(device) => Ok(device),
        Err(hardware) => create_device_on(D3D_DRIVER_TYPE_WARP)
            .with_context(|| format!("hardware D3D11 device failed first: {hardware:#}")),
    }
}

fn create_device_on(driver: D3D_DRIVER_TYPE) -> Result<ID3D11Device> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.context("D3D11 device")?;
    let context = context.context("D3D11 immediate context")?;
    // The decode thread shares this device, so its calls must be serialised.
    let multithread: ID3D11Multithread = context.cast()?;
    let _ = unsafe { multithread.SetMultithreadProtected(true) };
    Ok(device)
}

/// `None` when hardware decode is available, or the typed reason it is not.
/// Any other failure is a real error and never becomes a counter-only run.
fn preflight(codec: Codec, device: &ID3D11Device) -> Result<Option<&'static str>> {
    match crate::media::hardware_decoder_available(codec, device) {
        Ok(true) => Ok(None),
        Ok(false) => Ok(Some("no hardware decoder reported")),
        Err(error) => match error.downcast_ref::<NoHardwareVideoDecoder>() {
            Some(typed) => Ok(Some(typed.reason)),
            None => Err(error),
        },
    }
}

/// Relay origin for `NetConfig`. The loopback check runs before any device,
/// audio or network work starts.
fn relay_origin(value: &str) -> Result<String> {
    require_loopback_websocket(value)?;
    let mut relay = url::Url::parse(value).context("invalid --dev-wss URL")?;
    let scheme = match relay.scheme() {
        "ws" => "http",
        "wss" => "https",
        _ => bail!("headless sessions require ws:// or wss://"),
    };
    relay
        .set_scheme(scheme)
        .map_err(|_| anyhow!("invalid WebSocket scheme"))?;
    Ok(relay.to_string())
}

fn gaps_ms(times: &VecDeque<Instant>) -> Vec<f64> {
    times
        .iter()
        .zip(times.iter().skip(1))
        .map(|(a, b)| b.saturating_duration_since(*a).as_secs_f64() * 1000.0)
        .collect()
}

fn summary(values: &[f64]) -> Value {
    let at = |p| percentile(values.iter().copied(), p);
    json!({
        "samples": values.len(),
        "p50_ms": at(0.50),
        "p95_ms": at(0.95),
        "p99_ms": at(0.99),
    })
}

pub(super) fn run(options: DevOptions) -> Result<()> {
    // Refusals come before the first device, audio or network start.
    let endpoint = relay_origin(&options.url)?;
    let viewer = viewer_auth(&options)?;
    let started = Instant::now();
    let mut codec = match options.codec {
        CodecChoice::Auto | CodecChoice::H264 => Codec::H264,
        CodecChoice::Hevc => Codec::Hevc,
    };
    let _runtime = (!options.count_only).then(MediaRuntime::new).transpose()?;
    let (outcome, decoder_reason, device) = if options.count_only {
        (Outcome::CountOnly, None, None)
    } else {
        let device = create_device()?;
        if options.codec == CodecChoice::Auto
            && options.transport != net::TransportChoice::Wss
            && preflight(Codec::Hevc, &device)?.is_none()
        {
            codec = Codec::Hevc;
        }
        match preflight(codec, &device)? {
            None => (Outcome::Decoded, None, Some(device)),
            Some(reason) => (Outcome::CounterOnly, Some(reason), None),
        }
    };

    let player = AudioPlayer::new(OutputMode::Fake)?;
    let audio = player.handle();
    audio.set_volume(options.audio_volume);
    audio.set_muted(options.audio_muted);
    let transport = Arc::new(TransportStats::new());
    let input = InputLink::new(Arc::new(|| {}));
    let net_codec = match codec {
        Codec::H264 => net::Codec::H264,
        Codec::Hevc => net::Codec::Hevc,
    };
    let mut config = NetConfig::new(endpoint, net_codec, viewer);
    config.audio = Some(audio.clone());
    config.input = Some(input.clone());
    config.stream_mode = Some(options.mode.stream_mode().into());
    config.transport = options.transport;
    config.ice_checking_timeout = options
        .ice_timeout
        .unwrap_or(net::AUTO_ICE_CHECKING_TIMEOUT);
    // Without a decoder nothing can prove a frame, so the watchdog and the
    // Decoded readiness gate stay off and the counters are the only evidence.
    let decode_free = device.is_none();
    config.video_watchdog = !decode_free;
    config.headless_decoded_ready = !decode_free;
    let controls = config.controls.clone();

    let stop = Arc::new(AtomicBool::new(false));
    let diagnostics = Arc::new(Mutex::new(Diagnostics::default()));
    let measure = Arc::new(Mutex::new(Measure::default()));
    let (event_tx, event_rx) = mpsc::sync_channel(8);
    let (feedback_tx, feedback_rx) = mpsc::channel();
    let mut running = Running {
        stop: stop.clone(),
        handles: Vec::new(),
        timer_cancel: None,
        diagnostics: diagnostics.clone(),
    };
    running.handles.push(
        thread::Builder::new()
            .name("bubbleplay-headless-wss".into())
            .spawn({
                let stop = stop.clone();
                let transport = transport.clone();
                move || {
                    net::run(config, event_tx, feedback_rx, stop, transport)
                        .map_err(anyhow::Error::new)
                }
            })?,
    );
    let counter_events = match device {
        Some(device) => {
            running.handles.push(
                thread::Builder::new()
                    .name("bubbleplay-headless-decode".into())
                    .spawn({
                        let stop = stop.clone();
                        let diagnostics = diagnostics.clone();
                        let measure = measure.clone();
                        let feedback = feedback_tx.clone();
                        move || {
                            let callbacks = DecodeCallbacks {
                                publish: {
                                    let measure = measure.clone();
                                    move |frame: DecodedFrame,
                                          meta: FrameMeta,
                                          decoded_at: Instant| {
                                        let mut measured = lock(&measure);
                                        measured.codec = Some(frame.codec);
                                        measured.resolution = Some((frame.width, frame.height));
                                        measured.note_decoded(meta, decoded_at);
                                        drop(measured);
                                        // Dropping the frame returns its surface to the pool; nothing is drawn.
                                        drop(frame);
                                    }
                                },
                                reset: {
                                    let diagnostics = diagnostics.clone();
                                    move |epoch: u64, reason: &str, at: Instant| {
                                        lock(&diagnostics).reset(epoch, reason, at, started);
                                    }
                                },
                                error: {
                                    let measure = measure.clone();
                                    let diagnostics = diagnostics.clone();
                                    move |error: String| {
                                        lock(&measure).decode_errors += 1;
                                        lock(&diagnostics).error(error);
                                    }
                                },
                                dropped: {
                                    let measure = measure.clone();
                                    move |count: u64| lock(&measure).dropped += count
                                },
                            };
                            decode::run(device, event_rx, feedback, stop, callbacks)
                        }
                    })?,
            );
            None
        }
        None => Some(event_rx),
    };

    let mut driver = options
        .input_script
        .or(options.session_script.then_some(InputScript::Work))
        .map(InputDriver::new);
    let mut exercise = options.session_script.then(ControlExercise::default);
    let mut received_at: VecDeque<Instant> = VecDeque::new();
    let mut last_received: Option<Instant> = None;
    let duration = options.exit_after.unwrap_or(DEFAULT_DURATION);
    let loop_started = Instant::now();
    while loop_started.elapsed() < duration {
        match &counter_events {
            Some(events) => match events.recv_timeout(TICK) {
                Ok(NetEvent::Reset(reset)) => lock(&diagnostics).reset(
                    reset.local_epoch,
                    &reset.reason,
                    Instant::now(),
                    started,
                ),
                // Frames are counted by the transport; the counter path keeps the queue drained.
                Ok(NetEvent::Frame(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => thread::sleep(TICK),
        }
        for at in transport.receive_timestamps() {
            if last_received.is_some_and(|last| at <= last) {
                continue;
            }
            last_received = Some(at);
            push_capped(&mut received_at, at);
        }
        if let (Some(exercise), Some(driver)) = (&mut exercise, &mut driver) {
            let measured = lock(&measure);
            exercise.tick(
                &controls,
                &input,
                driver,
                &measured,
                lock(&diagnostics).resets.len(),
                transport.snapshot().attempts,
            );
        }
        if let Some(driver) = driver.as_mut() {
            driver.tick(&input, Instant::now());
        }
        if running.handles.iter().any(|handle| handle.is_finished()) {
            break;
        }
    }

    stop.store(true, Ordering::Release);
    // Drop the counter receiver before joining so the net thread never waits on it.
    drop(counter_events);
    let mut failures: Vec<anyhow::Error> = Vec::new();
    for handle in running.handles.drain(..) {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(error),
            Err(_) => failures.push(anyhow!("headless worker panicked")),
        }
    }
    drop(player);
    let audio_stats = audio.stats();
    let net = transport.snapshot();
    let input_snapshot = input.snapshot();
    let typed = failures
        .iter()
        .position(|error| error.is::<NoHardwareVideoDecoder>());
    let (mut errors, resets) = {
        let diagnostics = lock(&diagnostics);
        (diagnostics.errors.clone(), diagnostics.resets.clone())
    };
    errors.extend(failures.iter().map(|error| format!("{error:#}")));
    let measured = lock(&measure);
    let facts = Facts {
        errors: errors.len(),
        received: net.frames_received,
        decoded: measured.decoded,
        decode_errors: measured.decode_errors,
        input_sent: driver.as_ref().map(|driver| driver.sent),
    };
    let pass = passes(outcome, &facts) && exercise.as_ref().is_none_or(|run| run.phase == 4);
    let mut document = json!({
        "pass": pass,
        "headless": true,
        "outcome": outcome.name(),
        "sink": "fake",
        "duration_s": loop_started.elapsed().as_secs_f64(),
        "errors": errors,
        "error_kind": typed.is_some().then_some("no_hardware_video_decoder"),
        "audio": audio_stats,
        "transport": {
            "requested": format!("{:?}", options.transport),
            "state": format!("{:?}", net.state),
            "attempts": net.attempts,
            "established": net.established,
            "local_epoch": net.local_epoch,
            "frames_received": net.frames_received,
            "frames_accepted": net.frames_accepted,
            "gate_drops": net.gate_drops,
            "queue_overflows": net.queue_overflows,
            "acks_sent": net.acks_sent,
            "keyframe_requests": net.keyframe_requests,
            "decoded": net.decoded,
            "last_error": transport.last_error(),
            "webrtc": transport.webrtc_snapshot(),
        },
        "video": {
            "codec": measured.codec.unwrap_or(codec).name(),
            "resolution": measured.resolution,
            "decoder": {
                "preflight": match outcome {
                    Outcome::Decoded => "available",
                    Outcome::CounterOnly => "unavailable",
                    Outcome::CountOnly => "not-run",
                },
                "reason": decoder_reason,
            },
            "received": net.frames_received,
            "decoded": measured.decoded,
            "dropped": measured.dropped+net.gate_drops+net.queue_overflows,
            "decoder_dropped": measured.dropped,
            "decode_errors": measured.decode_errors,
            "resets": resets,
            "received_interval_ms": summary(&gaps_ms(&received_at)),
            "decoded_interval_ms": summary(&gaps_ms(&measured.decoded_at)),
            "receipt_to_decode_ms": summary(
                &measured.receipt_to_decode_ms.iter().copied().collect::<Vec<_>>()
            ),
        },
        "present": null,
        "input": {
            "script": options.input_script.map(|script| match script {
                InputScript::Work => "work",
                InputScript::Game => "game",
            }),
            "driver_sent": driver.as_ref().map(|driver| driver.sent),
            "driver_refused": driver.as_ref().map(|driver| driver.refused),
            "rtt_samples": input_snapshot.rtt_samples,
            "rtt_p50_ms": input_snapshot.rtt_p50_ms,
            "rtt_p95_ms": input_snapshot.rtt_p95_ms,
            "link": input_snapshot,
        },
    });
    let seconds = loop_started.elapsed().as_secs_f64();
    let mut rtc = transport.webrtc_snapshot();
    rtc["measurement_scope"] = json!("whole headless session, including WSS after fallback");
    rtc["fps_average"] = json!({"received":net.frames_received as f64/seconds,"decoded":measured.decoded as f64/seconds,"presented":null});
    rtc["frame_interval_ms"] = document["video"]["decoded_interval_ms"].clone();
    rtc["received_frame_interval_ms"] = document["video"]["received_interval_ms"].clone();
    rtc["receive_to_decoded_ms"] = document["video"]["receipt_to_decode_ms"].clone();
    rtc["receive_to_presented_ms"] = Value::Null;
    rtc["audio_counters"] = document["audio"].clone();
    rtc["input_round_trip_ms"] = json!({"p50":input_snapshot.rtt_p50_ms,"p95":input_snapshot.rtt_p95_ms,"samples":input_snapshot.rtt_samples});
    document["webrtc"] = rtc;
    if let Some(exercise) = &exercise {
        document["session_controls"] = json!({
            "complete": exercise.phase == 4,
            "phase": exercise.phase,
            "gate_reopened": exercise.gate_reopened,
            "gate_closes_before": exercise.gate_closes_before,
            "gate_closes_at_reopen": exercise.gate_closes_at_reopen,
            "gate_closes_after": input_snapshot.gate_closes,
            "monitor": exercise.target,
            "generation_before": exercise.generation_before,
            "generation_after": measured.generation,
            "modes": exercise.modes,
            "game_input_sent": exercise.game_input_sent,
            "work_input_sent": driver.as_ref().map(|d| d.sent.saturating_sub(exercise.input_at_mode)),
            "error": controls.snapshot().error.map(|e| e.reason),
            "selected": controls.snapshot().selected,
            "pending": controls.snapshot().pending,
            "mode_revision": controls.snapshot().mode_revision,
        });
    }
    drop(measured);
    write_dev_report(&options, &document)?;
    if let Some(index) = typed {
        // The typed failure is returned as-is; `finish_dev` leaves this report in place.
        return Err(failures.swap_remove(index));
    }
    if outcome == Outcome::CounterOnly {
        bail!(
            "counter-only: no hardware video decoder ({}); decode is not qualified, see the stats file",
            decoder_reason.unwrap_or("unknown")
        );
    }
    if !pass {
        bail!("headless session did not pass; see the stats file");
    }
    Ok(())
}

/// A bounded synthetic qualification sequence. Net readiness is decode proof;
/// no windows, hooks or OS input participate in any stage.
#[derive(Default)]
struct ControlExercise {
    phase: u8,
    target: Option<String>,
    generation_before: Option<u32>,
    decoded_before: u64,
    resets_before: usize,
    attempts_before: u64,
    gate_reopened: bool,
    gate_closes_before: u64,
    gate_closes_at_reopen: u64,
    mode_revision: u64,
    mode_at: Option<Instant>,
    input_at_mode: u64,
    game_input_sent: u64,
    modes: Vec<&'static str>,
}

impl ControlExercise {
    fn tick(
        &mut self,
        controls: &net::SessionControls,
        input: &InputLink,
        driver: &mut InputDriver,
        measured: &Measure,
        resets: usize,
        attempts: u64,
    ) {
        use crate::model::StreamMode;
        let view = controls.snapshot();
        let input_view = input.snapshot();
        let ready = input_view.ready;
        match self.phase {
            0 if ready && measured.decoded > 0 => {
                let Some(target) = view.monitors.iter().find(|m| !m.selected) else {
                    return;
                };
                if controls.pick_monitor(&target.selector) != net::PickOutcome::Queued {
                    return;
                }
                self.target = Some(target.selector.clone());
                self.generation_before = measured.generation;
                self.decoded_before = measured.decoded;
                self.resets_before = resets;
                self.attempts_before = attempts;
                self.gate_closes_before = input_view.gate_closes;
                self.phase = 1;
            }
            1 if ready
                && view.pending.is_none()
                && view.error.is_none()
                && view.selected == self.target
                && measured.generation != self.generation_before
                && measured.decoded > self.decoded_before
                && resets > self.resets_before
                && input_view.gate_closes > self.gate_closes_before
                && attempts == self.attempts_before =>
            {
                self.gate_reopened = true;
                self.gate_closes_at_reopen = input_view.gate_closes;
                controls.set_mode(StreamMode::Game);
                self.mode_revision = view.mode_revision + 1;
                self.mode_at = Some(Instant::now());
                self.input_at_mode = driver.sent;
                driver.steps = GAME_STEPS;
                driver.next = 0;
                self.phase = 2;
            }
            2 | 3
                if ready
                    && view.mode_revision >= self.mode_revision
                    && self
                        .mode_at
                        .is_some_and(|at| at.elapsed() >= Duration::from_millis(600))
                    && driver.sent >= self.input_at_mode + 4 =>
            {
                if self.phase == 2 && view.mode == StreamMode::Game {
                    self.modes.push("game");
                    self.game_input_sent = driver.sent - self.input_at_mode;
                    controls.set_mode(StreamMode::Work);
                    self.mode_revision = view.mode_revision + 1;
                    self.mode_at = Some(Instant::now());
                    self.input_at_mode = driver.sent;
                    driver.steps = WORK_STEPS;
                    driver.next = 0;
                    self.phase = 3;
                } else if self.phase == 3 && view.mode == StreamMode::Work {
                    self.modes.push("work");
                    self.phase = 4;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "step6_e2e.rs"]
mod step6_e2e;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn packet_types(steps: &[Step]) -> BTreeSet<u8> {
        steps
            .iter()
            .filter_map(|step| step.packet(0))
            .map(|packet| packet[0])
            .collect()
    }

    #[test]
    fn scripts_cover_the_production_encoders_per_mode() {
        let work = packet_types(WORK_STEPS);
        let game = packet_types(GAME_STEPS);
        let every: BTreeSet<u8> = work.union(&game).copied().collect();
        assert_eq!(
            every,
            BTreeSet::from([
                protocol::TYPE_KEY_DOWN,
                protocol::TYPE_KEY_UP,
                protocol::TYPE_BUTTONS,
                protocol::TYPE_MOTION,
                protocol::TYPE_ABSOLUTE,
                protocol::TYPE_BUTTONS_AT,
                protocol::TYPE_CURSOR_SYNC,
                protocol::TYPE_SCAN_KEY,
            ])
        );
        // Relative motion belongs to game mode; absolute positioning to work mode.
        assert!(game.contains(&protocol::TYPE_MOTION) && !game.contains(&protocol::TYPE_ABSOLUTE));
        assert!(work.contains(&protocol::TYPE_ABSOLUTE) && !work.contains(&protocol::TYPE_MOTION));
        assert!(WORK_STEPS.contains(&Step::Release) && GAME_STEPS.contains(&Step::Release));
    }

    #[test]
    fn sequenced_packets_carry_the_allocated_sequence_and_others_do_not() {
        let absolute = Step::Absolute { x: 1, y: 2 }.packet(0x0102).unwrap();
        assert_eq!(
            absolute,
            vec![protocol::TYPE_ABSOLUTE, 0x02, 0x01, 1, 0, 2, 0]
        );
        assert_eq!(
            Step::Key { vk: VK_A, up: true }.packet(999).unwrap(),
            vec![protocol::TYPE_KEY_UP, 0x41, 0]
        );
        assert!(Step::Release.packet(0).is_none());
        assert!(Step::CursorSync.sequenced() && !Step::Buttons(1).sequenced());
    }

    #[test]
    fn input_driver_sends_only_after_decoded_readiness() {
        let link = InputLink::new(Arc::new(|| {}));
        let mut driver = InputDriver::new(InputScript::Work);
        let now = Instant::now();
        link.begin_epoch(7);
        driver.tick(&link, now);
        assert_eq!((driver.sent, driver.refused), (0, 0));
        link.set_ready(true);
        driver.tick(&link, now);
        assert_eq!((driver.sent, driver.refused), (1, 0));
        // Within one interval nothing else is due.
        driver.tick(&link, now);
        assert_eq!(driver.sent, 1);
    }

    #[test]
    fn relay_origin_is_loopback_websocket_only() {
        assert_eq!(
            relay_origin("ws://127.0.0.1:19764").unwrap(),
            "http://127.0.0.1:19764/"
        );
        assert!(relay_origin("wss://example.com").is_err());
        assert!(relay_origin("https://127.0.0.1").is_err());
        assert!(relay_origin("ws://192.168.1.2:19764").is_err());
    }

    #[test]
    fn interval_summary_uses_gaps_between_received_times() {
        let base = Instant::now();
        let times: VecDeque<Instant> = (0..5u64)
            .map(|i| base + Duration::from_millis(10 * i))
            .collect();
        let gaps = gaps_ms(&times);
        assert_eq!(gaps.len(), 4);
        let value = summary(&gaps);
        assert_eq!(value["samples"], 4);
        assert_eq!(value["p50_ms"].as_f64(), Some(10.0));
        assert_eq!(value["p99_ms"].as_f64(), Some(10.0));
    }

    #[test]
    fn pass_rules_keep_counter_only_and_count_only_distinct() {
        let good = Facts {
            errors: 0,
            received: 100,
            decoded: 90,
            decode_errors: 0,
            input_sent: None,
        };
        assert!(passes(Outcome::Decoded, &good));
        assert!(!passes(Outcome::Decoded, &Facts { decoded: 0, ..good }));
        assert!(!passes(Outcome::Decoded, &Facts { errors: 1, ..good }));
        assert!(!passes(
            Outcome::Decoded,
            &Facts {
                decode_errors: 1,
                ..good
            }
        ));
        assert!(!passes(
            Outcome::Decoded,
            &Facts {
                input_sent: Some(0),
                ..good
            }
        ));
        assert!(passes(
            Outcome::Decoded,
            &Facts {
                input_sent: Some(3),
                ..good
            }
        ));
        assert!(passes(Outcome::CountOnly, &Facts { decoded: 0, ..good }));
        assert!(!passes(
            Outcome::CountOnly,
            &Facts {
                received: 0,
                ..good
            }
        ));
        assert!(!passes(Outcome::CounterOnly, &good));
    }
}
