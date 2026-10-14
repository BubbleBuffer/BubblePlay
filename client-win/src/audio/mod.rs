//! BPA1 audio is independent of video ACKs, input readiness and generation.
//! The bounded ingress never waits for the device. Epoch teardown synchronously
//! flushes software and device buffers; dropping the player joins its thread.
mod convert;
pub mod engine;
pub mod sink;
#[cfg(windows)]
mod wasapi;
pub mod wire;

use anyhow::{Context, Result};
use engine::{Engine, TimedPacket};
use serde::Serialize;
use sink::{AudioSink, FakeSink};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub use wire::{is_audio_packet, parse_packet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputMode {
    Wasapi,
    Fake,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AudioStats {
    pub received: u64,
    /// Last admitted BPA1 capture timestamp in the host's clock domain.
    /// Diagnostic only; it is not comparable with the local render clock.
    pub last_capture_ts_us: Option<u64>,
    pub rejected: u64,
    pub decoded: u64,
    pub concealed: u64,
    /// Valid packets discarded, including queued packets on resets/teardown.
    /// A decoded packet with a discarded unplayed tail also counts once;
    /// decoded and dropped therefore are not mutually exclusive counters.
    pub dropped: u64,
    pub late: u64,
    pub duplicates: u64,
    pub depth_ms: f64,
    pub depth_max_ms: f64,
    pub depth_p50_ms: Option<f64>,
    pub depth_p95_ms: Option<f64>,
    pub underruns: u64,
    /// Empty hardware padding after established playback. An observation,
    /// not acoustic proof of a glitch (a callback can coincide with empty).
    pub device_empty_observations: u64,
    pub decode_errors: u64,
    pub device_errors: u64,
    pub device_reopens: u64,
    pub device_open: bool,
    pub last_device_error: Option<String>,
    pub schedule_resets: u64,
    pub rendered_frames: u64,
    /// Cumulative post-volume PCM measurements, before the device mixer.
    pub output_rms: f64,
    pub output_peak: f32,
    /// Positive zero-crossing estimate over non-silent PCM. Meaningful for a
    /// single test tone, not a pitch detector for arbitrary host sound.
    pub output_frequency_hz: f64,
    /// Local receipt to scheduled sink write (including reported device
    /// padding). This is not host-capture-to-ear or acoustic latency.
    pub receipt_to_render_p50_ms: Option<f64>,
    pub receipt_to_render_p95_ms: Option<f64>,
    pub receipt_to_render_max_ms: Option<f64>,
    pub sink_buffer_ms: f64,
    pub sink_period_ms: f64,
    pub muted: bool,
    pub volume: f32,
    pub epoch: u64,
}

struct State {
    queue: VecDeque<TimedPacket>,
    epoch: u64,
    reset: u64,
    reset_done: u64,
    stop: bool,
    alive: bool,
    volume: f32,
    muted: bool,
    received: u64,
    rejected: u64,
    ingress_dropped: u64,
    stats: AudioStats,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    origin: Instant,
}

/// Cloneable transport/control handle. No COM objects or OS calls live here.
#[derive(Clone)]
pub struct AudioHandle {
    shared: Arc<Shared>,
}
impl std::fmt::Debug for AudioHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioHandle")
            .field("epoch", &lock(&self.shared.state).epoch)
            .finish_non_exhaustive()
    }
}
impl AudioHandle {
    /// Returns false for malformed packets, stale epochs or a stopped player.
    /// Full ingress drops oldest packets and marks the new front discontinuous.
    pub fn push(&self, epoch: u64, bytes: &[u8], received_at: Instant) -> bool {
        let packet = parse_packet(bytes);
        let mut state = lock(&self.shared.state);
        if epoch != state.epoch || state.stop || !state.alive {
            return false;
        }
        state.received += 1;
        let Some(packet) = packet else {
            state.rejected += 1;
            return false;
        };
        if state.queue.len() == 15 {
            state.queue.pop_front();
            state.ingress_dropped += 1;
            if let Some(front) = state.queue.front_mut() {
                front.packet.discontinuity = true;
            }
        }
        state.queue.push_back(TimedPacket {
            packet,
            received_us: received_at
                .saturating_duration_since(self.shared.origin)
                .as_micros() as u64,
        });
        self.shared.wake.notify_one();
        true
    }

