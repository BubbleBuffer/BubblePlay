//! A streaming session without a window of its own, for the Flutter app.
//!
//! One owner thread (MTA, so Media Foundation and COM work) creates the session's own D3D11
//! device, picks the hardware codec, and draws every decoded frame into a pair of shared
//! textures (see `target`). The network and decode threads are the ones the egui client uses.
//! The input capture lives on the caller's thread, the Flutter platform thread, because it
//! needs that window's messages.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use windows::{
    core::Interface,
    Win32::{
        Foundation::{HMODULE, HWND, LPARAM, LRESULT, WPARAM},
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
    control::{self, SessionHandles},
    decode::{self, DecodeCallbacks, FrameMeta},
    live_input::{self, LiveInput, LiveInputControl},
    stats::SessionStats,
    target::SharedPair,
};
use crate::{
    audio::{AudioPlayer, OutputMode},
    media::{Codec, DecodedFrame, MediaRuntime, MediaStats, NoHardwareVideoDecoder, VideoRenderer},
    model::{Action, AppState, Link, Quality, Session, Stats, StreamMode},
    net::{self, DecoderFeedback, InputLink, NetConfig, NetState, ViewerAuth},
    surface::{InputCapture, Rect},
};

/// What the caller supplies to start a session.
pub struct StreamParams {
    pub viewer: ViewerAuth,
    /// Relay origin of the PC, for example `https://relay.example`.
    pub relay: String,
    pub pc_id: String,
    pub pc_name: String,
    pub mode: StreamMode,
    /// The PC's saved default monitor selector, if any.
    pub default_monitor: Option<String>,
    /// Runs on an internal thread whenever a new picture is ready to show.
    pub on_frame: Arc<dyn Fn() + Send + Sync>,
    /// Runs on an internal thread whenever the session state may have changed.
    pub on_change: Arc<dyn Fn() + Send + Sync>,
}

/// The picture to show now: the shared handle and its size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Front {
    /// The DXGI shared handle value (`HANDLE`).
    pub handle: isize,
    pub width: u32,
    pub height: u32,
    /// Increases with every finished frame.
    pub serial: u64,
}

struct Shared {
    front: Mutex<Option<Front>>,
    errors: Mutex<Vec<String>>,
    serial: AtomicU64,
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct StreamSession {
    stop: Arc<AtomicBool>,
    handles: SessionHandles,
    stats: SessionStats,
    input_link: InputLink,
    shared: Arc<Shared>,
    capture: LiveInput,
    video_rect: Option<Rect>,
    threads: Vec<JoinHandle<Result<()>>>,
    on_change: Arc<dyn Fn() + Send + Sync>,
    /// The codec the session decodes.
    codec: Codec,
}

/// What the owner thread reports once it is up.
struct Ready {
    codec: Codec,
}

pub fn create_device() -> Result<ID3D11Device> {
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

fn pick_codec(device: &ID3D11Device, transport: net::TransportChoice) -> Result<Codec> {
    let available = |codec| -> Result<bool> {
        match crate::media::hardware_decoder_available(codec, device) {
            Err(error) if error.is::<NoHardwareVideoDecoder>() => Ok(false),
            other => other,
        }
    };
    if transport != net::TransportChoice::Wss && available(Codec::Hevc)? {
        return Ok(Codec::Hevc);
    }
    if available(Codec::H264)? {
        return Ok(Codec::H264);
    }
    Err(NoHardwareVideoDecoder {
        codec: Codec::H264,
        reason: "the session adapter has no usable D3D11/NV12 decoder",
    }
    .into())
}

impl StreamSession {
    /// Starts the session. `hwnd` is the window whose messages feed [`Self::on_message`].
    /// Call on the platform thread.
    pub fn start(params: StreamParams) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            front: Mutex::new(None),
            errors: Mutex::new(Vec::new()),
            serial: AtomicU64::new(0),
        });
        let media_stats = MediaStats::new();
        let transport = Arc::new(net::TransportStats::new());
        let on_change = params.on_change.clone();
        let input_link = InputLink::new(Arc::new({
            let on_change = on_change.clone();
            move || on_change()
        }));
        let audio_handle = Arc::new(Mutex::new(None));

