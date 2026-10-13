//! Host PC audio: WASAPI loopback of the default playback device, encoded as
//! 10 ms Opus frames. Capture runs on its own thread. Device and encoder
//! problems are logged and retried there, so video never waits on audio.

mod convert;
mod encoder;
mod gate;
mod wasapi;
mod wire;

pub use wire::{decode_packet, encode_packet};

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::activity::Activity;
use encoder::{FrameAssembler, OpusEncoder};
use gate::{frame_is_active, SilenceGate};
use wasapi::{ComApartment, Device, Read};
use wire::FRAME_SAMPLES;

/// Frames kept for the drain queue when no sink is set. Beyond this the oldest is dropped.
const PENDING_CAPACITY: usize = 10;
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

/// One encoded 10 ms packet, ready for `encode_packet`.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub seq: u32,
    pub capture_ts_us: u64,
    pub discontinuity: bool,
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
}

impl AudioCapture {
    /// `clock_start` is the origin of `capture_ts_us`, the same clock video uses.
    /// Only a failed thread spawn is returned; device and encoder failures stay in the thread.
    pub fn start(clock_start: Instant, activity: Activity) -> Result<Self> {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let sink = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::Builder::new()
            .name("audio-capture".into())
            .spawn({
                let pending = Arc::clone(&pending);
                let sink = Arc::clone(&sink);
                let stop = Arc::clone(&stop);
                move || run(clock_start, activity, pending, sink, stop)
            })
            .context("spawn audio capture thread")?;
        Ok(Self {
            pending,
            sink,
            stop,
            thread: Some(thread),
        })
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
            let _ = thread.join();
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
}

impl Pipeline {
    fn new(
        clock_start: Instant,
        activity: Activity,
        pending: Arc<Mutex<Pending>>,
        sink: Arc<Mutex<Option<AudioSink>>>,
    ) -> Self {
        Self {
            clock_start,
            activity,
            pending,
            sink,
            assembler: FrameAssembler::default(),
            gate: SilenceGate::new(),
            encoder: None,
            encoder_attempt: None,
            open_encoder: OpusEncoder::new,
            encoder_warn: WarnOnce::default(),
            seq: 0,
        }
    }

    /// The next packet starts a new run: buffered partial audio is dropped and the packet is marked.
    fn gap(&mut self) {
        self.gate.mark_gap();
        self.assembler.clear();
    }

