//! Opus encoding of 10 ms stereo frames for the host audio stream.

use anyhow::{ensure, Context, Result};

use super::wire::{FRAME_SAMPLES, MAX_PAYLOAD_BYTES, SAMPLE_RATE};

/// Interleaved f32 samples in one 10 ms stereo frame.
pub const FRAME_LEN: usize = FRAME_SAMPLES as usize * 2;
const BITRATE_BPS: i32 = 128_000;

/// Application Audio, 128 kbps VBR, stereo, no in-band FEC, no DTX.
pub struct OpusEncoder {
    inner: opus::Encoder,
    scratch: Vec<u8>,
}

impl OpusEncoder {
    pub fn new() -> Result<Self> {
        let mut inner = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )
        .context("create opus encoder")?;
        inner
            .set_bitrate(opus::Bitrate::Bits(BITRATE_BPS))
            .context("set opus bitrate")?;
        inner.set_vbr(true).context("set opus vbr")?;
        inner.set_inband_fec(false).context("disable opus fec")?;
        inner.set_dtx(false).context("disable opus dtx")?;
        Ok(Self {
            inner,
            scratch: vec![0; MAX_PAYLOAD_BYTES],
        })
    }

    /// Encodes exactly one 10 ms frame (480 stereo samples, interleaved).
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>> {
        ensure!(
            pcm.len() == FRAME_LEN,
            "opus frame needs {FRAME_LEN} samples, got {}",
            pcm.len()
        );
        let len = self
            .inner
            .encode_float(pcm, &mut self.scratch)
            .context("encode opus frame")?;
        Ok(self.scratch[..len].to_vec())
    }
}

/// Collects 48 kHz stereo frames and hands out whole 10 ms frames.
#[derive(Default)]
pub struct FrameAssembler {
    pending: Vec<f32>,
}

impl FrameAssembler {
    pub fn push(&mut self, frames: &[[f32; 2]]) {
        for frame in frames {
            self.pending.extend_from_slice(frame);
        }
    }

    /// Removes and returns the next complete frame, if one is buffered.
    pub fn pop(&mut self) -> Option<Vec<f32>> {
        if self.pending.len() < FRAME_LEN {
            return None;
        }
        Some(self.pending.drain(..FRAME_LEN).collect())
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_frame(phase: &mut f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(FRAME_LEN);
        for _ in 0..FRAME_SAMPLES {
            let v = (*phase).sin() * 0.25;
            out.push(v);
            out.push(v);
            *phase += 2.0 * std::f32::consts::PI * 440.0 / SAMPLE_RATE as f32;
        }
        out
    }

    #[test]
    fn encoder_turns_each_480_sample_frame_into_one_packet() {
        let mut enc = OpusEncoder::new().expect("encoder");
        let mut phase = 0.0f32;
        for _ in 0..20 {
            let packet = enc.encode(&sine_frame(&mut phase)).expect("encode");
            assert!(!packet.is_empty());
            assert!(
                packet.len() <= MAX_PAYLOAD_BYTES,
                "packet {} bytes",
                packet.len()
            );
            let samples = opus::packet::get_nb_samples(&packet, SAMPLE_RATE).expect("packet");
            assert_eq!(samples, FRAME_SAMPLES as usize);
        }
    }

    #[test]
    fn encoder_rejects_frames_of_the_wrong_length() {
        let mut enc = OpusEncoder::new().expect("encoder");
        assert!(enc.encode(&[0.0; FRAME_LEN - 2]).is_err());
        assert!(enc.encode(&[0.0; FRAME_LEN + 2]).is_err());
        // Mono-length input is not a stereo 10 ms frame.
        assert!(enc.encode(&[0.0; FRAME_LEN / 2]).is_err());
    }

    #[test]
    fn encoder_packets_decode_back_to_480_stereo_samples() {
        let mut enc = OpusEncoder::new().expect("encoder");
        let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).expect("decoder");
        let mut phase = 0.0f32;
        let mut out = vec![0f32; FRAME_LEN];
        let mut decoded_any_energy = false;
        for _ in 0..10 {
            let packet = enc.encode(&sine_frame(&mut phase)).expect("encode");
            let n = dec.decode_float(&packet, &mut out, false).expect("decode");
            assert_eq!(n, FRAME_SAMPLES as usize);
            decoded_any_energy |= out.iter().any(|s| s.abs() > 0.01);
        }
        assert!(decoded_any_energy, "decoded sine should not be silent");
    }

    #[test]
    fn assembler_yields_whole_frames_only() {
        let mut asm = FrameAssembler::default();
        asm.push(&vec![[0.5, -0.5]; 300]);
        assert!(asm.pop().is_none());
        asm.push(&vec![[0.25, 0.25]; 300]);
        let first = asm.pop().expect("480 frames buffered");
        assert_eq!(first.len(), FRAME_LEN);
        assert_eq!(&first[..2], &[0.5, -0.5]);
        assert!(asm.pop().is_none(), "120 frames remain");
        asm.push(&vec![[0.0, 0.0]; 360]);
        let second = asm.pop().expect("second frame");
        assert_eq!(second.len(), FRAME_LEN);
        assert!(asm.pop().is_none());
    }

    #[test]
    fn assembler_clear_drops_partial_frames() {
        let mut asm = FrameAssembler::default();
        asm.push(&vec![[1.0, 1.0]; 100]);
        asm.clear();
        asm.push(&vec![[0.0, 0.0]; FRAME_SAMPLES as usize]);
        let frame = asm.pop().expect("frame");
        assert!(frame.iter().all(|s| *s == 0.0));
    }
}
