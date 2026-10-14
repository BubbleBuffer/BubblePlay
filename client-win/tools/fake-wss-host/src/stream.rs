//! Recorded Annex-B streams: access-unit splitting (AUD delimited, as in the
//! decode-latency spike's `streams.rs`), IDR and parameter-set inspection,
//! SPS dimensions, and the payload builder that puts parameter sets in front
//! of a keyframe that lacks them.

use std::ops::Range;
use std::path::Path;

use anyhow::{ensure, Context, Result};

use crate::protocol::{CODEC_H264, CODEC_HEVC};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    /// The `--stream` name and the wire name the browser uses.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "h264" => Some(Codec::H264),
            "hevc" => Some(Codec::Hevc),
            _ => None,
        }
    }

    pub fn wire_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
        }
    }

    pub fn bpv4(self) -> u8 {
        match self {
            Codec::H264 => CODEC_H264,
            Codec::Hevc => CODEC_HEVC,
        }
    }

    pub fn file_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264.h264",
            Codec::Hevc => "hevc.hevc",
        }
    }

    /// `start` names the codec as the browser does (`"h264"`/`"hevc"`); an
    /// RFC 6381 prefix (`avc1`, `hev1`, `hvc1`) is accepted too.
    pub fn matches_start_value(self, value: &str) -> bool {
        match self {
            Codec::H264 => value == "h264" || value.starts_with("avc1"),
            Codec::Hevc => {
                value == "hevc" || value.starts_with("hev1") || value.starts_with("hvc1")
            }
        }
    }

    /// NAL unit type from the first header byte. HEVC's type sits in the
    /// middle bits of that byte, so the second header byte is never needed.
    pub fn nal_type(self, first: u8) -> u8 {
        match self {
            Codec::H264 => first & 0x1f,
            Codec::Hevc => (first >> 1) & 0x3f,
        }
    }

    fn is_aud(self, nal_type: u8) -> bool {
        match self {
            Codec::H264 => nal_type == 9,
            Codec::Hevc => nal_type == 35,
        }
    }

    fn is_idr(self, nal_type: u8) -> bool {
        match self {
            Codec::H264 => nal_type == 5,
            Codec::Hevc => nal_type == 19 || nal_type == 20,
        }
    }

    fn sps_type(self) -> u8 {
        match self {
            Codec::H264 => 7,
            Codec::Hevc => 33,
        }
    }

    /// Parameter-set NAL types that must precede a keyframe for MF to start.
    fn param_set_types(self) -> &'static [u8] {
        match self {
            Codec::H264 => &[7, 8],
            Codec::Hevc => &[32, 33, 34],
        }
    }
}

/// One NAL unit: `start` is the first byte of its start code, `payload` the
/// NAL header byte, `end` the byte after the unit (exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalSpan {
    pub start: usize,
    pub payload: usize,
    pub end: usize,
}

/// Every NAL unit in an Annex-B buffer, with 3- or 4-byte start codes.
pub fn nal_spans(data: &[u8]) -> Vec<NalSpan> {
    let mut starts: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((start, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &(start, payload))| NalSpan {
            start,
            payload,
            end: starts.get(n + 1).map_or(data.len(), |next| next.0),
        })
        .collect()
}

/// One access unit: a byte range of the stream that starts at its AUD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub range: Range<usize>,
    pub key: bool,
    pub has_param_sets: bool,
    /// Offset inside `range` just after the leading AUD, where missing
    /// parameter sets are inserted (0 when the unit has no leading AUD).
    pub insert_at: usize,
}

/// Split an Annex-B buffer at its access-unit delimiters. The first unit
/// always starts at byte 0, whatever precedes its AUD.
pub fn split_access_units(data: &[u8], codec: Codec) -> Result<Vec<AccessUnit>> {
    let nals = nal_spans(data);
    let kind_of = |nal: &NalSpan| codec.nal_type(data[nal.payload]);
    let aud_starts: Vec<usize> = nals
        .iter()
        .filter(|nal| codec.is_aud(kind_of(nal)))
        .map(|nal| nal.start)
        .collect();
    ensure!(
        !aud_starts.is_empty(),
        "stream has no access-unit delimiters"
    );

    let mut bounds = aud_starts;
    bounds[0] = 0;
    bounds.push(data.len());
    let mut units = Vec::with_capacity(bounds.len() - 1);
    for window in bounds.windows(2) {
        let range = window[0]..window[1];
        let mut key = false;
        let mut present = Vec::new();
        let mut insert_at = 0;
        for nal in nals
            .iter()
            .filter(|nal| nal.start >= range.start && nal.start < range.end)
        {
            let kind = kind_of(nal);
            if codec.is_aud(kind) && nal.start == range.start {
                insert_at = nal.end - range.start;
            }
            key |= codec.is_idr(kind);
            if codec.param_set_types().contains(&kind) {
                present.push(kind);
            }
        }
        let has_param_sets = codec
            .param_set_types()
            .iter()
            .all(|kind| present.contains(kind));
        units.push(AccessUnit {
            range,
            key,
            has_param_sets,
            insert_at,
        });
    }
    Ok(units)
}