        let mut config = NetConfig::new(params.relay.clone(), net::Codec::H264, params.viewer);
        config.input = Some(input_link.clone());
        config.stream_mode = Some(live_input::input_mode(params.mode).stream_mode().into());
        let controls = config.controls.clone();
        controls.initialize_mode(params.mode);
        controls.set_default_monitor(params.default_monitor.clone());
        controls.set_notifier(Arc::new({
            let on_change = on_change.clone();
            move || on_change()
        }));
        let transport_choice = config.transport;

        // The owner thread: device, codec, renderer, present loop, decode and audio.
        let (event_tx, event_rx) = mpsc::sync_channel(8);
        let (feedback_tx, feedback_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<Ready>>();
        let (audio_tx, audio_rx) = mpsc::channel();
        let owner = thread::Builder::new()
            .name("bubbleplay-stream".into())
            .spawn({
                let stop = stop.clone();
                let shared = shared.clone();
                let media_stats = media_stats.clone();
                let transport = transport.clone();
                let on_frame = params.on_frame.clone();
                let on_change = on_change.clone();
                let audio_slot = audio_handle.clone();
                move || {
                    let result = owner_thread(
                        &stop,
                        &shared,
                        &media_stats,
                        transport_choice,
                        event_rx,
                        feedback_tx,
                        on_frame,
                        on_change,
                        &ready_tx,
                        &audio_tx,
                        &audio_slot,
                    );
                    if let Err(error) = &result {
                        lock(&shared.errors).push(format!("stream: {error:#}"));
                        stop.store(true, Ordering::Release);
                        let _ = ready_tx.send(Err(anyhow::anyhow!("{error:#}")));
                    }
                    let _ = &transport;
                    result
                }
            })
            .context("spawn the stream owner thread")?;
        let ready = match ready_rx.recv() {
            Ok(Ok(ready)) => ready,
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = owner.join();
                return Err(error);
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                let result = owner.join();
                return Err(match result {
                    Ok(Err(error)) => error,
                    _ => anyhow::anyhow!("the stream owner thread ended before it was ready"),
                });
            }
        };
        let audio = audio_rx
            .recv()
            .context("audio handle from the owner thread")?;
        config.codec = match ready.codec {
            Codec::H264 => net::Codec::H264,
            Codec::Hevc => net::Codec::Hevc,
        };
        config.audio = Some(audio.clone());
        let stats = SessionStats {
            media: media_stats.clone(),
            transport: transport.clone(),
            audio,
        };

        // The input capture, on this thread.
        let (capture, input_control) = LiveInput::new(
            crate::input::Mode::from(params.mode),
            controls.clone(),
            live_input::session_input_factory(
                input_link.clone(),
                stop.clone(),
                Arc::new({
                    let on_change = on_change.clone();
                    move || on_change()
                }),
            ),
        );
        let handles = SessionHandles {
            controls: controls.clone(),
            input: input_control,
            pc_id: params.pc_id.clone(),
            pc_name: params.pc_name.clone(),
        };

        let net_thread = thread::Builder::new()
            .name("bubbleplay-net".into())
            .spawn({
                let stop = stop.clone();
                let shared = shared.clone();
                let on_change = on_change.clone();
                move || {
                    let result = net::run(config, event_tx, feedback_rx, stop.clone(), transport)
                        .map_err(anyhow::Error::new);
                    if let Err(error) = &result {
                        lock(&shared.errors).push(format!("network: {error:#}"));
                        stop.store(true, Ordering::Release);
                        on_change();
                    }
                    result
                }
            })
            .context("spawn the network thread")?;
        let _ = &audio_handle;
        Ok(Self {
            stop,
            handles,
            stats,
            input_link,
            shared,
            capture,
            video_rect: None,
            threads: vec![owner, net_thread],
            on_change,
            codec: ready.codec,
        })
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// True once the session has ended by itself (leave, error).
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// The last picture that is ready to show.
    pub fn front(&self) -> Option<Front> {
        *lock(&self.shared.front)
    }

    /// The one-second media window and totals (decode-to-present percentiles, fps, counters).
    pub fn stats(&self) -> crate::media::StatsSnapshot {
        self.stats.snapshot()
    }

