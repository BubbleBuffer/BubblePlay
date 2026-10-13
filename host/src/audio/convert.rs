//! Turns WASAPI loopback bytes into 48 kHz stereo f32: decode the device sample
//! format, fold the channels to stereo, then resample linearly with state kept
//! between packets.

use anyhow::{bail, ensure, Result};

use super::wire::SAMPLE_RATE;

const SQRT_HALF: f32 = std::f32::consts::FRAC_1_SQRT_2;

const SPEAKER_FRONT_LEFT: u32 = 0x1;
const SPEAKER_FRONT_RIGHT: u32 = 0x2;
const SPEAKER_FRONT_CENTER: u32 = 0x4;
const SPEAKER_LOW_FREQUENCY: u32 = 0x8;
const SPEAKER_BACK_LEFT: u32 = 0x10;
const SPEAKER_BACK_RIGHT: u32 = 0x20;
const SPEAKER_FRONT_LEFT_OF_CENTER: u32 = 0x40;
const SPEAKER_FRONT_RIGHT_OF_CENTER: u32 = 0x80;
const SPEAKER_BACK_CENTER: u32 = 0x100;
const SPEAKER_SIDE_LEFT: u32 = 0x200;
const SPEAKER_SIDE_RIGHT: u32 = 0x400;

/// Speaker order used when a multichannel format carries no usable channel mask.
const DEFAULT_LAYOUT: [u32; 8] = [
    SPEAKER_FRONT_LEFT,
    SPEAKER_FRONT_RIGHT,
    SPEAKER_FRONT_CENTER,
    SPEAKER_LOW_FREQUENCY,
    SPEAKER_BACK_LEFT,
    SPEAKER_BACK_RIGHT,
    SPEAKER_SIDE_LEFT,
    SPEAKER_SIDE_RIGHT,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleKind {
    Float32,
    Int16,
    Int24,
    Int32,
}

/// The device's delivered format, as reported by WASAPI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceFormat {
    pub kind: SampleKind,
    pub channels: usize,
    pub sample_rate: u32,
    pub channel_mask: u32,
}

impl SourceFormat {
    /// `is_float` selects IEEE float; otherwise integer PCM of `bits` container width.
    pub fn new(
        is_float: bool,
        bits: u16,
        channels: u16,
        sample_rate: u32,
        channel_mask: u32,
    ) -> Result<Self> {
        ensure!(
            (1..=8).contains(&channels),
            "unsupported channel count {channels}"
        );
        ensure!(
            (8_000..=384_000).contains(&sample_rate),
            "unsupported sample rate {sample_rate}"
        );
        let kind = match (is_float, bits) {
            (true, 32) => SampleKind::Float32,
            (false, 16) => SampleKind::Int16,
            (false, 24) => SampleKind::Int24,
            (false, 32) => SampleKind::Int32,
            _ => bail!("unsupported sample format: float={is_float} bits={bits}"),
        };
        Ok(Self {
            kind,
            channels: channels as usize,
            sample_rate,
            channel_mask,
        })
    }

    pub fn bytes_per_sample(&self) -> usize {
        match self.kind {
            SampleKind::Float32 | SampleKind::Int32 => 4,
            SampleKind::Int16 => 2,
            SampleKind::Int24 => 3,
        }
    }

    pub fn frame_bytes(&self) -> usize {
        self.bytes_per_sample() * self.channels
    }
}

/// Appends the samples in `bytes` as f32 in -1..1. Bytes that do not form a
/// whole sample are ignored.
pub fn decode_samples(bytes: &[u8], kind: SampleKind, out: &mut Vec<f32>) {
    match kind {
        SampleKind::Float32 => out.extend(
            bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
        ),
        SampleKind::Int16 => out.extend(
            bytes
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32_768.0),
        ),
        SampleKind::Int24 => out.extend(bytes.chunks_exact(3).map(|c| {
            // Put the 24 bits in the top of an i32, then shift back with sign.
            (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8_388_608.0
        })),
        SampleKind::Int32 => out.extend(
            bytes
                .chunks_exact(4)
                .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0),
        ),
    }
}

