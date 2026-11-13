//! Host PC audio: WASAPI loopback of the default playback device, encoded as
//! 10 ms (WebRTC) or 20 ms (WSS) Opus frames. Capture runs on its own thread. Device and encoder
//! problems are logged and retried there, so video never waits on audio.

mod convert;
mod encoder;
mod gate;
mod wasapi;
mod wire;

pub use wire::{decode_packet, encode_packet, FRAME_SAMPLES, FRAME_SAMPLES_20MS};

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::activity::Activity;
use crate::capture_wait::bounded_join::join_bounded;
use encoder::{FrameAssembler, OpusEncoder};
use gate::{frame_is_active, SilenceGate};
use wasapi::{ComApartment, Device, Read};

/// Frames kept for the drain queue when no sink is set. Beyond this the oldest is dropped.
const PENDING_CAPACITY: usize = 10;
/// How long dropping the capture waits for its thread before detaching it.
const AUDIO_STOP_TIMEOUT_MS: u64 = 2_000;
/// How often the default endpoint is compared with the open one.
const CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// Device reopens and Opus creation attempts are at most this far apart, whatever the failure.
const REOPEN_INTERVAL: Duration = Duration::from_secs(1);
/// Longest wait for the engine's event before the loop looks again.
const WAIT_SLICE_MS: u32 = 10;
const FRAME_DURATION: Duration = Duration::from_millis(10);
/// On an open device with no data for this long, silence is generated in real time
/// so the silence gate sees the passage of time.
const FILL_AFTER: Duration = Duration::from_millis(50);
/// Most silent frames generated in one pass (1 s), so a stalled thread catches up gradually.
const MAX_FILL_FRAMES_PER_PASS: usize = 100;
const SILENCE: [[f32; 2]; FRAME_SAMPLES as usize] = [[0.0; 2]; FRAME_SAMPLES as usize];

/// One encoded packet, ready for `encode_packet`.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub seq: u32,
    pub capture_ts_us: u64,
    pub discontinuity: bool,
    /// Samples per channel: `FRAME_SAMPLES` (10 ms) or `FRAME_SAMPLES_20MS`.
    pub samples: u16,
    pub payload: Vec<u8>,
}

/// Receives each packet on the capture thread. It must not block.
pub type AudioSink = Arc<dyn Fn(AudioFrame) + Send + Sync>;

/// Owns the capture thread. Dropping it stops the thread and waits for it.
pub struct AudioCapture {
    pending: Arc<Mutex<Pending>>,
    sink: Arc<Mutex<Option<AudioSink>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    frame_samples: u16,
}

impl AudioCapture {
    /// `clock_start` is the origin of `capture_ts_us`, the same clock video uses.
    /// Only a failed thread spawn is returned; device and encoder failures stay in the thread.
    /// `frame_samples` is the packet size: `FRAME_SAMPLES` (10 ms) or `FRAME_SAMPLES_20MS`.
    pub fn start(clock_start: Instant, activity: Activity, frame_samples: u16) -> Result<Self> {
        anyhow::ensure!(
            wire::is_frame_samples(frame_samples),
            "unsupported audio frame size {frame_samples}"
        );
        let pending = Arc::new(Mutex::new(Pending::default()));
        let sink = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::Builder::new()
            .name("audio-capture".into())
            .spawn({
                let pending = Arc::clone(&pending);
                let sink = Arc::clone(&sink);
                let stop = Arc::clone(&stop);
                move || run(clock_start, activity, frame_samples, pending, sink, stop)
            })
            .context("spawn audio capture thread")?;
        Ok(Self {
            pending,
            sink,
            stop,
            thread: Some(thread),
            frame_samples,
        })
    }

    /// The packet size this capture produces.
    pub fn frame_samples(&self) -> u16 {
        self.frame_samples
    }

    /// Hands each new packet to `sink` on the capture thread, in place of the drain queue.
    /// The callback must not block: have it push into a bounded queue that the transport drains.
    /// `None` returns to the drain queue. Packets already queued stay there until drained.
    pub fn set_sink(&self, sink: Option<AudioSink>) {
        *lock(&self.sink) = sink;
    }

    /// Takes the frames captured since the last call, oldest first. Empty while a sink is set.
    pub fn drain(&self) -> Vec<AudioFrame> {
        lock(&self.pending).frames.drain(..).collect()
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // The thread may be inside a WASAPI call that never returns.
            let joined = join_bounded(
                thread,
                Duration::from_millis(AUDIO_STOP_TIMEOUT_MS),
                "audio-capture",
            );
            if joined.is_some_and(|result| result.is_err()) {
                warn!("audio capture thread panicked");
            }
        }
    }
}

