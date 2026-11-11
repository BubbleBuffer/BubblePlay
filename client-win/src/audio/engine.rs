//! Bounded packet jitter and Opus playout, independent of the OS device.
use super::{
    sink::AudioSink,
    wire::{Packet, FRAME_SAMPLES, MAX_FRAME_SAMPLES, SAMPLE_RATE},
    AudioStats,
};
use anyhow::{Context, Result};
use std::collections::VecDeque;

const LEAD_US: u64 = 40_000;
const MAX_DEPTH_US: u64 = 150_000;
const RECOVER_PACKETS: usize = 6;
const MAX_PLC: u8 = 3;
const MAX_SAMPLES: usize = 2048;

#[derive(Clone)]
pub struct TimedPacket {
    pub packet: Packet,
    pub received_us: u64,
}

/// Deterministic engine. `now_us` is a local monotonic clock, never host time.
/// Packet timestamps are diagnostic only: the host clock cannot measure local
/// one-way latency. Sequence comparisons use wrapping u32 half-range ordering.
pub struct Engine {
    decoder: opus::Decoder,
    pending: Vec<TimedPacket>,
    expected: Option<u32>,
    starts_us: u64,
    started: bool,
    pcm: VecDeque<f32>,
    pcm_received_us: Option<u64>,
    missing: u8,
    clear_sink: bool,
    stats: AudioStats,
    depths: VecDeque<f64>,
    latencies: VecDeque<f64>,
    energy: f64,
    energy_samples: u64,
    crossings: u64,
    tone_frames: u64,
    previous: f32,
    padding_us: u64,
    padding_at_us: u64,
    has_written: bool,
    /// Samples per channel of the last packet seen; sizes concealment.
    frame_samples: usize,
}