/// (gain into left, gain into right) for one speaker position.
fn speaker_gains(speaker: u32) -> (f32, f32) {
    match speaker {
        SPEAKER_FRONT_LEFT => (1.0, 0.0),
        SPEAKER_FRONT_RIGHT => (0.0, 1.0),
        SPEAKER_FRONT_CENTER => (SQRT_HALF, SQRT_HALF),
        SPEAKER_LOW_FREQUENCY => (0.0, 0.0),
        SPEAKER_BACK_LEFT | SPEAKER_SIDE_LEFT | SPEAKER_FRONT_LEFT_OF_CENTER => (SQRT_HALF, 0.0),
        SPEAKER_BACK_RIGHT | SPEAKER_SIDE_RIGHT | SPEAKER_FRONT_RIGHT_OF_CENTER => (0.0, SQRT_HALF),
        SPEAKER_BACK_CENTER => (0.5, 0.5),
        // Height and other positions have no stereo home; spread them evenly.
        _ => (SQRT_HALF, SQRT_HALF),
    }
}

/// Per-channel gains into the stereo pair. Mono is duplicated to both sides;
/// stereo passes through; wider layouts use the mask bits in ascending order
/// (the WAVE convention), or the default order when the mask does not fit.
fn channel_gains(channels: usize, mask: u32) -> Vec<(f32, f32)> {
    if channels == 1 {
        return vec![(1.0, 1.0)];
    }
    if channels == 2 {
        return vec![(1.0, 0.0), (0.0, 1.0)];
    }
    let mut speakers: Vec<u32> = (0..32)
        .map(|bit| 1u32 << bit)
        .filter(|bit| mask & bit != 0)
        .collect();
    if speakers.len() != channels {
        speakers = (0..channels)
            .map(|i| DEFAULT_LAYOUT.get(i).copied().unwrap_or(0))
            .collect();
    }
    speakers.into_iter().map(speaker_gains).collect()
}

/// Folds interleaved source frames to stereo with clipping to -1..1.
pub struct Downmixer {
    channels: usize,
    gains: Vec<(f32, f32)>,
}

impl Downmixer {
    pub fn new(format: &SourceFormat) -> Self {
        Self {
            channels: format.channels,
            gains: channel_gains(format.channels, format.channel_mask),
        }
    }

    pub fn apply(&self, interleaved: &[f32], out: &mut Vec<[f32; 2]>) {
        for frame in interleaved.chunks_exact(self.channels) {
            let (mut left, mut right) = (0.0f32, 0.0f32);
            for (sample, (gl, gr)) in frame.iter().zip(&self.gains) {
                left += sample * gl;
                right += sample * gr;
            }
            out.push([left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0)]);
        }
    }
}

/// Streaming linear resampler. Position is kept as an integer count of
/// 1/out_rate input frames, so output does not depend on how input is chunked.
pub struct LinearResampler {
    in_rate: u64,
    out_rate: u64,
    pos: u64,
    buf: Vec<[f32; 2]>,
}

impl LinearResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            in_rate: in_rate as u64,
            out_rate: out_rate as u64,
            pos: 0,
            buf: Vec::new(),
        }
    }

    pub fn push(&mut self, input: &[[f32; 2]], out: &mut Vec<[f32; 2]>) {
        if self.in_rate == self.out_rate {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        loop {
            let index = (self.pos / self.out_rate) as usize;
            if index + 1 >= self.buf.len() {
                break;
            }
            let frac = (self.pos % self.out_rate) as f32 / self.out_rate as f32;
            let (a, b) = (self.buf[index], self.buf[index + 1]);
            out.push([a[0] + (b[0] - a[0]) * frac, a[1] + (b[1] - a[1]) * frac]);
            self.pos += self.in_rate;
        }
        // Keep the frame the next output interpolates from. A high input rate can step
        // past the end of the buffer, so drop only what is buffered; the rest of the
        // offset stays in `pos` and is applied to the input pushed later.
        let index = (self.pos / self.out_rate) as usize;
        let drained = index.min(self.buf.len());
        if drained > 0 {
            self.buf.drain(..drained);
            self.pos -= drained as u64 * self.out_rate;
        }
    }
}

/// Source bytes to 48 kHz stereo, with all conversion state kept between packets.
pub struct Converter {
    format: SourceFormat,
    downmix: Downmixer,
    resampler: LinearResampler,
    samples: Vec<f32>,
    stereo: Vec<[f32; 2]>,
}

impl Converter {
    pub fn new(format: SourceFormat) -> Self {
        Self {
            downmix: Downmixer::new(&format),
            resampler: LinearResampler::new(format.sample_rate, SAMPLE_RATE),
            format,
            samples: Vec::new(),
            stereo: Vec::new(),
        }
    }

    pub fn format(&self) -> &SourceFormat {
        &self.format
    }

