//! Small streaming format adapter for the endpoint mix format. The WASAPI
//! low-period API permits EVENTCALLBACK but not AUTOCONVERTPCM. Retaining a
//! rational phase across writes avoids cumulative 44.1/48 kHz drift.
use super::wire::SAMPLE_RATE;
use anyhow::{ensure, Result};

#[derive(Clone, Copy, Debug)]
pub struct NativeFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub float: bool,
}
impl NativeFormat {
    pub fn validate(self) -> Result<Self> {
        ensure!(
            (8_000..=192_000).contains(&self.sample_rate),
            "unsupported endpoint rate"
        );
        ensure!(
            (1..=8).contains(&self.channels),
            "unsupported endpoint channels"
        );
        ensure!(
            if self.float {
                self.bits == 32
            } else {
                [16, 24, 32].contains(&self.bits)
            },
            "unsupported endpoint sample representation"
        );
        Ok(self)
    }
    pub fn stereo() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            channels: 2,
            bits: 32,
            float: true,
        }
    }
    pub fn frame_bytes(self) -> usize {
        self.channels as usize * self.bits as usize / 8
    }
}

pub struct Converter {
    pub format: NativeFormat,
    remainder: u64,
    previous: [f32; 2],
}
impl Converter {
    pub fn new(format: NativeFormat) -> Self {
        Self {
            format,
            remainder: 0,
            previous: [0.0; 2],
        }
    }
    pub fn reset(&mut self) {
        self.remainder = 0;
        self.previous = [0.0; 2];
    }
    /// Largest number of input frames whose converted output fits exactly in
    /// the currently available native buffer, including the retained phase.
    pub fn input_capacity(&self, native_frames: u32) -> usize {
        if native_frames == 0 {
            return 0;
        }
        (((native_frames as u64 + 1) * SAMPLE_RATE as u64 - 1 - self.remainder)
            / self.format.sample_rate as u64) as usize
    }
    pub fn convert(&mut self, pcm: &[f32]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(pcm.len() / 2 * self.format.frame_bytes() * 4);
        for input in pcm.chunks_exact(2) {
            let current = [input[0].clamp(-1.0, 1.0), input[1].clamp(-1.0, 1.0)];
            self.remainder += self.format.sample_rate as u64;
            while self.remainder >= SAMPLE_RATE as u64 {
                let t = 1.0
                    - (self.remainder - SAMPLE_RATE as u64) as f32 / self.format.sample_rate as f32;
                let stereo = [
                    self.previous[0] + (current[0] - self.previous[0]) * t,
                    self.previous[1] + (current[1] - self.previous[1]) * t,
                ];
                for channel in 0..self.format.channels {
                    let value = if self.format.channels == 1 {
                        (stereo[0] + stereo[1]) * 0.5
                    } else {
                        match channel {
                            0 => stereo[0],
                            1 => stereo[1],
                            _ => 0.0,
                        }
                    };
                    if self.format.float {
                        bytes.extend_from_slice(&value.to_le_bytes());
                    } else {
                        let scale = ((1u64 << (self.format.bits - 1)) - 1) as f64;
                        let integer = (value as f64 * scale).round() as i32;
                        bytes.extend_from_slice(
                            &integer.to_le_bytes()[..self.format.bits as usize / 8],
                        );
                    }
                }
                self.remainder -= SAMPLE_RATE as u64;
            }
            self.previous = current;
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rational_resampling_has_no_frame_drift_and_never_overfills_native_capacity() {
        for rate in [8_000, 44_100, 48_000, 96_000, 192_000] {
            let format = NativeFormat {
                sample_rate: rate,
                channels: 2,
                bits: 32,
                float: true,
            }
            .validate()
            .unwrap();
            let mut converter = Converter::new(format);
            let mut inputs = 0u64;
            let mut outputs = 0u64;
            for available in (1..500).cycle().take(3000) {
                let count = converter.input_capacity(available);
                let bytes = converter.convert(&vec![0.2; count * 2]);
                let frames = bytes.len() / format.frame_bytes();
                assert!(frames <= available as usize);
                inputs += count as u64;
                outputs += frames as u64;
                assert_eq!(outputs, inputs * rate as u64 / SAMPLE_RATE as u64);
            }
        }
    }
    #[test]
    fn integer_and_channel_layouts_are_initialized_and_reset_clears_history() {
        for bits in [16, 24, 32] {
            let format = NativeFormat {
                sample_rate: SAMPLE_RATE,
                channels: 6,
                bits,
                float: false,
            }
            .validate()
            .unwrap();
            let mut converter = Converter::new(format);
            let bytes = converter.convert(&[0.5, -0.5]);
            assert_eq!(bytes.len(), format.frame_bytes());
            assert!(bytes[bits as usize / 8 * 2..].iter().all(|v| *v == 0));
            converter.reset();
            assert!(converter.convert(&[0.0, 0.0]).iter().all(|v| *v == 0));
        }
        let format = NativeFormat {
            sample_rate: SAMPLE_RATE,
            channels: 1,
            bits: 32,
            float: true,
        };
        let mut converter = Converter::new(format);
        assert_eq!(converter.convert(&[0.5, -0.5]), 0.0f32.to_le_bytes());
    }
}