    /// Feeds 48 kHz stereo; every completed 10 ms frame goes through the gate.
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
    pending: Arc<Mutex<Pending>>,
    sink: Arc<Mutex<Option<AudioSink>>>,
    stop: Arc<AtomicBool>,
) {
    // Declared first so it is dropped last, after the COM objects.
    let _com = ComApartment::enter();
    let mut pipe = Pipeline::new(clock_start, activity, pending, sink);
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
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn frame(seq: u32) -> AudioFrame {
        AudioFrame {
            seq,
            capture_ts_us: seq as u64,
            discontinuity: false,
            payload: vec![0xFC],
        }
    }

    fn pipeline() -> (Pipeline, Arc<Mutex<Pending>>) {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let sink = Arc::new(Mutex::new(None));
        let activity = Activity::new().expect("activity");
        let pipe = Pipeline::new(Instant::now(), activity, Arc::clone(&pending), sink);
        (pipe, pending)
    }

    fn take(pending: &Arc<Mutex<Pending>>) -> Vec<AudioFrame> {
        lock(pending).frames.drain(..).collect()
    }

    /// Pushes one 10 ms frame and returns what reached the drain queue.
    fn push_frame(
        pipe: &mut Pipeline,
        pending: &Arc<Mutex<Pending>>,
        pcm: &[[f32; 2]],
        now: Instant,
    ) -> Vec<AudioFrame> {
        pipe.push_stereo(pcm, now);
        take(pending)
    }

    fn tone() -> Vec<[f32; 2]> {
        (0..FRAME_SAMPLES as usize)
            .map(|i| {
                let v = (i as f32 * 0.1).sin() * 0.3;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn pending_buffer_drops_oldest_and_marks_the_new_front() {
        let mut pending = Pending::default();
        for seq in 0..(PENDING_CAPACITY as u32 + 3) {
            pending.push(frame(seq));
        }
        assert_eq!(pending.frames.len(), PENDING_CAPACITY);
        let front = pending.frames.front().unwrap();
        assert_eq!(front.seq, 3);
        assert!(front.discontinuity);
        assert!(pending.frames.iter().skip(1).all(|f| !f.discontinuity));
    }

    #[test]
    fn silence_is_gated_end_to_end_into_packets() {
        let (mut pipe, pending) = pipeline();
        let now = Instant::now();
        // One frame at a time: the drain queue holds 10, so a batch would overflow it.
        let mut sent = Vec::new();
        for _ in 0..40 {
            sent.extend(push_frame(&mut pipe, &pending, &SILENCE, now));
        }
        // The first 20 silent frames (200 ms) are sent, the rest suppressed.
        assert_eq!(sent.len(), 20);
        assert!(sent[0].discontinuity);
        assert!(sent[1..].iter().all(|f| !f.discontinuity));
        for (i, f) in sent.iter().enumerate() {
            assert_eq!(f.seq, i as u32, "seq counts sent packets only");
        }

        // Sound returns: the first packet after the suppressed span is marked.
        let resumed = push_frame(&mut pipe, &pending, &tone(), now);
        assert_eq!(resumed.len(), 1);
        assert!(resumed[0].discontinuity);
        assert_eq!(resumed[0].seq, 20);
    }

    #[test]
    fn sink_receives_every_packet_and_the_drain_queue_stays_empty() {
        let got: Arc<Mutex<Vec<AudioFrame>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_got = Arc::clone(&got);
        let sink: AudioSink = Arc::new(move |f: AudioFrame| {
            lock(&sink_got).push(f);
        });
        let pending = Arc::new(Mutex::new(Pending::default()));
        let mut pipe = Pipeline::new(
            Instant::now(),
            Activity::new().expect("activity"),
            Arc::clone(&pending),
            Arc::new(Mutex::new(Some(sink))),
        );
        let now = Instant::now();
        // 50 packets is well past the drain queue's capacity of 10.
        for _ in 0..50 {
            pipe.push_stereo(&tone(), now);
        }
        assert!(
            take(&pending).is_empty(),
            "fallback queue unused with a sink"
        );
        let received = lock(&got);
        assert_eq!(received.len(), 50, "sink gets every packet");
        for (i, f) in received.iter().enumerate() {
            assert_eq!(f.seq, i as u32);
        }
        assert!(received[0].discontinuity);
        assert!(received[1..].iter().all(|f| !f.discontinuity));
    }

    #[test]
    fn every_emitted_packet_is_a_valid_bpa1_packet_with_a_480_sample_opus_frame() {
        let (mut pipe, pending) = pipeline();
        let now = Instant::now();
        let sine: Vec<[f32; 2]> = (0..FRAME_SAMPLES as usize * 5)
            .map(|i| {
                let v = (i as f32 * 0.05).sin() * 0.5;
                [v, -v]
            })
            .collect();
        pipe.push_stereo(&sine, now);
        let sent = take(&pending);
        assert_eq!(sent.len(), 5);
        for f in &sent {
            let bytes = encode_packet(f).expect("encodable");
            let back = decode_packet(&bytes).expect("decodable");
            assert_eq!(back.payload, f.payload);
            let samples = opus::packet::get_nb_samples(&f.payload, 48_000).expect("opus packet");
            assert_eq!(samples, FRAME_SAMPLES as usize);
        }
    }

    #[test]
    fn gap_marks_the_next_packet_and_drops_partial_audio() {
        let (mut pipe, pending) = pipeline();
        let now = Instant::now();
        let loud = vec![[0.2f32, 0.2]; FRAME_SAMPLES as usize];
        pipe.push_stereo(&loud, now);
        pipe.push_stereo(&vec![[0.2f32, 0.2]; 100], now);
        pipe.gap();
        pipe.push_stereo(&vec![[0.2f32, 0.2]; FRAME_SAMPLES as usize], now);
        let sent = take(&pending);
        assert_eq!(
            sent.len(),
            2,
            "the 100 partial frames were dropped by the gap"
        );
        assert!(sent[0].discontinuity);
        assert!(sent[1].discontinuity);
    }

    #[test]
    fn capture_timestamps_follow_the_supplied_clock_origin() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let origin = Instant::now();
        let sink = Arc::new(Mutex::new(None));
        let mut pipe = Pipeline::new(origin, Activity::new().unwrap(), Arc::clone(&pending), sink);
        let later = origin + Duration::from_millis(250);
        pipe.push_stereo(&vec![[0.3f32, 0.3]; FRAME_SAMPLES as usize], later);
        let sent = take(&pending);
        assert_eq!(sent[0].capture_ts_us, 250_000);
    }

    static OPEN_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

    fn refusing_encoder() -> Result<OpusEncoder> {
        OPEN_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("encoder refused for the test")
    }

    #[test]
    fn failing_encoder_creation_is_retried_at_most_once_per_second() {
        let (mut pipe, pending) = pipeline();
        pipe.open_encoder = refusing_encoder;
        let now = Instant::now();
        // 100 active frames arrive well inside one second, so one attempt is expected.
        for _ in 0..100 {
            pipe.push_stereo(&tone(), now);
        }
        assert_eq!(OPEN_ATTEMPTS.load(Ordering::SeqCst), 1);
        assert!(take(&pending).is_empty(), "no packets without an encoder");
    }

    static QUIET_OPEN_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

    fn refusing_quiet_encoder() -> Result<OpusEncoder> {
        QUIET_OPEN_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("encoder refused during silence")
    }

    #[test]
    fn device_checks_retry_encoder_after_silence_suppresses_frames() {
        let (mut pipe, pending) = pipeline();
        pipe.open_encoder = refusing_quiet_encoder;
        let now = Instant::now();
        for _ in 0..30 {
            pipe.push_stereo(&SILENCE, now);
        }
        assert_eq!(QUIET_OPEN_ATTEMPTS.load(Ordering::SeqCst), 1);
        let attempted_at = pipe.encoder_attempt.unwrap();
        assert!(!pipe.ensure_encoder(attempted_at + Duration::from_millis(999)));
        assert_eq!(QUIET_OPEN_ATTEMPTS.load(Ordering::SeqCst), 1);
        assert!(!pipe.ensure_encoder(attempted_at + REOPEN_INTERVAL));
        assert_eq!(QUIET_OPEN_ATTEMPTS.load(Ordering::SeqCst), 2);
        assert!(take(&pending).is_empty());
    }

    #[test]
    fn filler_only_owes_silence_for_quiet_time_past_fill_after() {
        let t0 = Instant::now();
        let (mut pipe, _pending) = pipeline();
        let mut filler = Filler::new(t0);
        filler.fill(t0 + Duration::from_millis(40), &mut pipe);
        assert_eq!(
            filler.covered_until, t0,
            "under FILL_AFTER nothing is generated"
        );
        filler.fill(t0 + Duration::from_millis(60), &mut pipe);
        assert_eq!(filler.covered_until, t0 + Duration::from_millis(60));
    }

    #[test]
    fn filler_reset_leaves_no_catch_up_burst_after_a_gap() {
        let t0 = Instant::now();
        let (mut pipe, pending) = pipeline();
        let mut filler = Filler::new(t0);
        // No device for five seconds: the filler is reset every pass, not left behind.
        let resumed = t0 + Duration::from_secs(5);
        filler.reset(resumed);
        filler.fill(resumed, &mut pipe);
        assert_eq!(
            filler.covered_until, resumed,
            "nothing owed right after the reset"
        );
        assert!(take(&pending).is_empty());
    }

    #[test]
    fn filler_catch_up_is_capped_per_pass() {
        let t0 = Instant::now();
        let (mut pipe, _pending) = pipeline();
        let mut filler = Filler::new(t0);
        filler.fill(t0 + Duration::from_secs(5), &mut pipe);
        assert_eq!(
            filler.covered_until,
            t0 + FRAME_DURATION * MAX_FILL_FRAMES_PER_PASS as u32
        );
    }
}