    /// Invalidates old ingress immediately, then waits for the renderer to
    /// flush queued PCM. The render worker uses at most a 5 ms event wait.
    /// This is reserved for socket/session epochs, never video generation.
    pub fn reset_epoch(&self, epoch: u64) {
        let mut state = lock(&self.shared.state);
        state.epoch = epoch;
        state.ingress_dropped += state.queue.len() as u64;
        state.queue.clear();
        state.reset += 1;
        let request = state.reset;
        self.shared.wake.notify_all();
        while state.reset_done < request && state.alive && !state.stop {
            state = self
                .shared
                .wake
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
    pub fn set_volume(&self, volume: f32) {
        lock(&self.shared.state).volume = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.shared.wake.notify_one();
    }
    pub fn set_muted(&self, muted: bool) {
        lock(&self.shared.state).muted = muted;
        self.shared.wake.notify_one();
    }
    pub fn stats(&self) -> AudioStats {
        let state = lock(&self.shared.state);
        let mut stats = state.stats.clone();
        stats.received = state.received;
        stats.rejected = state.rejected;
        stats.dropped += state.ingress_dropped;
        stats.epoch = state.epoch;
        stats.muted = state.muted;
        stats.volume = state.volume;
        stats
    }
}

pub struct AudioPlayer {
    handle: AudioHandle,
    thread: Option<JoinHandle<()>>,
}
impl AudioPlayer {
    /// Validates libopus immediately. The output endpoint is opened lazily on
    /// the first valid packet, and failures retry at most once per second.
    pub fn new(mode: OutputMode) -> Result<Self> {
        let engine = Engine::new()?;
        let shared = Arc::new(Shared {
            origin: Instant::now(),
            wake: Condvar::new(),
            state: Mutex::new(State {
                queue: VecDeque::new(),
                epoch: 0,
                reset: 0,
                reset_done: 0,
                stop: false,
                alive: true,
                volume: 1.0,
                muted: false,
                received: 0,
                rejected: 0,
                ingress_dropped: 0,
                stats: AudioStats::default(),
            }),
        });
        let handle = AudioHandle {
            shared: Arc::clone(&shared),
        };
        let thread = thread::Builder::new()
            .name("audio-render".into())
            .spawn(move || run(shared, engine, mode))
            .context("spawn audio render thread")?;
        Ok(Self {
            handle,
            thread: Some(thread),
        })
    }
    pub fn handle(&self) -> AudioHandle {
        self.handle.clone()
    }
}
impl Drop for AudioPlayer {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.handle.shared.state);
            state.ingress_dropped += state.queue.len() as u64;
            state.queue.clear();
            state.stop = true;
        }
        self.handle.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ensures a reset waiter cannot remain blocked if a renderer unwinds.
struct Alive(Arc<Shared>);
impl Drop for Alive {
    fn drop(&mut self) {
        let mut state = lock(&self.0.state);
        state.alive = false;
        state.stats.device_open = false;
        self.0.wake.notify_all();
    }
}

fn run(shared: Arc<Shared>, mut engine: Engine, mode: OutputMode) {
    let _alive = Alive(Arc::clone(&shared));
    #[cfg(windows)]
    let _com = if mode == OutputMode::Wasapi {
        wasapi::ComApartment::enter().ok()
    } else {
        None
    };
    #[cfg(windows)]
    let _scheduling = if mode == OutputMode::Wasapi {
        wasapi::AudioScheduling::enter()
    } else {
        None
    };
    let mut sink: Option<Box<dyn AudioSink>> = None;
    let mut active = false;
    let mut next_open = Instant::now();
    let mut next_check = Instant::now();
    let mut next_publish = Instant::now();
    let mut device_errors = 0;
    let mut device_reopens = 0;
    let mut last_device_error = None;
    loop {
        let (packets, reset, volume, muted) = {
            let mut state = lock(&shared.state);
            if state.stop {
                break;
            }
            (
                state.queue.drain(..).collect::<Vec<_>>(),
                (state.reset > state.reset_done).then_some(state.reset),
                state.volume,
                state.muted,
            )
        };
        if let Some(request) = reset {
            engine.reset();
            if let Some(opened) = sink.as_mut() {
                if let Err(error) = engine.flush_sink(opened.as_mut()) {
                    last_device_error = Some(format!("{error:#}"));
                    sink = None;
                    device_errors += 1;
                }
            }
            active = false;
            let mut state = lock(&shared.state);
            state.stats = engine.stats();
            state.stats.device_errors = device_errors;
            state.stats.device_reopens = device_reopens;
            state.stats.device_open = sink.is_some();
            state.stats.last_device_error = last_device_error.clone();
            state.reset_done = request;
            shared.wake.notify_all();
        }
        let now = Instant::now();
        let now_us = now.saturating_duration_since(shared.origin).as_micros() as u64;
        for packet in packets {
            active = true;
            engine.push(packet, now_us);
        }
        if now >= next_check {
            next_check = now + Duration::from_secs(1);
            if sink.as_ref().is_some_and(|sink| sink.default_changed()) {
                sink = None;
                engine.reset();
                next_open = now;
            }
        }
        if active && sink.is_none() && now >= next_open {
            next_open = now + Duration::from_secs(1);
            match open_sink(mode) {
                Ok(opened) => {
                    sink = Some(opened);
                    device_reopens += 1;
                }
                Err(error) => {
                    last_device_error = Some(format!("{error:#}"));
                    device_errors += 1;
                }
            }
        }
        if let Some(opened) = sink.as_mut() {
            // Endpoint activation can take time. Schedule against the clock
            // after opening so a slow COM/device call cannot replay old audio
            // under an earlier timestamp or hide latency in diagnostics.
            let render_us = Instant::now()
                .saturating_duration_since(shared.origin)
                .as_micros() as u64;
            if let Err(error) = engine.render(opened.as_mut(), render_us, volume, muted) {
                last_device_error = Some(format!("{error:#}"));
                sink = None;
                engine.reset();
                device_errors += 1;
                next_open = now + Duration::from_secs(1);
            }
        }
        if now >= next_publish {
            next_publish = now + Duration::from_millis(50);
            let snapshot = engine.stats();
            let mut state = lock(&shared.state);
            state.stats = snapshot;
            state.stats.device_errors = device_errors;
            state.stats.device_reopens = device_reopens;
            state.stats.device_open = sink.is_some();
            state.stats.last_device_error = last_device_error.clone();
        }
        if let Some(opened) = sink.as_ref() {
            opened.wait(5);
        } else {
            let state = lock(&shared.state);
            if state.queue.is_empty() && !state.stop && state.reset == state.reset_done {
                drop(
                    shared
                        .wake
                        .wait_timeout(state, Duration::from_millis(5))
                        .unwrap_or_else(|e| e.into_inner()),
                );
            }
        }
    }
    // Publish the exact terminal counters after counting pending teardown
    // discards. No final PLC or extra render pass is manufactured.
    engine.reset();
    drop(sink);
    let snapshot = engine.stats();
    let mut state = lock(&shared.state);
    state.stats = snapshot;
    state.stats.device_errors = device_errors;
    state.stats.device_reopens = device_reopens;
    state.stats.last_device_error = last_device_error;
    state.stats.device_open = false;
}

fn open_sink(mode: OutputMode) -> Result<Box<dyn AudioSink>> {
    match mode {
        OutputMode::Fake => Ok(Box::new(FakeSink::default())),
        OutputMode::Wasapi => {
            #[cfg(windows)]
            {
                Ok(Box::new(wasapi::WasapiSink::open()?))
            }
            #[cfg(not(windows))]
            {
                anyhow::bail!("WASAPI requires Windows")
            }
        }
    }
}

#[cfg(test)]
mod tests;