/// A loaded recording, with the parameter sets to inject and its dimensions.
#[derive(Debug)]
pub struct Stream {
    pub codec: Codec,
    pub data: Vec<u8>,
    pub units: Vec<AccessUnit>,
    pub width: u32,
    pub height: u32,
    /// Annex-B parameter sets (VPS/SPS/PPS or SPS/PPS) from the first keyframe
    /// that carries all of them.
    pub param_sets: Vec<u8>,
    /// Keyframes that already carried their parameter sets in the recording.
    pub native_param_set_keyframes: usize,
}

impl Stream {
    pub fn load(codec: Codec, path: &Path) -> Result<Self> {
        let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        Self::from_bytes(codec, data).with_context(|| format!("analyse {}", path.display()))
    }

    pub fn from_bytes(codec: Codec, data: Vec<u8>) -> Result<Self> {
        let units = split_access_units(&data, codec)?;
        ensure!(units[0].key, "stream must start at an IDR access unit");
        let nals = nal_spans(&data);
        let kind_of = |nal: &NalSpan| codec.nal_type(data[nal.payload]);

        let mut param_sets = Vec::new();
        if let Some(unit) = units.iter().find(|unit| unit.key && unit.has_param_sets) {
            for nal in nals
                .iter()
                .filter(|nal| nal.start >= unit.range.start && nal.start < unit.range.end)
            {
                if codec.param_set_types().contains(&kind_of(nal)) {
                    param_sets.extend_from_slice(&data[nal.start..nal.end]);
                }
            }
        }
        ensure!(!param_sets.is_empty(), "no keyframe carries parameter sets");
        let native_param_set_keyframes = units
            .iter()
            .filter(|unit| unit.key && unit.has_param_sets)
            .count();

        let sps = nals
            .iter()
            .find(|nal| kind_of(nal) == codec.sps_type())
            .context("stream has no SPS")?;
        let (width, height) = match codec {
            Codec::H264 => h264_sps_dimensions(&rbsp(&data[sps.payload + 1..sps.end]))?,
            Codec::Hevc => hevc_sps_dimensions(&rbsp(&data[sps.payload + 2..sps.end]))?,
        };
        Ok(Self {
            codec,
            data,
            units,
            width,
            height,
            param_sets,
            native_param_set_keyframes,
        })
    }

    pub fn len(&self) -> usize {
        self.units.len()
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    pub fn is_key(&self, index: usize) -> bool {
        self.units[index].key
    }

    /// The first IDR at or after `index`, wrapping to 0. Unit 0 is always an
    /// IDR, so the wrap never lands mid-GOP.
    pub fn next_key_at_or_after(&self, index: usize) -> usize {
        let len = self.units.len();
        (0..len)
            .map(|step| (index + step) % len)
            .find(|&i| self.units[i].key)
            .unwrap_or(0)
    }

    /// The access unit as it goes on the wire. A keyframe that lacks parameter
    /// sets gets the stream's own sets right after its AUD, so MF can configure
    /// itself from the keyframe alone.
    pub fn payload(&self, index: usize) -> Vec<u8> {
        let unit = &self.units[index];
        let au = &self.data[unit.range.clone()];
        if !unit.key || unit.has_param_sets {
            return au.to_vec();
        }
        let mut out = Vec::with_capacity(au.len() + self.param_sets.len());
        out.extend_from_slice(&au[..unit.insert_at]);
        out.extend_from_slice(&self.param_sets);
        out.extend_from_slice(&au[unit.insert_at..]);
        out
    }

    pub fn idr_indices(&self) -> Vec<usize> {
        self.units
            .iter()
            .enumerate()
            .filter(|(_, unit)| unit.key)
            .map(|(i, _)| i)
            .collect()
    }
}

/// Remove emulation-prevention bytes (`00 00 03` -> `00 00`).
pub fn rbsp(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut zeros = 0;
    for &byte in bytes {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

pub struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub fn bit(&mut self) -> Result<u32> {
        let byte = *self.bytes.get(self.pos >> 3).context("SPS truncated")?;
        let bit = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Ok(u32::from(bit))
    }

    pub fn bits(&mut self, count: usize) -> Result<u64> {
        let mut value = 0u64;
        for _ in 0..count {
            value = (value << 1) | u64::from(self.bit()?);
        }
        Ok(value)
    }

    pub fn skip(&mut self, count: usize) -> Result<()> {
        self.bits(count).map(|_| ())
    }

    pub fn ue(&mut self) -> Result<u64> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            ensure!(zeros < 32, "exp-golomb code too long");
        }
        Ok((1u64 << zeros) - 1 + self.bits(zeros)?)
    }