    /// The most recent worker error, if the session ended on one.
    pub fn last_error(&self) -> Option<String> {
        lock(&self.shared.errors).last().cloned()
    }

    /// The model `Session` for the snapshot: the network's monitors and mode over the media
    /// and transport numbers.
    pub fn state(&self) -> Session {
        let mut state = AppState {
            session: Some(media_session(&self.stats, &self.input_link)),
            ..Default::default()
        };
        control::overlay_controls(&mut state, &self.handles);
        state.session.unwrap_or_default()
    }

    /// Applies a session action. Returns true when something changed.
    pub fn act(&self, action: Action) -> bool {
        control::act(&self.handles, &self.stop, action)
    }

    /// The input capture entry for the window's messages. Call on the platform thread only.
    pub fn on_message(
        &mut self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        self.capture.on_message(hwnd, msg, wparam, lparam)
    }

    /// Whether the capture holds the pointer and keyboard now.
    pub fn captured(&self) -> bool {
        self.capture.captured()
    }

    /// Where the video sits in the window's client area, in physical pixels. Call on the
    /// platform thread; the capture needs it for absolute pointer mapping and ClipCursor.
    pub fn set_video_rect(&mut self, hwnd: HWND, rect: Option<Rect>) {
        self.video_rect = rect;
        self.capture.on_frame(hwnd, rect);
    }

    /// Runs the capture's per-frame step with the last known rect (mode switches take effect
    /// here). Call on the platform thread, about once per frame.
    pub fn tick(&mut self, hwnd: HWND) {
        self.capture.on_frame(hwnd, self.video_rect);
    }