    /// Appends the 48 kHz stereo frames for `bytes`, which holds whole interleaved frames.
    pub fn push_bytes(&mut self, bytes: &[u8], out: &mut Vec<[f32; 2]>) {
        self.samples.clear();
        decode_samples(bytes, self.format.kind, &mut self.samples);
        self.stereo.clear();
        self.downmix.apply(&self.samples, &mut self.stereo);
        self.resampler.push(&self.stereo, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(kind_float: bool, bits: u16, channels: u16, rate: u32, mask: u32) -> SourceFormat {
        SourceFormat::new(kind_float, bits, channels, rate, mask).unwrap()
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn rejects_unsupported_formats() {
        assert!(SourceFormat::new(true, 16, 2, 48_000, 0).is_err());
        assert!(SourceFormat::new(false, 8, 2, 48_000, 0).is_err());
        assert!(SourceFormat::new(false, 16, 0, 48_000, 0).is_err());
        assert!(SourceFormat::new(false, 16, 9, 48_000, 0).is_err());
        assert!(SourceFormat::new(false, 16, 2, 1_000, 0).is_err());
    }

    #[test]
    fn decodes_every_supported_sample_kind() {
        let mut out = Vec::new();
        decode_samples(&0.25f32.to_le_bytes(), SampleKind::Float32, &mut out);
        assert_eq!(out, vec![0.25]);

        out.clear();
        decode_samples(&(-16_384i16).to_le_bytes(), SampleKind::Int16, &mut out);
        assert!(close(out[0], -0.5));

        out.clear();
        // -2^22 as 24-bit little endian: 0x00C00000 -> bytes 00 00 C0.
        decode_samples(&[0x00, 0x00, 0xC0], SampleKind::Int24, &mut out);
        assert!(close(out[0], -0.5), "got {}", out[0]);
        out.clear();
        decode_samples(&[0xFF, 0xFF, 0x7F], SampleKind::Int24, &mut out);
        assert!(out[0] > 0.999 && out[0] < 1.0);

        out.clear();
        decode_samples(&(1i32 << 30).to_le_bytes(), SampleKind::Int32, &mut out);
        assert!(close(out[0], 0.5));
    }

    #[test]
    fn mono_is_duplicated_to_both_sides() {
        let f = fmt(true, 32, 1, 48_000, 0x4);
        let d = Downmixer::new(&f);
        let mut out = Vec::new();
        d.apply(&[0.1, -0.2, 0.3], &mut out);
        assert_eq!(out.len(), 3);
        for (frame, expected) in out.iter().zip([0.1f32, -0.2, 0.3]) {
            assert!(close(frame[0], expected) && close(frame[1], expected));
        }
    }

    #[test]
    fn stereo_passes_through() {
        let f = fmt(true, 32, 2, 48_000, 0x3);
        let d = Downmixer::new(&f);
        let mut out = Vec::new();
        d.apply(&[0.25, -0.5, 0.75, 0.0], &mut out);
        assert_eq!(out, vec![[0.25, -0.5], [0.75, 0.0]]);
    }

    #[test]
    fn five_point_one_folds_centre_and_surrounds_and_drops_lfe() {
        // FL FR FC LFE BL BR, mask 0x3F.
        let f = fmt(true, 32, 6, 48_000, 0x3F);
        let d = Downmixer::new(&f);
        let mut out = Vec::new();
        d.apply(&[0.1, 0.2, 0.3, 0.9, 0.5, 0.6], &mut out);
        let left = 0.1 + SQRT_HALF * (0.3 + 0.5);
        let right = 0.2 + SQRT_HALF * (0.3 + 0.6);
        assert!(close(out[0][0], left), "left {}", out[0][0]);
        assert!(close(out[0][1], right), "right {}", out[0][1]);
    }

    #[test]
    fn five_point_one_without_mask_uses_default_order() {
        let masked = fmt(true, 32, 6, 48_000, 0x3F);
        let unmasked = fmt(true, 32, 6, 48_000, 0);
        let frame = [0.1, 0.2, 0.3, 0.9, 0.5, 0.6];
        let (mut a, mut b) = (Vec::new(), Vec::new());
        Downmixer::new(&masked).apply(&frame, &mut a);
        Downmixer::new(&unmasked).apply(&frame, &mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn downmix_clips_to_unit_range() {
        let f = fmt(true, 32, 2, 48_000, 0x3);
        let mut out = Vec::new();
        Downmixer::new(&f).apply(&[3.0, -3.0], &mut out);
        assert_eq!(out, vec![[1.0, -1.0]]);
    }

    #[test]
    fn resampler_is_identity_at_48k() {
        let mut r = LinearResampler::new(48_000, 48_000);
        let input: Vec<[f32; 2]> = (0..100).map(|i| [i as f32, -(i as f32)]).collect();
        let mut out = Vec::new();
        r.push(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn resampler_44k1_produces_expected_frame_count_per_second() {
        // Output k sits at input position k * 44100 / 48000. The last output that
        // still has its successor input frame is k = 47998, so 47999 are produced.
        let mut r = LinearResampler::new(44_100, 48_000);
        let input = vec![[0.0f32; 2]; 44_100];
        let mut out = Vec::new();
        r.push(&input, &mut out);
        assert_eq!(out.len(), 47_999);
    }

    #[test]
    fn resampler_output_is_independent_of_chunking() {
        let input: Vec<[f32; 2]> = (0..5_000)
            .map(|i| {
                [
                    ((i * 37) % 101) as f32 / 101.0,
                    ((i * 11) % 53) as f32 / 53.0,
                ]
            })
            .collect();
        let mut whole = Vec::new();
        LinearResampler::new(44_100, 48_000).push(&input, &mut whole);

        let mut chunked = Vec::new();
        let mut r = LinearResampler::new(44_100, 48_000);
        for chunk in input.chunks(441).chain(std::iter::once(&input[0..0])) {
            r.push(chunk, &mut chunked);
        }
        // Chunk boundaries of uneven sizes must not change a single output frame.
        let mut uneven = Vec::new();
        let mut r2 = LinearResampler::new(44_100, 48_000);
        let mut rest: &[[f32; 2]] = &input;
        let mut size = 1;
        while !rest.is_empty() {
            let take = size.min(rest.len());
            r2.push(&rest[..take], &mut uneven);
            rest = &rest[take..];
            size = size * 3 % 997 + 1;
        }
        assert_eq!(whole, chunked);
        assert_eq!(whole, uneven);
    }

    #[test]
    fn resampler_192k_tiny_chunks_match_whole_input_without_panicking() {
        // 192 kHz input advances 4 frames per output, so the read position can run
        // past the buffer end. One-frame and odd-sized pushes hit that on every call.
        let input: Vec<[f32; 2]> = (0..192_000)
            .map(|i| {
                [
                    ((i * 7) % 113) as f32 / 113.0,
                    -(((i * 5) % 29) as f32) / 29.0,
                ]
            })
            .collect();
        let mut whole = Vec::new();
        LinearResampler::new(192_000, 48_000).push(&input, &mut whole);
        // Outputs k with 4k + 1 < 192000 exist: k = 0..=47999.
        assert_eq!(whole.len(), 48_000);

        let mut tiny = Vec::new();
        let mut r = LinearResampler::new(192_000, 48_000);
        for chunk in input.chunks(1) {
            r.push(chunk, &mut tiny);
        }
        assert_eq!(tiny, whole, "one-frame chunks");

        let mut odd = Vec::new();
        let mut r = LinearResampler::new(192_000, 48_000);
        let sizes = [1usize, 2, 3, 5, 7];
        let mut rest: &[[f32; 2]] = &input;
        let mut i = 0;
        while !rest.is_empty() {
            let take = sizes[i % sizes.len()].min(rest.len());
            r.push(&rest[..take], &mut odd);
            rest = &rest[take..];
            i += 1;
        }
        assert_eq!(odd, whole, "mixed odd-sized chunks");
    }

    #[test]
    fn resampler_preserves_a_linear_ramp() {
        // Linear interpolation of a ramp is exact, so output k must equal k * 44100 / 48000 / 1000.
        let input: Vec<[f32; 2]> = (0..2_000).map(|i| [i as f32 / 1000.0, 0.0]).collect();
        let mut out = Vec::new();
        LinearResampler::new(44_100, 48_000).push(&input, &mut out);
        for (k, frame) in out.iter().enumerate() {
            let expected = k as f64 * 44_100.0 / 48_000.0 / 1000.0;
            assert!((frame[0] as f64 - expected).abs() < 1e-4, "frame {k}");
        }
    }

    #[test]
    fn converter_runs_bytes_through_decode_fold_and_resample() {
        // 44.1 kHz stereo 16-bit: one second of silence becomes 47999 stereo frames.
        let format = SourceFormat::new(false, 16, 2, 44_100, 0x3).unwrap();
        let mut conv = Converter::new(format);
        let mut out = Vec::new();
        conv.push_bytes(&vec![0u8; 44_100 * 4], &mut out);
        assert_eq!(out.len(), 47_999);
        assert!(out.iter().all(|f| *f == [0.0, 0.0]));
    }
}