#[derive(Default)]
struct Pending {
    frames: VecDeque<AudioFrame>,
}

impl Pending {
    fn push(&mut self, frame: AudioFrame) {
        if self.frames.len() == PENDING_CAPACITY {
            self.frames.pop_front();
            // The consumer missed frames before the new front. Mark it so the receiver resets.
            if let Some(front) = self.frames.front_mut() {
                front.discontinuity = true;
            }
        }
        self.frames.push_back(frame);
    }
}

fn lock<T>(shared: &Mutex<T>) -> MutexGuard<'_, T> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Logs a recurring failure once, then stays quiet until it clears.
#[derive(Default)]
struct WarnOnce(bool);

impl WarnOnce {
    fn warn(&mut self, what: &str, err: &anyhow::Error) {
        if !self.0 {
            let reason = format!("{err:#}");
            warn!(error = %reason, "{what}; will keep retrying");
            self.0 = true;
        }
    }

    fn clear(&mut self) {
        self.0 = false;
    }
}

/// Everything after the device: 48 kHz stereo in, gated Opus packets out.
struct Pipeline {
    clock_start: Instant,
    activity: Activity,
    pending: Arc<Mutex<Pending>>,
    sink: Arc<Mutex<Option<AudioSink>>>,
    assembler: FrameAssembler,
    gate: SilenceGate,
    encoder: Option<OpusEncoder>,
    /// When the encoder was last created, so a failing creation is tried once per REOPEN_INTERVAL.
    encoder_attempt: Option<Instant>,
    open_encoder: fn() -> Result<OpusEncoder>,
    encoder_warn: WarnOnce,
    seq: u32,
    /// Samples per channel in each packet.
    frame_samples: u16,
}

impl Pipeline {
    fn new(
        clock_start: Instant,
        activity: Activity,
        pending: Arc<Mutex<Pending>>,
        sink: Arc<Mutex<Option<AudioSink>>>,
        frame_samples: u16,
    ) -> Self {
        Self {
            clock_start,
            activity,
            pending,
            sink,
            assembler: FrameAssembler::new(frame_samples),
            // Silence is sent for about 200 ms whatever the packet size.
            gate: SilenceGate::with_limit(
                gate::MAX_SILENT_FRAMES * u32::from(FRAME_SAMPLES) / u32::from(frame_samples),
            ),
            encoder: None,
            encoder_attempt: None,
            open_encoder: OpusEncoder::new,
            encoder_warn: WarnOnce::default(),
            seq: 0,
            frame_samples,
        }
    }

    /// The next packet starts a new run: buffered partial audio is dropped and the packet is marked.
    fn gap(&mut self) {
        self.gate.mark_gap();
        self.assembler.clear();
    }

    /// Feeds 48 kHz stereo; every completed frame goes through the gate.
    /// `now` stamps the frames this call completes.
    fn push_stereo(&mut self, frames: &[[f32; 2]], now: Instant) {
        self.assembler.push(frames);
        while let Some(pcm) = self.assembler.pop() {
            self.emit(&pcm, now);
        }
    }

    fn emit(&mut self, pcm: &[f32], now: Instant) {
        let Some(discontinuity) = self.gate.on_frame(frame_is_active(pcm)) else {
            return;
        };
        let Some(payload) = self.encode(pcm) else {
            // The frame is lost, so the next one must announce the gap.
            self.gate.mark_gap();
            return;
        };
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        let capture_ts_us = now.saturating_duration_since(self.clock_start).as_micros() as u64;
        let frame = AudioFrame {
            seq,
            capture_ts_us,
            discontinuity,
            samples: self.frame_samples,
            payload,
        };
        // Copy the sink out so the lock is released before the callback runs.
        let sink = lock(&self.sink).clone();
        match sink {
            Some(sink) => sink(frame),
            None => {
                lock(&self.pending).push(frame);
                self.activity.notify();
            }
        }
    }

    fn encode(&mut self, pcm: &[f32]) -> Option<Vec<u8>> {
        if !self.ensure_encoder(Instant::now()) {
            return None;
        }
        let encoder = self.encoder.as_mut()?;
        match encoder.encode(pcm) {
            Ok(payload) => {
                self.encoder_warn.clear();
                Some(payload)
            }
            Err(err) => {
                self.encoder_warn.warn("opus encode failed", &err);
                self.encoder = None;
                None
            }
        }
    }

