use anyhow::Result;

/// The renderer is pull driven. The sink reports currently writable frames and
/// already queued duration; all PCM is 48 kHz interleaved stereo float. No sink
/// may retain the borrowed PCM. COM/device objects stay on the renderer thread.
pub trait AudioSink {
    fn writable(&mut self, now_us: u64) -> Result<(usize, f64)>;
    fn write(&mut self, pcm: &[f32]) -> Result<()>;
    fn clear(&mut self) -> Result<()>;
    fn wait(&self, timeout_ms: u32);
    fn default_changed(&self) -> bool {
        false
    }
    fn buffer_ms(&self) -> f64;
    fn period_ms(&self) -> f64;
    fn observes_device_padding(&self) -> bool {
        false
    }
}

/// Clocked sink with no OS interaction. Its 5 ms deadlines retain their phase
/// across scheduler delays; a wake can consume several elapsed quanta. Catch-up
/// is capped at 50 ms and PCM is discarded after engine measurement.
#[derive(Default)]
pub struct FakeSink {
    next_us: Option<u64>,
    pub frames: u64,
    pub fail_next: bool,
}

impl AudioSink for FakeSink {
    fn writable(&mut self, now_us: u64) -> Result<(usize, f64)> {
        if self.fail_next {
            self.fail_next = false;
            anyhow::bail!("injected output device failure");
        }
        let next = self.next_us.get_or_insert(now_us);
        if now_us < *next {
            return Ok((0, 0.0));
        }
        let due = (now_us - *next) / 5_000 + 1;
        let quanta = due.min(10);
        *next = if due > 10 {
            now_us + 5_000
        } else {
            *next + quanta * 5_000
        };
        Ok((240 * quanta as usize, 0.0))
    }
    fn write(&mut self, pcm: &[f32]) -> Result<()> {
        self.frames += (pcm.len() / 2) as u64;
        Ok(())
    }
    fn clear(&mut self) -> Result<()> {
        self.next_us = None;
        Ok(())
    }
    fn wait(&self, timeout_ms: u32) {
        std::thread::sleep(std::time::Duration::from_millis(timeout_ms as u64));
    }
    fn buffer_ms(&self) -> f64 {
        5.0
    }
    fn period_ms(&self) -> f64 {
        5.0
    }
}