    pub fn input_control(&self) -> &LiveInputControl {
        &self.handles.input
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        (self.on_change)();
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Release a held pointer clip and cursor state before the threads go.
        self.capture.on_frame(HWND::default(), None);
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

fn media_session(stats: &SessionStats, input: &InputLink) -> Session {
    let sample = stats.snapshot();
    let net = stats.transport.snapshot();
    let rtt = input.snapshot().rtt_p50_ms.unwrap_or(0.0);
    Session {
        quality: Quality::Sharp,
        link: if net.state == NetState::Streaming {
            Link::Live
        } else {
            Link::Reconnecting {
                attempt: net.attempts as u32,
                secs: 0,
            }
        },
        rtt_ms: rtt.round() as u32,
        stats: Stats {
            resolution: format!("{} × {}", sample.width, sample.height),
            fps: sample.presented_fps as f32,
            rtt_ms: rtt.round() as u32,
            decode_ms: sample.recv_to_decode_p50_ms.unwrap_or(0.0) as f32,
            dropped: sample.totals.dropped as u32,
            transport: stats.transport.transport_name().into(),
            codec: sample.codec.map_or("", Codec::name).into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn owner_thread(
    stop: &Arc<AtomicBool>,
    shared: &Arc<Shared>,
    media_stats: &MediaStats,
    transport: net::TransportChoice,
    events: mpsc::Receiver<net::NetEvent>,
    feedback: mpsc::Sender<DecoderFeedback>,
    on_frame: Arc<dyn Fn() + Send + Sync>,
    on_change: Arc<dyn Fn() + Send + Sync>,
    ready: &mpsc::Sender<Result<Ready>>,
    audio_out: &mpsc::Sender<crate::audio::AudioHandle>,
    _audio_slot: &Arc<Mutex<Option<crate::audio::AudioHandle>>>,
) -> Result<()> {
    let _runtime = MediaRuntime::new()?;
    let device = create_device()?;
    let codec = pick_codec(&device, transport)?;
    let audio = AudioPlayer::new(OutputMode::Wasapi)?;
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    let wake = {
        let wake_tx = Mutex::new(wake_tx);
        Arc::new(move || {
            let _ = lock(&wake_tx).send(());
        })
    };
    let mut video = VideoRenderer::new(&device, media_stats.clone(), Some(wake.clone()))?;
    let metadata: Arc<Mutex<BTreeMap<i64, FrameMeta>>> = Arc::new(Mutex::new(BTreeMap::new()));
    video.set_presented_callback(Arc::new({
        let metadata = metadata.clone();
        let feedback = feedback.clone();
        move |pts, _at| {
            let meta = {
                let mut entries = lock(&metadata);
                let meta = entries.remove(&pts);
                entries.retain(|key, _| *key > pts);
                meta
            };
            if let Some(meta) = meta {
                let _ = feedback.send(DecoderFeedback::Presented {
                    local_epoch: meta.local_epoch,
                    generation: meta.generation,
                    seq: meta.seq,
                });
            }
        }
    }));
    let sink = video.sink();

    // The decode worker.
    let decode_thread = thread::Builder::new()
        .name("bubbleplay-decode".into())
        .spawn({
            let stop = stop.clone();
            let shared = shared.clone();
            let device = device.clone();
            let publish = {
                let metadata = metadata.clone();
                let sink = sink.clone();
                let stats = media_stats.clone();
                move |frame: DecodedFrame, meta: FrameMeta, decoded_at: Instant| {
                    stats.note_received(meta.pts_100ns, meta.received_at);
                    {
                        let mut entries = lock(&metadata);
                        entries.insert(meta.pts_100ns, meta);
                        while entries.len() > 64 {
                            entries.pop_first();
                        }
                    }
                    sink.push_timed(frame, decoded_at);
                }
            };
            let reset = {
                let metadata = metadata.clone();
                let sink = sink.clone();
                let wake = wake.clone();
                move |_epoch, _reason: &str, _at| {
                    lock(&metadata).clear();
                    sink.clear_pending();
                    wake();
                }
            };
            let error = {
                let stats = media_stats.clone();
                let shared = shared.clone();
                move |error: String| {
                    stats.note_decode_error();
                    let mut errors = lock(&shared.errors);
                    if errors.len() < 16 {
                        errors.push(error);
                    }
                }
            };
            let dropped = {
                let stats = media_stats.clone();
                move |count| {
                    for _ in 0..count {
                        stats.note_dropped(Instant::now());
                    }
                }
            };
            move || {
                let result = decode::run(
                    device,
                    events,
                    feedback,
                    stop.clone(),
                    DecodeCallbacks {
                        publish,
                        reset,
                        error,
                        dropped,
                    },
                );
                if let Err(error) = &result {
                    lock(&shared.errors).push(format!("decode worker: {error:#}"));
                    stop.store(true, Ordering::Release);
                }
                result
            }
        })
        .context("spawn the decode thread")?;

    let _ = audio_out.send(audio.handle());
    let _ = ready.send(Ok(Ready { codec }));

    // The present loop: draw each new picture into the back texture, flush, publish.
    let ctx = unsafe { device.GetImmediateContext()? };
    let mut pair: Option<SharedPair> = None;
    let mut presented_seen = 0u64;
    while !stop.load(Ordering::Acquire) {
        match wake_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while wake_rx.try_recv().is_ok() {}
        let sample = media_stats.snapshot(Instant::now());
        if sample.width == 0 || sample.height == 0 {
            continue;
        }
        let size = (sample.width, sample.height);
        if pair.as_ref().map(SharedPair::size) != Some(size) {
            pair = Some(SharedPair::new(&device, size.0, size.1)?);
            *lock(&shared.front) = None;
        }
        let Some(textures) = pair.as_mut() else {
            continue;
        };
        let back = textures.back();
        crate::surface::VideoLayer::draw(&mut video, &ctx, back.rtv(), back.viewport());
        let presented = media_stats.snapshot(Instant::now()).totals.presented;
        if presented == presented_seen {
            continue;
        }
        presented_seen = presented;
        textures.finish(&ctx);
        if let Some(front) = textures.front() {
            let serial = shared.serial.fetch_add(1, Ordering::AcqRel) + 1;
            *lock(&shared.front) = Some(Front {
                handle: front.handle().0 as isize,
                width: size.0,
                height: size.1,
                serial,
            });
            on_frame();
        }
        on_change();
    }
    stop.store(true, Ordering::Release);
    let result = decode_thread
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("decode thread panicked")));
    drop(audio);
    result
}

impl From<crate::model::StreamMode> for crate::input::Mode {
    fn from(mode: crate::model::StreamMode) -> Self {
        live_input::input_mode(mode)
    }
}

#[allow(unused)]
fn _assert_capture(_: &dyn InputCapture) {}