    /// Device checks retry initialization even while silence suppresses frames.
    fn ensure_encoder(&mut self, now: Instant) -> bool {
        if self.encoder.is_none() {
            if self
                .encoder_attempt
                .is_some_and(|at| now.duration_since(at) < REOPEN_INTERVAL)
            {
                return false;
            }
            self.encoder_attempt = Some(now);
            match (self.open_encoder)() {
                Ok(encoder) => {
                    self.encoder = Some(encoder);
                    self.encoder_warn.clear();
                }
                Err(err) => {
                    self.encoder_warn.warn("opus encoder unavailable", &err);
                    return false;
                }
            }
        }
        true
    }
}

/// Stands in for device data while an open device stays quiet, so the silence
/// gate sees time pass. Owes no silence for time when no device was open.
struct Filler {
    covered_until: Instant,
}

impl Filler {
    fn new(now: Instant) -> Self {
        Self { covered_until: now }
    }

    /// Marks everything up to `now` as covered. Called whenever data arrives or no
    /// device is open, so a reopened device never triggers a catch-up burst.
    fn reset(&mut self, now: Instant) {
        self.covered_until = now;
    }

    fn fill(&mut self, now: Instant, pipe: &mut Pipeline) {
        if now.duration_since(self.covered_until) < FILL_AFTER {
            return;
        }
        let mut passes = 0;
        while passes < MAX_FILL_FRAMES_PER_PASS
            && now.duration_since(self.covered_until) >= FRAME_DURATION
        {
            self.covered_until += FRAME_DURATION;
            pipe.push_stereo(&SILENCE, self.covered_until);
            passes += 1;
        }
    }
}

/// The capture loop. Every failure leads to a retry, never to thread exit,
/// except when `stop` is set.
fn run(
    clock_start: Instant,
    activity: Activity,
    frame_samples: u16,
    pending: Arc<Mutex<Pending>>,
    sink: Arc<Mutex<Option<AudioSink>>>,
    stop: Arc<AtomicBool>,
) {
    // Declared first so it is dropped last, after the COM objects.
    let _com = ComApartment::enter();
    let mut pipe = Pipeline::new(clock_start, activity, pending, sink, frame_samples);
    let mut device: Option<Device> = None;
    let mut device_warn = WarnOnce::default();
    let mut last_attempt: Option<Instant> = None;
    let mut next_check = Instant::now();
    let mut filler = Filler::new(Instant::now());
    let mut stereo: Vec<[f32; 2]> = Vec::new();

    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();

        let check_due = now >= next_check;
        if check_due {
            next_check = now + CHECK_INTERVAL;
            if device.as_ref().is_some_and(Device::default_changed) {
                info!("default playback device changed; reopening loopback");
                device = None;
                pipe.gap();
            }
        }

        if device.is_none()
            && last_attempt.is_none_or(|at| now.duration_since(at) >= REOPEN_INTERVAL)
        {
            last_attempt = Some(now);
            match Device::open() {
                Ok(opened) => {
                    let format = opened.format();
                    info!(
                        channels = format.channels,
                        sample_rate = format.sample_rate,
                        kind = ?format.kind,
                        "loopback capture started"
                    );
                    device = Some(opened);
                    device_warn.clear();
                    pipe.gap();
                }
                Err(err) => device_warn.warn("loopback capture unavailable", &err),
            }
        }

        if check_due && device.is_some() {
            pipe.ensure_encoder(now);
        }
        let outcome = device.as_mut().map(|open| {
            open.wait(WAIT_SLICE_MS);
            stereo.clear();
            open.read(&mut stereo)
        });
        let mut heard = false;
        match outcome {
            Some(Ok(Read {
                got_data,
                discontinuity,
            })) => {
                heard = got_data;
                if discontinuity {
                    pipe.gap();
                }
                device_warn.clear();
                pipe.push_stereo(&stereo, Instant::now());
            }
            Some(Err(err)) => {
                let err = anyhow::Error::from(err);
                device_warn.warn("loopback read failed; reopening", &err);
                device = None;
                pipe.gap();
            }
            None => thread::sleep(Duration::from_millis(WAIT_SLICE_MS as u64)),
        }

        // Silence is synthesised only while a device is open and quiet. With no
        // device, or after data, the filler's mark moves to now instead.
        let now = Instant::now();
        if heard || device.is_none() {
            filler.reset(now);
        } else {
            filler.fill(now, &mut pipe);
        }
    }
}

#[cfg(test)]
mod tests;