impl Engine {
    pub fn new() -> Result<Self> {
        Ok(Self {
            decoder: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)
                .context("create Opus audio decoder")?,
            pending: Vec::new(),
            expected: None,
            starts_us: 0,
            started: false,
            pcm: VecDeque::new(),
            pcm_received_us: None,
            missing: 0,
            clear_sink: false,
            stats: AudioStats::default(),
            depths: VecDeque::new(),
            latencies: VecDeque::new(),
            energy: 0.0,
            energy_samples: 0,
            crossings: 0,
            tone_frames: 0,
            previous: 0.0,
            padding_us: 0,
            padding_at_us: 0,
            has_written: false,
            frame_samples: FRAME_SAMPLES,
        })
    }

    /// Drops all decode and device state. Counters remain cumulative.
    pub fn reset(&mut self) {
        self.stats.dropped += self.pending.len() as u64
            + u64::from(!self.pcm.is_empty() && self.pcm_received_us.is_some());
        self.pending.clear();
        self.pcm.clear();
        self.expected = None;
        self.started = false;
        self.starts_us = 0;
        self.missing = 0;
        self.pcm_received_us = None;
        self.padding_us = 0;
        self.has_written = false;
        let _ = self.decoder.reset_state();
        self.clear_sink = true;
        self.stats.depth_ms = 0.0;
        self.stats.schedule_resets += 1;
    }

    pub fn push(&mut self, timed: TimedPacket, now_us: u64) {
        let seq = timed.packet.seq;
        if self.pending.iter().any(|p| p.packet.seq == seq) {
            self.stats.duplicates += 1;
            self.stats.dropped += 1;
            return;
        }
        if let Some(expected) = self.expected {
            let delta = seq.wrapping_sub(expected) as i32;
            // Before playback, allow modest reordering ahead of the first
            // deadline. A flagged start remains the earliest admissible packet.
            if delta < 0 {
                if !self.started
                    && delta >= -4
                    && !timed.packet.discontinuity
                    && !self.pending.iter().any(|p| p.packet.discontinuity)
                {
                    self.expected = Some(seq);
                } else {
                    self.stats.late += 1;
                    self.stats.dropped += 1;
                    return;
                }
            }
            if delta > 100 || timed.packet.discontinuity {
                self.reset();
            }
        }
        if self.expected.is_none() {
            self.expected = Some(seq);
            self.starts_us = timed.received_us.saturating_add(LEAD_US);
        }
        self.stats.last_capture_ts_us = Some(timed.packet.capture_ts_us);
        self.pending.push(timed);
        let expected = self.expected.unwrap_or(seq);
        self.pending
            .sort_by_key(|p| p.packet.seq.wrapping_sub(expected));
        self.trim_overgrowth(now_us);
        self.measure_depth(now_us);
    }

    fn trim_overgrowth(&mut self, now_us: u64) {
        let depth = self.depth_us(now_us);
        if depth > MAX_DEPTH_US || self.pending.len() > 15 {
            // Retain newest 60 ms and immediately abandon the old schedule.
            // This also bounds sustained faster-than-clock input and bursts.
            let Some(newest) = self.pending.last().map(|p| p.packet.seq) else {
                self.stats.dropped +=
                    u64::from(!self.pcm.is_empty() && self.pcm_received_us.is_some());
                self.pcm.clear();
                self.pcm_received_us = None;
                self.expected = None;
                self.started = false;
                self.starts_us = 0;
                self.padding_us = 0;
                self.clear_sink = true;
                let _ = self.decoder.reset_state();
                self.stats.schedule_resets += 1;
                return;
            };
            let before = self.pending.len();
            // Keep the newest 60 ms whatever the packet duration.
            let recover = self.pending.last().map_or(RECOVER_PACKETS, |p| {
                (RECOVER_PACKETS * FRAME_SAMPLES / p.packet.frame_samples).max(1)
            }) as u32;
            self.pending
                .retain(|p| newest.wrapping_sub(p.packet.seq) < recover);
            self.stats.dropped += (before - self.pending.len()) as u64;
            self.stats.dropped += u64::from(!self.pcm.is_empty() && self.pcm_received_us.is_some());
            self.pcm.clear();
            self.pcm_received_us = None;
            self.expected = self.pending.first().map(|p| p.packet.seq);
            self.starts_us = now_us;
            self.started = true;
            self.missing = 0;
            let _ = self.decoder.reset_state();
            self.clear_sink = true;
            self.padding_us = 0;
            self.stats.schedule_resets += 1;
        }
    }

    fn depth_us(&self, now_us: u64) -> u64 {
        let lead = if self.started {
            0
        } else {
            self.starts_us.saturating_sub(now_us)
        };
        let packets = match (self.expected, self.pending.last()) {
            (Some(seq), Some(last)) => {
                (last.packet.seq.wrapping_sub(seq) as u64 + 1)
                    * (last.packet.frame_samples as u64 * 1_000_000 / SAMPLE_RATE as u64)
            }
            _ => 0,
        };
        let padding = self
            .padding_us
            .saturating_sub(now_us.saturating_sub(self.padding_at_us));
        let ahead = if self.expected.is_none() && self.pcm.is_empty() {
            0
        } else {
            lead.max(padding)
        };
        ahead + packets + self.pcm.len() as u64 * 1_000_000 / (SAMPLE_RATE as u64 * 2)
    }

    fn measure_depth(&mut self, now_us: u64) {
        self.stats.depth_ms = self.depth_us(now_us) as f64 / 1000.0;
        self.stats.depth_max_ms = self.stats.depth_max_ms.max(self.stats.depth_ms);
        sample(&mut self.depths, self.stats.depth_ms);
    }

    fn decode_next(&mut self, at_us: u64) {
        let Some(seq) = self.expected else {
            return;
        };
        // Sized for the longest accepted packet (20 ms); trimmed to the
        // packet's own length before it joins the playout queue.
        let mut decoded = vec![0f32; MAX_FRAME_SAMPLES * 2];
        let packet = if self.pending.first().is_some_and(|p| p.packet.seq == seq) {
            Some(self.pending.remove(0))
        } else {
            None
        };
        let frames = packet
            .as_ref()
            .map_or(self.frame_samples, |p| p.packet.frame_samples);
        self.frame_samples = frames;
        decoded.truncate(frames * 2);
        if let Some(timed) = packet {
            if at_us.saturating_sub(timed.received_us) > MAX_DEPTH_US {
                self.stats.dropped += 1;
                self.pcm_received_us = None;
                let _ = self.decoder.reset_state();
                self.conceal(&mut decoded);
                self.expected = Some(seq.wrapping_add(1));
                self.pcm.extend(decoded);
                return;
            }
            self.pcm_received_us = Some(timed.received_us);
            self.missing = 0;
            // The Opus payload must last exactly what its BPA1 header says, so
            // a header cannot shift the timeline or overrun the decode scratch.
            let duration_ok = opus::packet::get_nb_samples(&timed.packet.payload, SAMPLE_RATE)
                .is_ok_and(|n| n == frames);
            let result = if duration_ok {
                self.decoder
                    .decode_float(&timed.packet.payload, &mut decoded, false)
                    .ok()
            } else {
                None
            };
            if result == Some(frames) {
                self.stats.decoded += 1;
            } else {
                self.stats.decode_errors += 1;
                self.stats.dropped += 1;
                decoded.fill(0.0);
                self.conceal(&mut decoded);
            }
        } else {
            self.pcm_received_us = None;
            self.stats.underruns += 1;
            self.missing += 1;
            // At most 30 ms of concealment, whatever the packet duration.
            if usize::from(self.missing) * frames <= usize::from(MAX_PLC) * FRAME_SAMPLES {
                self.conceal(&mut decoded);
            } else {
                // Host deliberately stops sending during silence. Do not
                // invent unbounded PLC, nor advance into a future sound run.
                self.expected = self.pending.first().map(|p| p.packet.seq);
                self.starts_us = at_us + LEAD_US;
                self.started = false;
                self.missing = 0;
                self.stats.schedule_resets += 1;
                let _ = self.decoder.reset_state();
                return;
            }
        }
        self.expected = Some(seq.wrapping_add(1));
        self.pcm.extend(decoded);
    }

    fn conceal(&mut self, pcm: &mut [f32]) {
        // Host has in-band FEC disabled. Empty decode requests libopus PLC;
        // never pass the subsequent packet with decode_fec=true.
        if self.decoder.decode_float(&[], pcm, false).ok() != Some(pcm.len() / 2) {
            pcm.fill(0.0);
        }
        self.stats.concealed += 1;
    }

    /// Render one sink opportunity. Device errors propagate to the owner,
    /// which drops the endpoint, clears this engine and retries quietly.
    pub fn render<S: AudioSink + ?Sized>(
        &mut self,
        sink: &mut S,
        now_us: u64,
        volume: f32,
        muted: bool,
    ) -> Result<()> {
        if !self.pcm.is_empty()
            && self
                .pcm_received_us
                .is_some_and(|at| now_us.saturating_sub(at) > MAX_DEPTH_US)
        {
            self.pcm.clear();
            self.pcm_received_us = None;
            self.stats.dropped += 1;
            self.clear_sink = true;
            self.padding_us = 0;
            let _ = self.decoder.reset_state();
            self.stats.schedule_resets += 1;
        }
        self.flush_sink(sink)?;
        let (mut frames, mut padding_ms) = sink.writable(now_us)?;
        self.padding_us = (padding_ms * 1000.0) as u64;
        self.padding_at_us = now_us;
        if sink.observes_device_padding() && self.has_written && self.started && padding_ms == 0.0 {
            self.stats.device_empty_observations += 1;
        }
        self.trim_overgrowth(now_us);
        if self.clear_sink {
            self.flush_sink(sink)?;
            (frames, padding_ms) = sink.writable(now_us)?;
        }
        self.stats.sink_buffer_ms = sink.buffer_ms();
        self.stats.sink_period_ms = sink.period_ms();
        if frames == 0 {
            self.measure_depth(now_us);
            return Ok(());
        }
        // Keep a failed/malicious sink from causing an unbounded allocation.
        anyhow::ensure!(
            frames <= SAMPLE_RATE as usize,
            "audio sink requested over 1 s"
        );
        let mut output = vec![0.0f32; frames * 2];
        let mut source_packets = Vec::new();
        let mut discarded_tails = Vec::new();
        let mut scheduled_latencies = Vec::new();
        let mut max_latency = None::<f64>;
        let gain = if muted { 0.0 } else { volume.clamp(0.0, 1.0) };
        for index in 0..frames {
            let at = now_us
                + (padding_ms * 1000.0) as u64
                + index as u64 * 1_000_000 / SAMPLE_RATE as u64;
            if self.expected.is_some() && at >= self.starts_us {
                self.started = true;
                // A packet may span two device callbacks. Recheck every
                // projected output sample, including hardware padding and
                // this write's frame offset, rather than only decode time.
                if !self.pcm.is_empty()
                    && self
                        .pcm_received_us
                        .is_some_and(|received| at.saturating_sub(received) > MAX_DEPTH_US)
                {
                    if let Some(seq) = self.expected {
                        discarded_tails.push(seq.wrapping_sub(1));
                    }
                    self.pcm.clear();
                    self.pcm_received_us = None;
                    self.stats.dropped += 1;
                    let _ = self.decoder.reset_state();
                    self.stats.schedule_resets += 1;
                }
                if self.pcm.is_empty() {
                    self.decode_next(at);
                    if let Some(received) = self.pcm_received_us {
                        let latency = at.saturating_sub(received) as f64 / 1000.0;
                        scheduled_latencies.push(latency);
                    }
                }
                if let Some(received) = self.pcm_received_us {
                    if !self.pcm.is_empty() {
                        if let Some(seq) = self.expected {
                            let seq = seq.wrapping_sub(1);
                            if source_packets.last() != Some(&seq) {
                                source_packets.push(seq);
                            }
                        }
                    }
                    let latency = at.saturating_sub(received) as f64 / 1000.0;
                    max_latency = Some(max_latency.unwrap_or(0.0).max(latency));
                }
                for channel in 0..2 {
                    output[index * 2 + channel] = self.pcm.pop_front().unwrap_or(0.0) * gain;
                }
            }
        }
        if let Err(error) = sink.write(&output) {
            // The output was consumed from software but never committed to the
            // device. Count each affected source packet once, including an
            // older partial packet, without re-counting an aged tail above.
            self.stats.dropped += source_packets
                .iter()
                .filter(|seq| !discarded_tails.contains(seq))
                .count() as u64;
            if self.pcm_received_us.is_some() {
                self.pcm.clear();
                self.pcm_received_us = None;
            }
            return Err(error);
        }
        for latency in scheduled_latencies {
            sample(&mut self.latencies, latency);
        }
        if let Some(latency) = max_latency {
            self.stats.receipt_to_render_max_ms = Some(
                self.stats
                    .receipt_to_render_max_ms
                    .unwrap_or(0.0)
                    .max(latency),
            );
        }
        self.has_written = true;
        self.padding_us =
            (padding_ms * 1000.0) as u64 + frames as u64 * 1_000_000 / SAMPLE_RATE as u64;
        self.padding_at_us = now_us;
        self.stats.rendered_frames += frames as u64;
        for frame in output.chunks_exact(2) {
            let value = frame[0];
            self.energy += (value as f64).powi(2) + (frame[1] as f64).powi(2);
            self.energy_samples += 2;
            self.stats.output_peak = self.stats.output_peak.max(value.abs()).max(frame[1].abs());
            if value.abs() > 0.00001 || self.previous.abs() > 0.00001 {
                self.tone_frames += 1;
                if self.previous <= 0.0 && value > 0.0 {
                    self.crossings += 1;
                }
            }
            self.previous = value;
        }
        self.measure_depth(now_us);
        Ok(())
    }

    pub fn flush_sink<S: AudioSink + ?Sized>(&mut self, sink: &mut S) -> Result<()> {
        if self.clear_sink {
            sink.clear()?;
            self.clear_sink = false;
        }
        Ok(())
    }

    pub fn stats(&self) -> AudioStats {
        let mut stats = self.stats.clone();
        stats.depth_p50_ms = quantile(&self.depths, 0.50);
        stats.depth_p95_ms = quantile(&self.depths, 0.95);
        stats.receipt_to_render_p50_ms = quantile(&self.latencies, 0.50);
        stats.receipt_to_render_p95_ms = quantile(&self.latencies, 0.95);
        stats.output_rms = if self.energy_samples == 0 {
            0.0
        } else {
            (self.energy / self.energy_samples as f64).sqrt()
        };
        stats.output_frequency_hz = if self.tone_frames == 0 {
            0.0
        } else {
            self.crossings as f64 * SAMPLE_RATE as f64 / self.tone_frames as f64
        };
        stats
    }
}

fn sample(values: &mut VecDeque<f64>, value: f64) {
    if values.len() == MAX_SAMPLES {
        values.pop_front();
    }
    values.push_back(value);
}
fn quantile(values: &VecDeque<f64>, q: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = values.iter().copied().collect();
    sorted.sort_by(f64::total_cmp);
    Some(sorted[((sorted.len() - 1) as f64 * q).round() as usize])
}