    pub fn se(&mut self) -> Result<i64> {
        let code = self.ue()? as i64;
        Ok(if code % 2 == 1 {
            (code + 1) / 2
        } else {
            -(code / 2)
        })
    }
}

fn skip_scaling_list(reader: &mut BitReader, size: usize) -> Result<()> {
    let mut last = 8i64;
    let mut next = 8i64;
    for _ in 0..size {
        if next != 0 {
            next = (last + reader.se()? + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

/// Width and height of an H.264 SPS RBSP (the bytes after the NAL header),
/// with the frame cropping applied.
pub fn h264_sps_dimensions(rbsp: &[u8]) -> Result<(u32, u32)> {
    let mut r = BitReader::new(rbsp);
    let profile = r.bits(8)? as u8;
    r.skip(16)?; // constraint flags, reserved bits, level_idc
    r.ue()?; // seq_parameter_set_id
    let mut chroma_format = 1u64;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format = r.ue()?;
        if chroma_format == 3 {
            r.skip(1)?;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? == 1 {
            let lists = if chroma_format == 3 { 12 } else { 8 };
            for index in 0..lists {
                if r.bit()? == 1 {
                    skip_scaling_list(&mut r, if index < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.skip(1)?;
            r.se()?;
            r.se()?;
            let cycle = r.ue()?;
            for _ in 0..cycle {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = r.ue()? + 1;
    let height_map_units = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        r.skip(1)?; // mb_adaptive_frame_field_flag
    }
    r.skip(1)?; // direct_8x8_inference_flag
    let (mut left, mut right, mut top, mut bottom) = (0u64, 0u64, 0u64, 0u64);
    if r.bit()? == 1 {
        left = r.ue()?;
        right = r.ue()?;
        top = r.ue()?;
        bottom = r.ue()?;
    }
    let frame_factor = 2 - u64::from(frame_mbs_only);
    let (crop_x, crop_y) = match chroma_format {
        1 => (2, 2 * frame_factor),
        2 => (2, frame_factor),
        0 => (1, frame_factor),
        _ => (1, frame_factor),
    };
    let width = width_mbs * 16 - (left + right) * crop_x;
    let height = height_map_units * 16 * frame_factor - (top + bottom) * crop_y;
    Ok((width as u32, height as u32))
}

/// Width and height of an HEVC SPS RBSP (the bytes after the 2-byte NAL
/// header), with the conformance window applied.
pub fn hevc_sps_dimensions(rbsp: &[u8]) -> Result<(u32, u32)> {
    let mut r = BitReader::new(rbsp);
    r.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)? as usize;
    r.skip(1)?; // sps_temporal_id_nesting_flag
    skip_profile_tier_level(&mut r, max_sub_layers_minus1)?;
    r.ue()?; // sps_seq_parameter_set_id
    let chroma_format = r.ue()?;
    if chroma_format == 3 {
        r.skip(1)?;
    }
    let width = r.ue()?;
    let height = r.ue()?;
    let (mut left, mut right, mut top, mut bottom) = (0u64, 0u64, 0u64, 0u64);
    if r.bit()? == 1 {
        left = r.ue()?;
        right = r.ue()?;
        top = r.ue()?;
        bottom = r.ue()?;
    }
    let (sub_w, sub_h) = match chroma_format {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let width = width
        .checked_sub((left + right) * sub_w)
        .context("HEVC conformance window too large")?;
    let height = height
        .checked_sub((top + bottom) * sub_h)
        .context("HEVC conformance window too large")?;
    Ok((width as u32, height as u32))
}

fn skip_profile_tier_level(r: &mut BitReader, max_sub_layers_minus1: usize) -> Result<()> {
    // general profile (88 bits) and general_level_idc (8 bits).
    r.skip(88 + 8)?;
    let mut profile_present = Vec::with_capacity(max_sub_layers_minus1);
    let mut level_present = Vec::with_capacity(max_sub_layers_minus1);
    for _ in 0..max_sub_layers_minus1 {
        profile_present.push(r.bit()? == 1);
        level_present.push(r.bit()? == 1);
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.skip(2)?;
        }
    }
    for i in 0..max_sub_layers_minus1 {
        if profile_present[i] {
            r.skip(88)?;
        }
        if level_present[i] {
            r.skip(8)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
        out
    }

    #[test]
    fn splits_h264_on_access_unit_delimiters() {
        // AUD=9, non-IDR slice=1, IDR slice=5, SPS=7, PPS=8.
        let mut data = annex_b(&[&[0x09, 0xf0], &[0x65, 0x88], &[0x41, 0x9a]]);
        data.extend(annex_b(&[&[0x09, 0xf0], &[0x41, 0x9b]]));
        let units = split_access_units(&data, Codec::H264).unwrap();
        assert_eq!(units.len(), 2);
        assert!(units[0].key);
        assert!(!units[1].key);
        assert_eq!(units[0].range.end, units[1].range.start);
        assert_eq!(units[1].range.end, data.len());
        assert_eq!(
            units[0].insert_at, 6,
            "after the 4-byte start code and the 2-byte AUD"
        );
    }

    #[test]
    fn hevc_types_come_from_the_first_header_byte() {
        assert_eq!(Codec::Hevc.nal_type(0x46), 35); // AUD
        assert_eq!(Codec::Hevc.nal_type(0x26), 19); // IDR_W_RADL
        assert_eq!(Codec::Hevc.nal_type(0x42), 33); // SPS
        assert_eq!(Codec::H264.nal_type(0x65), 5);
    }

    #[test]
    fn missing_delimiters_are_refused() {
        assert!(split_access_units(&annex_b(&[&[0x65, 0x88]]), Codec::H264).is_err());
    }

    #[test]
    fn rbsp_strips_emulation_prevention_bytes() {
        assert_eq!(rbsp(&[0, 0, 3, 1, 0, 0, 3, 0]), vec![0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn exp_golomb_reads_known_codes() {
        // ue: 1 -> 0, 010 -> 1, 011 -> 2, 00100 -> 3. se: 010 -> 1, 011 -> -1.
        let mut r = BitReader::new(&[0b1010_0110, 0b0100_0000]);
        assert_eq!(r.ue().unwrap(), 0);
        assert_eq!(r.ue().unwrap(), 1);
        assert_eq!(r.ue().unwrap(), 2);
        assert_eq!(r.ue().unwrap(), 3);
        let mut r = BitReader::new(&[0b0100_1100]);
        assert_eq!(r.se().unwrap(), 1);
        assert_eq!(r.se().unwrap(), -1);
    }

    #[test]
    fn recorded_streams_match_their_evidence() {
        // Runs against the real spike recordings when they are present.
        let dir = Path::new("F:/build/spike-streams");
        for codec in [Codec::H264, Codec::Hevc] {
            let path = dir.join(codec.file_name());
            if !path.exists() {
                eprintln!("SKIPPED {}: recording not present", path.display());
                continue;
            }
            let evidence = crate::evidence::load(dir, codec).unwrap();
            let stream = Stream::load(codec, &path).unwrap();
            assert_eq!(
                (stream.width, stream.height),
                (evidence.width, evidence.height),
                "SPS dimensions"
            );
            assert_eq!(stream.len(), evidence.frames, "access units");
            assert_eq!(
                stream.idr_indices(),
                evidence.idr_au_indices,
                "IDR positions"
            );
            // Whether each IDR carries its own parameter sets is a property of
            // the recording; the payload builder covers the ones that do not.
            eprintln!(
                "{}: {} of {} keyframes carry parameter sets in-band",
                codec.wire_name(),
                stream.native_param_set_keyframes,
                evidence.idr_au_indices.len()
            );
            assert!(stream.native_param_set_keyframes >= 1);
        }
    }
}
