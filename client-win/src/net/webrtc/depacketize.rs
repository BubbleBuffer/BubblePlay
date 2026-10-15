//! RTP depacketizers for H.264 (RFC 6184) and H.265 (RFC 7798).
//!
//! A [`Depacketizer`] takes RTP packets in arrival order and returns complete
//! Annex-B access units. An access unit is released only when it is contiguous
//! from a recognized start packet to its marker packet, every NAL in it is
//! complete, and it contains a VCL NAL. Anything else is discarded as a whole.
//! Reordering is bounded by [`Limits`]. A stalled stream is resolved by
//! [`Depacketizer::flush`], which the caller must run on a stall timer.

use std::collections::BTreeMap;
use std::fmt;

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_DIMENSION: u64 = 16_384;

/// Codec carried by the RTP stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

/// One RTP packet with the header fields the depacketizer needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtpPacketRef<'a> {
    pub sequence_number: u16,
    pub timestamp: u32,
    pub marker: bool,
    /// Payload after the RTP header, padding and extensions.
    pub payload: &'a [u8],
}

/// Bounds on buffering and on one access unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest Annex-B access unit that may be assembled, in bytes.
    pub max_access_unit_bytes: usize,
    /// Largest number of RTP packets that may make up one access unit.
    pub max_access_unit_packets: usize,
    /// Packets held while waiting for a gap to fill. Clamped to at least 1.
    pub max_reorder_packets: usize,
    /// Payload bytes held while waiting for a gap to fill.
    pub max_reorder_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_access_unit_bytes: 8 << 20,
            max_access_unit_packets: 4096,
            max_reorder_packets: 64,
            max_reorder_bytes: 512 << 10,
        }
    }
}

/// Picture size from the SPS. `coded_*` is the decoded, macroblock-aligned size.
/// `display_*` is the size after the cropping (H.264) or conformance (H.265) window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub coded_width: u32,
    pub coded_height: u32,
    pub display_width: u32,
    pub display_height: u32,
}

/// A complete access unit ready for a decoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessUnit {
    pub rtp_timestamp: u32,
    pub keyframe: bool,
    /// NAL units with 4-byte start codes. Emulation prevention bytes are kept.
    pub annex_b: Vec<u8>,
    /// Latest SPS geometry seen when this unit was released. `None` before any SPS.
    pub geometry: Option<Geometry>,
}

/// Why a payload was rejected. Rejected packets count as loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadError {
    Empty,
    Truncated,
    TooLarge,
    ForbiddenBit,
    UnsupportedType(u8),
    BadFragment,
}

impl fmt::Display for PayloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty RTP payload"),
            Self::Truncated => f.write_str("truncated RTP payload"),
            Self::TooLarge => f.write_str("RTP payload exceeds the size limit"),
            Self::ForbiddenBit => f.write_str("forbidden NAL bit set"),
            Self::UnsupportedType(t) => write!(f, "unsupported NAL type {t}"),
            Self::BadFragment => f.write_str("invalid aggregation or fragmentation unit"),
        }
    }
}

impl std::error::Error for PayloadError {}

/// Cumulative counters. They survive [`Depacketizer::reset`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Packets passed to `push`.
    pub packets: u64,
    /// Packets whose payload failed validation (counted as loss).
    pub malformed: u64,
    /// Packets older than the release point, or already buffered.
    pub late_or_duplicate: u64,
    /// Sequence numbers given up on as missing.
    pub lost_packets: u64,
    /// Access units released.
    pub access_units: u64,
    /// Released access units that are keyframes.
    pub keyframes: u64,
    /// Started access units discarded as incomplete or inconsistent.
    pub partial_discarded: u64,
    /// Access units discarded for exceeding [`Limits`].
    pub oversized_discarded: u64,
    /// Packets dropped because they could not start an access unit.
    pub unstarted_dropped: u64,
}

/// Turns RTP packets into Annex-B access units for one stream.
///
/// Reset the depacketizer when the SSRC, payload type or codec changes.
/// Call [`flush`](Self::flush) on a stall timer, because the reorder window
/// holds packets until a gap fills, a newer frame arrives, or the window fills.
#[derive(Debug)]
pub struct Depacketizer {
    codec: Codec,
    limits: Limits,
    /// Packets ahead of `expected`, keyed by unwrapped sequence number.
    window: BTreeMap<i64, Slot>,
    window_bytes: usize,
    /// Next unwrapped sequence number to process in order.
    expected: Option<i64>,
    /// Timestamp of the last packet processed in order.
    last_timestamp: Option<u32>,
    /// True when the last packet processed in order ended a frame.
    /// A new access unit may start only here, so a frame whose head was lost
    /// is never released.
    frame_boundary: bool,
    assembly: Option<Assembly>,
    geometry: Option<Geometry>,
    stats: Stats,
}

#[derive(Debug)]
enum Slot {
    Packet(Packet),
    /// A malformed packet or a gap that was given up on.
    Lost,
}

#[derive(Debug)]
struct Packet {
    timestamp: u32,
    marker: bool,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct Assembly {
    timestamp: u32,
    annex_b: Vec<u8>,
    packets: usize,
    keyframe: bool,
    has_vcl: bool,
    /// Open FU-A/FU fragment, if any.
    fragment: Option<Fragment>,
}

#[derive(Debug, Clone, Copy)]
struct Fragment {
    /// Offset of the start code in `Assembly::annex_b`.
    start: usize,
    nal_type: u8,
    info: NalInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    Oversized,
    Fragment,
}

impl Assembly {
    fn new(timestamp: u32) -> Self {
        Self {
            timestamp,
            annex_b: Vec::new(),
            packets: 0,
            keyframe: false,
            has_vcl: false,
            fragment: None,
        }
    }
}

impl Depacketizer {
    pub fn new(codec: Codec) -> Self {
        Self::with_limits(codec, Limits::default())
    }

    pub fn with_limits(codec: Codec, limits: Limits) -> Self {
        Self {
            codec,
            limits: Limits {
                max_reorder_packets: limits.max_reorder_packets.max(1),
                ..limits
            },
            window: BTreeMap::new(),
            window_bytes: 0,
            expected: None,
            last_timestamp: None,
            frame_boundary: true,
            assembly: None,
            geometry: None,
            stats: Stats::default(),
        }
    }

    /// Feeds one RTP packet. Complete access units are appended to `out`.
    ///
    /// Returns `Err` for a malformed payload. The packet still counts as lost,
    /// so the state is consistent either way.
    pub fn push(
        &mut self,
        packet: RtpPacketRef<'_>,
        out: &mut Vec<AccessUnit>,
    ) -> Result<(), PayloadError> {
        self.stats.packets += 1;
        let expected = *self
            .expected
            .get_or_insert(i64::from(packet.sequence_number));
        let seq = unwrap_seq(expected, packet.sequence_number);
        if seq < expected || self.window.contains_key(&seq) {
            self.stats.late_or_duplicate += 1;
            return Ok(());
        }
        let verdict = validate(self.codec, packet.payload).and({
            if packet.payload.len() > self.limits.max_reorder_bytes {
                Err(PayloadError::TooLarge)
            } else {
                Ok(())
            }
        });
        let size = if verdict.is_ok() {
            packet.payload.len()
        } else {
            0
        };
        // Making room may give up on gaps, which can release access units.
        self.make_room(size, out);
        if self.expected.is_some_and(|next| seq < next) {
            self.stats.late_or_duplicate += 1;
            return Ok(());
        }
        let slot = match verdict {
            Ok(()) => {
                self.window_bytes += size;
                Slot::Packet(Packet {
                    timestamp: packet.timestamp,
                    marker: packet.marker,
                    payload: packet.payload.to_vec(),
                })
            }
            Err(_) => {
                self.stats.malformed += 1;
                Slot::Lost
            }
        };
        self.window.insert(seq, slot);
        if self.expected == Some(seq) {
            self.drain(out);
        } else if is_newer(packet.timestamp, self.last_timestamp) {
            // A newer frame is waiting behind a gap. Give up on the gap, which
            // costs the frame that the gap belongs to.
            if let Some(&first) = self.window.keys().next() {
                self.skip_to(first, out);
            }
        }
        verdict
    }

    /// Gives up on every buffered gap and discards any incomplete access unit.
    /// Call this on a stall timer, not on every packet.
    pub fn flush(&mut self, out: &mut Vec<AccessUnit>) {
        while let Some(&first) = self.window.keys().next() {
            self.skip_to(first, out);
        }
        self.lose_au();
        self.frame_boundary = false;
    }

    /// Clears buffered state, geometry and sequence tracking. Counters are kept.
    pub fn reset(&mut self) {
        let stats = self.stats;
        *self = Self::with_limits(self.codec, self.limits);
        self.stats = stats;
    }

    /// Latest SPS geometry, or `None` before the first parsable SPS.
    #[cfg(test)]
    pub fn geometry(&self) -> Option<Geometry> {
        self.geometry
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Packets currently held in the reorder window.
    #[cfg(test)]
    pub fn waiting_packets(&self) -> usize {
        self.window.len()
    }

    fn drain(&mut self, out: &mut Vec<AccessUnit>) {
        while let Some(next) = self.expected {
            let Some(slot) = self.window.remove(&next) else {
                break;
            };
            self.expected = Some(next + 1);
            match slot {
                Slot::Lost => {
                    self.lose_au();
                    self.frame_boundary = false;
                }
                Slot::Packet(packet) => {
                    self.window_bytes -= packet.payload.len();
                    self.last_timestamp = Some(packet.timestamp);
                    self.assemble(&packet, out);
                    self.frame_boundary = packet.marker;
                }
            }
        }
    }

    fn assemble(&mut self, packet: &Packet, out: &mut Vec<AccessUnit>) {
        if self
            .assembly
            .as_ref()
            .is_some_and(|asm| asm.timestamp != packet.timestamp)
        {
            self.lose_au();
        }
        if self.assembly.is_none() {
            if !self.frame_boundary || !starts_access_unit(self.codec, &packet.payload) {
                self.stats.unstarted_dropped += 1;
                return;
            }
            self.assembly = Some(Assembly::new(packet.timestamp));
        }
        let Some(asm) = self.assembly.as_mut() else {
            return;
        };
        match append(
            self.codec,
            &self.limits,
            asm,
            &mut self.geometry,
            &packet.payload,
        ) {
            Ok(()) => {
                if packet.marker {
                    self.finish_au(out);
                }
            }
            Err(fault) => {
                self.lose_au();
                if fault == Fault::Oversized {
                    self.stats.oversized_discarded += 1;
                }
            }
        }
    }

    fn finish_au(&mut self, out: &mut Vec<AccessUnit>) {
        let Some(asm) = self.assembly.take() else {
            return;
        };
        if asm.fragment.is_some() || !asm.has_vcl {
            self.stats.partial_discarded += 1;
            return;
        }
        self.stats.access_units += 1;
        self.stats.keyframes += u64::from(asm.keyframe);
        out.push(AccessUnit {
            rtp_timestamp: asm.timestamp,
            keyframe: asm.keyframe,
            annex_b: asm.annex_b,
            geometry: self.geometry,
        });
    }

    fn lose_au(&mut self) {
        if self.assembly.take().is_some() {
            self.stats.partial_discarded += 1;
        }
    }

    /// Declares every sequence number before `next` lost, then drains from `next`.
    fn skip_to(&mut self, next: i64, out: &mut Vec<AccessUnit>) {
        if let Some(expected) = self.expected {
            self.stats.lost_packets += u64::try_from(next - expected).unwrap_or(0);
        }
        self.expected = Some(next);
        self.lose_au();
        self.frame_boundary = false;
        self.drain(out);
    }

    /// Gives up on the oldest gaps until one more packet of `size` bytes fits.
    fn make_room(&mut self, size: usize, out: &mut Vec<AccessUnit>) {
        while !self.window.is_empty()
            && (self.window.len() >= self.limits.max_reorder_packets
                || self.window_bytes + size > self.limits.max_reorder_bytes)
        {
            let Some(&first) = self.window.keys().next() else {
                break;
            };
            self.skip_to(first, out);
        }
    }
}

// ---- Payload validation ----------------------------------------------------

fn header_len(codec: Codec) -> usize {
    match codec {
        Codec::H264 => 1,
        Codec::H265 => 2,
    }
}

fn type_mask(codec: Codec) -> u8 {
    match codec {
        Codec::H264 => 0x1f,
        Codec::H265 => 0x3f,
    }
}

fn nal_type(codec: Codec, first: u8) -> u8 {
    match codec {
        Codec::H264 => first & 0x1f,
        Codec::H265 => (first >> 1) & 0x3f,
    }
}

fn aggregate_type(codec: Codec) -> u8 {
    match codec {
        Codec::H264 => 24,
        Codec::H265 => 48,
    }
}

fn fragment_type(codec: Codec) -> u8 {
    match codec {
        Codec::H264 => 28,
        Codec::H265 => 49,
    }
}

/// Single-NAL types that may appear in a single packet or inside an aggregate.
/// Aggregates and fragments are excluded, and so are unassigned types.
fn single_type_ok(codec: Codec, t: u8) -> bool {
    match codec {
        Codec::H264 => (1..=23).contains(&t),
        Codec::H265 => t <= 47,
    }
}

fn validate(codec: Codec, payload: &[u8]) -> Result<(), PayloadError> {
    let Some(&first) = payload.first() else {
        return Err(PayloadError::Empty);
    };
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(PayloadError::TooLarge);
    }
    if first & 0x80 != 0 {
        return Err(PayloadError::ForbiddenBit);
    }
    let hlen = header_len(codec);
    if payload.len() < hlen {
        return Err(PayloadError::Truncated);
    }
    let t = nal_type(codec, first);
    if t == aggregate_type(codec) {
        validate_aggregate(codec, &payload[hlen..])
    } else if t == fragment_type(codec) {
        validate_fragment(codec, payload)
    } else if single_type_ok(codec, t) {
        Ok(())
    } else {
        Err(PayloadError::UnsupportedType(t))
    }
}

fn validate_aggregate(codec: Codec, body: &[u8]) -> Result<(), PayloadError> {
    for nal in split_units(body)? {
        let Some(&first) = nal.first() else {
            return Err(PayloadError::BadFragment);
        };
        if first & 0x80 != 0 {
            return Err(PayloadError::ForbiddenBit);
        }
        if nal.len() < header_len(codec) || !single_type_ok(codec, nal_type(codec, first)) {
            return Err(PayloadError::BadFragment);
        }
    }
    Ok(())
}

fn validate_fragment(codec: Codec, payload: &[u8]) -> Result<(), PayloadError> {
    let hlen = header_len(codec);
    if payload.len() <= hlen + 1 {
        return Err(PayloadError::Truncated);
    }
    let fu = payload[hlen];
    if fu & 0x80 != 0 && fu & 0x40 != 0 {
        return Err(PayloadError::BadFragment);
    }
    if !single_type_ok(codec, fu & type_mask(codec)) {
        return Err(PayloadError::BadFragment);
    }
    Ok(())
}

/// Splits `[u16 size][NAL]` units. The body must end exactly on a unit boundary.
fn split_units(mut body: &[u8]) -> Result<Vec<&[u8]>, PayloadError> {
    let mut units = Vec::new();
    while !body.is_empty() {
        let [hi, lo, rest @ ..] = body else {
            return Err(PayloadError::Truncated);
        };
        let size = usize::from(u16::from_be_bytes([*hi, *lo]));
        if size == 0 {
            return Err(PayloadError::BadFragment);
        }
        if rest.len() < size {
            return Err(PayloadError::Truncated);
        }
        units.push(&rest[..size]);
        body = &rest[size..];
    }
    if units.is_empty() {
        return Err(PayloadError::Truncated);
    }
    Ok(units)
}

// ---- Access unit assembly --------------------------------------------------

/// What a complete NAL unit contributes to an access unit.
#[derive(Debug, Clone, Copy, Default)]
struct NalInfo {
    vcl: bool,
    keyframe: bool,
    sps: bool,
    /// True when the NAL may begin an access unit.
    starts: bool,
}

/// Classifies a NAL from its first byte and the first byte after its header.
/// The first slice-segment bit is the top bit of that byte (`first_mb_in_slice == 0`
/// for H.264, `first_slice_segment_in_pic_flag` for H.265).
fn classify(codec: Codec, first: u8, body0: Option<u8>) -> NalInfo {
    let first_slice = body0.is_some_and(|b| b & 0x80 != 0);
    let t = nal_type(codec, first);
    match codec {
        Codec::H264 => NalInfo {
            vcl: (1..=5).contains(&t),
            keyframe: t == 5,
            sps: t == 7,
            starts: matches!(t, 6..=9) || (matches!(t, 1 | 5) && first_slice),
        },
        Codec::H265 => {
            let vcl = t <= 9 || (16..=21).contains(&t);
            NalInfo {
                vcl,
                keyframe: (16..=21).contains(&t),
                sps: t == 33,
                // VPS, SPS, PPS, AUD, prefix SEI, or a first slice segment.
                starts: matches!(t, 32..=35 | 39) || (vcl && first_slice),
            }
        }
    }
}

fn nal_info(codec: Codec, nal: &[u8]) -> NalInfo {
    classify(codec, nal[0], nal.get(header_len(codec)).copied())
}

/// True when `payload` begins a new access unit.
fn starts_access_unit(codec: Codec, payload: &[u8]) -> bool {
    let t = nal_type(codec, payload[0]);
    if t == aggregate_type(codec) {
        split_units(&payload[header_len(codec)..])
            .ok()
            .and_then(|units| units.first().copied())
            .is_some_and(|nal| nal_info(codec, nal).starts)
    } else if t == fragment_type(codec) {
        let fu = fu_head(codec, payload);
        fu.start && classify(codec, fu.header[0], Some(payload[header_len(codec) + 1])).starts
    } else {
        nal_info(codec, payload).starts
    }
}

/// Reconstructed NAL header of a fragment, plus its FU flags.
struct FuHead {
    header: [u8; 2],
    nal_type: u8,
    start: bool,
    end: bool,
}

/// Reads the FU header of a validated fragment payload.
fn fu_head(codec: Codec, payload: &[u8]) -> FuHead {
    let fu = payload[header_len(codec)];
    let nal_type = fu & type_mask(codec);
    let header = match codec {
        Codec::H264 => [(payload[0] & 0xe0) | nal_type, 0],
        Codec::H265 => [(payload[0] & 0x81) | (nal_type << 1), payload[1]],
    };
    FuHead {
        header,
        nal_type,
        start: fu & 0x80 != 0,
        end: fu & 0x40 != 0,
    }
}

/// Appends one validated payload to the open access unit.
fn append(
    codec: Codec,
    limits: &Limits,
    asm: &mut Assembly,
    geometry: &mut Option<Geometry>,
    payload: &[u8],
) -> Result<(), Fault> {
    asm.packets += 1;
    if asm.packets > limits.max_access_unit_packets {
        return Err(Fault::Oversized);
    }
    let t = nal_type(codec, payload[0]);
    if t == aggregate_type(codec) {
        let units = split_units(&payload[header_len(codec)..]).map_err(|_| Fault::Fragment)?;
        for nal in units {
            push_nal(codec, limits, asm, geometry, nal)?;
        }
        Ok(())
    } else if t == fragment_type(codec) {
        push_fragment(codec, limits, asm, geometry, payload)
    } else {
        push_nal(codec, limits, asm, geometry, payload)
    }
}

fn push_nal(
    codec: Codec,
    limits: &Limits,
    asm: &mut Assembly,
    geometry: &mut Option<Geometry>,
    nal: &[u8],
) -> Result<(), Fault> {
    if asm.fragment.is_some() {
        return Err(Fault::Fragment);
    }
    if asm.annex_b.len() + START_CODE.len() + nal.len() > limits.max_access_unit_bytes {
        return Err(Fault::Oversized);
    }
    let info = nal_info(codec, nal);
    let sps = if info.sps {
        parse_sps(codec, nal)
    } else {
        None
    };
    asm.annex_b.extend_from_slice(&START_CODE);
    asm.annex_b.extend_from_slice(nal);
    record(asm, geometry, info, sps);
    Ok(())
}

fn push_fragment(
    codec: Codec,
    limits: &Limits,
    asm: &mut Assembly,
    geometry: &mut Option<Geometry>,
    payload: &[u8],
) -> Result<(), Fault> {
    let fu = fu_head(codec, payload);
    let hlen = header_len(codec);
    let data = &payload[hlen + 1..];
    match asm.fragment.as_ref() {
        None if !fu.start => return Err(Fault::Fragment),
        Some(open) if fu.start || open.nal_type != fu.nal_type => return Err(Fault::Fragment),
        _ => {}
    }
    let extra = data.len() + if fu.start { START_CODE.len() + hlen } else { 0 };
    if asm.annex_b.len() + extra > limits.max_access_unit_bytes {
        return Err(Fault::Oversized);
    }
    if fu.start {
        let info = classify(codec, fu.header[0], Some(data[0]));
        asm.fragment = Some(Fragment {
            start: asm.annex_b.len(),
            nal_type: fu.nal_type,
            info,
        });
        asm.annex_b.extend_from_slice(&START_CODE);
        asm.annex_b.extend_from_slice(&fu.header[..hlen]);
    }
    asm.annex_b.extend_from_slice(data);
    if fu.end {
        finish_fragment(codec, geometry, asm);
    }
    Ok(())
}

/// Closes the open fragment. Its NAL is complete and is recorded.
fn finish_fragment(codec: Codec, geometry: &mut Option<Geometry>, asm: &mut Assembly) {
    let Some(frag) = asm.fragment.take() else {
        return;
    };
    let nal = &asm.annex_b[frag.start + START_CODE.len()..];
    let sps = if frag.info.sps {
        parse_sps(codec, nal)
    } else {
        None
    };
    record(asm, geometry, frag.info, sps);
}

fn record(
    asm: &mut Assembly,
    geometry: &mut Option<Geometry>,
    info: NalInfo,
    sps: Option<Geometry>,
) {
    asm.keyframe |= info.keyframe;
    asm.has_vcl |= info.vcl;
    if let Some(g) = sps {
        *geometry = Some(g);
    }
}

/// Serial-number comparison for RTP timestamps: true when `timestamp` is
/// newer than `last`. Any timestamp is newer than none.
fn is_newer(timestamp: u32, last: Option<u32>) -> bool {
    last.is_none_or(|last| timestamp != last && (timestamp.wrapping_sub(last) as i32) > 0)
}

/// Extends a 16-bit sequence number to the 64-bit value nearest to `expected`.
fn unwrap_seq(expected: i64, seq: u16) -> i64 {
    expected + i64::from(seq.wrapping_sub(expected as u16) as i16)
}

// ---- SPS geometry ----------------------------------------------------------

/// Parses the picture size from a complete SPS NAL unit (header included).
fn parse_sps(codec: Codec, nal: &[u8]) -> Option<Geometry> {
    let rbsp = unescape(nal.get(header_len(codec)..)?);
    match codec {
        Codec::H264 => parse_h264_sps(&rbsp),
        Codec::H265 => parse_h265_sps(&rbsp),
    }
}

fn parse_h264_sps(rbsp: &[u8]) -> Option<Geometry> {
    let mut b = Bits::new(rbsp);
    let profile = b.bits(8)?;
    b.skip(16)?; // constraint flags and level_idc
    b.ue()?; // seq_parameter_set_id
    let mut chroma = 1;
    let mut separate_planes = false;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma = b.ue()?;
        if chroma > 3 {
            return None;
        }
        if chroma == 3 {
            separate_planes = b.bit()?;
        }
        b.ue()?; // bit_depth_luma_minus8
        b.ue()?; // bit_depth_chroma_minus8
        b.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if b.bit()? {
            let lists = if chroma != 3 { 8 } else { 12 };
            for i in 0..lists {
                if b.bit()? {
                    skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    b.ue()?; // log2_max_frame_num_minus4
    match b.ue()? {
        0 => {
            b.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            b.skip(1)?;
            b.se()?;
            b.se()?;
            let cycle = b.ue()?;
            if cycle > 255 {
                return None;
            }
            for _ in 0..cycle {
                b.se()?;
            }
        }
        2 => {}
        _ => return None,
    }
    b.ue()?; // max_num_ref_frames
    b.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = b.ue()? + 1;
    let height_map_units = b.ue()? + 1;
    let frame_mbs_only = b.bit()?;
    if !frame_mbs_only {
        b.skip(1)?; // mb_adaptive_frame_field_flag
    }
    b.skip(1)?; // direct_8x8_inference_flag
    let (mut left, mut right, mut top, mut bottom) = (0, 0, 0, 0);
    if b.bit()? {
        left = b.ue()?;
        right = b.ue()?;
        top = b.ue()?;
        bottom = b.ue()?;
    }
    let frame_factor: u64 = if frame_mbs_only { 1 } else { 2 };
    // ChromaArrayType 0 (monochrome or separate planes) uses unit 1.
    let chroma_array = if separate_planes { 0 } else { chroma };
    let (sub_w, sub_h) = match chroma_array {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let coded_w = width_mbs * 16;
    let coded_h = height_map_units * 16 * frame_factor;
    let crop_w = sub_w * (left + right);
    let crop_h = sub_h * frame_factor * (top + bottom);
    geometry_from(coded_w, coded_h, crop_w, crop_h)
}

fn parse_h265_sps(rbsp: &[u8]) -> Option<Geometry> {
    let mut b = Bits::new(rbsp);
    b.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = b.bits(3)?;
    b.skip(1)?; // sps_temporal_id_nesting_flag
    skip_profile_tier_level(&mut b, max_sub_layers_minus1)?;
    b.ue()?; // sps_seq_parameter_set_id
    let chroma = b.ue()?;
    if chroma > 3 {
        return None;
    }
    if chroma == 3 {
        b.skip(1)?; // separate_colour_plane_flag
    }
    let width = b.ue()?;
    let height = b.ue()?;
    let (mut left, mut right, mut top, mut bottom) = (0, 0, 0, 0);
    if b.bit()? {
        left = b.ue()?;
        right = b.ue()?;
        top = b.ue()?;
        bottom = b.ue()?;
    }
    let (sub_w, sub_h) = match chroma {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    geometry_from(
        width,
        height,
        sub_w * (left + right),
        sub_h * (top + bottom),
    )
}

/// Skips profile_tier_level(1, max_sub_layers_minus1), keeping only the bit position.
fn skip_profile_tier_level(b: &mut Bits<'_>, max_sub_layers_minus1: u64) -> Option<()> {
    if max_sub_layers_minus1 > 6 {
        return None;
    }
    b.skip(88)?; // general profile: space, tier, idc, compatibility, source and constraint flags
    b.skip(8)?; // general_level_idc
    let mut present = [(false, false); 7];
    for flags in present.iter_mut().take(max_sub_layers_minus1 as usize) {
        *flags = (b.bit()?, b.bit()?);
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            b.skip(2)?; // reserved_zero_2bits
        }
    }
    for (profile, level) in present.iter().take(max_sub_layers_minus1 as usize) {
        if *profile {
            b.skip(88)?;
        }
        if *level {
            b.skip(8)?;
        }
    }
    Some(())
}

fn skip_scaling_list(b: &mut Bits<'_>, size: usize) -> Option<()> {
    let mut last: i64 = 8;
    let mut next: i64 = 8;
    for _ in 0..size {
        if next != 0 {
            let delta = b.se()?;
            next = (last + delta + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// Removes emulation prevention bytes (`00 00 03` becomes `00 00`).
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Validates the SPS dimensions. `crop_*` are total luma-sample crops.
fn geometry_from(coded_w: u64, coded_h: u64, crop_w: u64, crop_h: u64) -> Option<Geometry> {
    if coded_w == 0 || coded_h == 0 || coded_w > MAX_DIMENSION || coded_h > MAX_DIMENSION {
        return None;
    }
    let display_w = coded_w.checked_sub(crop_w).filter(|&w| w > 0)?;
    let display_h = coded_h.checked_sub(crop_h).filter(|&h| h > 0)?;
    Some(Geometry {
        coded_width: coded_w as u32,
        coded_height: coded_h as u32,
        display_width: display_w as u32,
        display_height: display_h as u32,
    })
}

/// MSB-first bit reader with exp-Golomb support. Every read is bounds-checked.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = ((byte >> (7 - self.pos % 8)) & 1) == 1;
        self.pos += 1;
        Some(bit)
    }

    fn bits(&mut self, n: u32) -> Option<u64> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bit()?);
        }
        Some(v)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos.checked_add(n)? > self.data.len() * 8 {
            return None;
        }
        self.pos += n;
        Some(())
    }

    fn ue(&mut self) -> Option<u64> {
        let mut zeros = 0u32;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u64 << zeros) - 1 + self.bits(zeros)?)
    }

    fn se(&mut self) -> Option<i64> {
        let k = i64::try_from(self.ue()?).ok()?;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1920x1080 High profile SPS (coded 1920x1088, cropped to 1080).
    const SPS_1080: [u8; 27] = [
        0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00,
        0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58,
    ];
    const PPS: [u8; 4] = [0x68, 0xCE, 0x3C, 0x80];
    /// IDR slice whose first slice bit is set (first_mb_in_slice == 0).
    const IDR: [u8; 4] = [0x65, 0x88, 0x84, 0x00];
    /// Non-IDR slice starting a picture.
    const SLICE: [u8; 3] = [0x41, 0x9A, 0x21];
    /// FU-A fragments of the IDR slice `65 88 84 00 11 22 33`.
    const FU_START: [u8; 4] = [0x7C, 0x85, 0x88, 0x84];
    const FU_MIDDLE: [u8; 4] = [0x7C, 0x05, 0x00, 0x11];
    const FU_END: [u8; 4] = [0x7C, 0x45, 0x22, 0x33];

    fn pkt(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> RtpPacketRef<'_> {
        RtpPacketRef {
            sequence_number: seq,
            timestamp: ts,
            marker,
            payload,
        }
    }

    fn annex(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| START_CODE.iter().chain(n.iter()).copied())
            .collect()
    }

    fn feed(d: &mut Depacketizer, packets: &[(u16, u32, bool, &[u8])]) -> Vec<AccessUnit> {
        let mut out = Vec::new();
        for &(seq, ts, marker, payload) in packets {
            d.push(pkt(seq, ts, marker, payload), &mut out).unwrap();
        }
        out
    }

    fn escape(rbsp: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut zeros = 0;
        for &b in rbsp {
            if zeros >= 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        out
    }

    struct BitWriter {
        out: Vec<u8>,
        bits: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                out: Vec::new(),
                bits: 0,
            }
        }

        fn put(&mut self, value: u64, n: u32) {
            for i in (0..n).rev() {
                if self.bits.is_multiple_of(8) {
                    self.out.push(0);
                }
                if ((value >> i) & 1) == 1 {
                    let last = self.out.len() - 1;
                    self.out[last] |= 0x80 >> (self.bits % 8);
                }
                self.bits += 1;
            }
        }

        fn ue(&mut self, v: u64) {
            let code = v + 1;
            let len = 64 - code.leading_zeros();
            self.put(0, len - 1);
            self.put(code, len);
        }

        fn finish(mut self) -> Vec<u8> {
            self.put(1, 1);
            while !self.bits.is_multiple_of(8) {
                self.put(0, 1);
            }
            self.out
        }
    }

    /// H.265 SPS NAL (header included) with 4:2:0 and a bottom crop of `crop_bottom` units.
    fn h265_sps(width: u64, height: u64, crop_bottom: u64) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(0, 4); // vps id
        w.put(0, 3); // max_sub_layers_minus1
        w.put(1, 1); // temporal_id_nesting
        w.put(0, 2); // profile space
        w.put(0, 1); // tier
        w.put(1, 5); // profile idc
        w.put(0x6000_0000, 32); // compatibility flags
        w.put(0, 4); // source and frame-only flags
        w.put(0, 43); // constraint flags and reserved bits
        w.put(0, 1); // inbld / reserved
        w.put(120, 8); // general_level_idc
        w.ue(0); // sps id
        w.ue(1); // chroma_format_idc 4:2:0
        w.ue(width);
        w.ue(height);
        if crop_bottom > 0 {
            w.put(1, 1);
            w.ue(0);
            w.ue(0);
            w.ue(0);
            w.ue(crop_bottom);
        } else {
            w.put(0, 1);
        }
        let mut nal = vec![0x42, 0x01];
        nal.extend(escape(&w.finish()));
        nal
    }

    fn geometry_1080() -> Geometry {
        Geometry {
            coded_width: 1920,
            coded_height: 1088,
            display_width: 1920,
            display_height: 1080,
        }
    }

    #[test]
    fn h264_single_nals_release_one_keyframe_at_marker() {
        let mut d = Depacketizer::new(Codec::H264);
        let out = feed(
            &mut d,
            &[
                (1, 1000, false, &SPS_1080[..]),
                (2, 1000, false, &PPS[..]),
                (3, 1000, true, &IDR[..]),
            ],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rtp_timestamp, 1000);
        assert!(out[0].keyframe);
        assert_eq!(out[0].annex_b, annex(&[&SPS_1080[..], &PPS[..], &IDR[..]]));
        assert_eq!(out[0].geometry, Some(geometry_1080()));
        assert_eq!(d.geometry(), Some(geometry_1080()));
        assert_eq!(d.stats().access_units, 1);
        assert_eq!(d.stats().keyframes, 1);
    }

    #[test]
    fn h264_non_idr_slice_is_not_a_keyframe_and_has_no_geometry_before_sps() {
        let mut d = Depacketizer::new(Codec::H264);
        let out = feed(&mut d, &[(1, 7, true, &SLICE[..])]);
        assert_eq!(out.len(), 1);
        assert!(!out[0].keyframe);
        assert_eq!(out[0].geometry, None);
        assert_eq!(out[0].annex_b, annex(&[&SLICE[..]]));
    }

    #[test]
    fn h264_stap_a_aggregate_releases_one_access_unit() {
        let mut stap = vec![0x18];
        for nal in [&SPS_1080[..], &PPS[..], &IDR[..]] {
            stap.extend_from_slice(&u16::try_from(nal.len()).unwrap().to_be_bytes());
            stap.extend_from_slice(nal);
        }
        let mut d = Depacketizer::new(Codec::H264);
        let out = feed(&mut d, &[(1, 5, true, &stap[..])]);
        assert_eq!(out.len(), 1);
        assert!(out[0].keyframe);
        assert_eq!(out[0].annex_b, annex(&[&SPS_1080[..], &PPS[..], &IDR[..]]));
        assert_eq!(out[0].geometry, Some(geometry_1080()));
    }

    #[test]
    fn h264_fu_a_fragments_reassemble_into_one_nal() {
        let mut d = Depacketizer::new(Codec::H264);
        let out = feed(
            &mut d,
            &[
                (10, 7, false, &FU_START[..]),
                (11, 7, false, &FU_MIDDLE[..]),
                (12, 7, true, &FU_END[..]),
            ],
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].keyframe);
        assert_eq!(
            out[0].annex_b,
            annex(&[&[0x65, 0x88, 0x84, 0x00, 0x11, 0x22, 0x33]])
        );
    }

    #[test]
    fn h264_fu_a_reordered_middle_and_end_still_reassembles() {
        let mut d = Depacketizer::new(Codec::H264);
        let mut out = feed(
            &mut d,
            &[(20, 7, false, &FU_START[..]), (22, 7, true, &FU_END[..])],
        );
        assert!(out.is_empty());
        d.push(pkt(21, 7, false, &FU_MIDDLE[..]), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].annex_b,
            annex(&[&[0x65, 0x88, 0x84, 0x00, 0x11, 0x22, 0x33]])
        );
        assert_eq!(d.stats().lost_packets, 0);
    }

    #[test]
    fn h264_loss_inside_access_unit_is_never_released() {
        let mut d = Depacketizer::new(Codec::H264);
        // 31 is lost. 32 has the same timestamp, so the window waits for it.
        let mut out = feed(
            &mut d,
            &[(30, 9, false, &FU_START[..]), (32, 9, true, &FU_END[..])],
        );
        assert!(out.is_empty());
        d.flush(&mut out);
        assert!(out.is_empty());
        let stats = d.stats();
        assert_eq!(stats.lost_packets, 1);
        assert_eq!(stats.partial_discarded, 1);
        assert_eq!(stats.unstarted_dropped, 1);
        assert_eq!(stats.access_units, 0);
    }

    #[test]
    fn newer_timestamp_gives_up_gap_and_next_frame_recovers() {
        let mut d = Depacketizer::new(Codec::H264);
        let out = feed(
            &mut d,
            &[
                (30, 200, true, &IDR[..]),
                // 31 and 32 are lost; 33 is the first packet of a newer frame.
                (33, 300, true, &SLICE[..]),
                (34, 400, true, &SLICE[..]),
            ],
        );
        // The frame at 300 lost its head, so it is dropped. The frame at 400 is released.
        let timestamps: Vec<u32> = out.iter().map(|au| au.rtp_timestamp).collect();
        assert_eq!(timestamps, vec![200, 400]);
        assert_eq!(d.stats().lost_packets, 2);
        assert_eq!(d.stats().unstarted_dropped, 1);
    }

    #[test]
    fn frame_boundary_reorder_costs_one_frame() {
        // Documented trade-off: a newer frame that overtakes the last packet
        // of the previous frame gives up the gap. The late packet is then dropped.
        let mut d = Depacketizer::new(Codec::H264);
        let mut out = feed(&mut d, &[(39, 100, true, &SLICE[..])]);
        assert_eq!(out.len(), 1);
        out.clear();
        let mut out2 = Vec::new();
        d.push(pkt(41, 200, true, &SLICE[..]), &mut out2).unwrap();
        d.push(pkt(40, 200, true, &SLICE[..]), &mut out2).unwrap();
        assert!(out2.is_empty());
        assert_eq!(d.stats().lost_packets, 1);
        assert_eq!(d.stats().late_or_duplicate, 1);
    }

    #[test]
    fn sequence_numbers_wrap_without_loss() {
        let mut d = Depacketizer::new(Codec::H264);
        let out = feed(
            &mut d,
            &[
                (65534, 1, true, &SLICE[..]),
                (65535, 2, true, &SLICE[..]),
                (0, 3, true, &SLICE[..]),
                (1, 4, true, &SLICE[..]),
            ],
        );
        assert_eq!(out.len(), 4);
        assert_eq!(d.stats().lost_packets, 0);
        assert_eq!(d.stats().late_or_duplicate, 0);
    }

    #[test]
    fn timestamp_comparison_is_serial() {
        assert!(is_newer(0, Some(0xFFFF_FFF0)));
        assert!(!is_newer(0xFFFF_FFF0, Some(0)));
        assert!(!is_newer(5, Some(5)));
        assert!(is_newer(7, None));
        assert!(is_newer(3000, Some(0)));
    }

    #[test]
    fn duplicate_and_late_packets_are_counted_and_ignored() {
        let mut d = Depacketizer::new(Codec::H264);
        let out = feed(
            &mut d,
            &[
                (1, 1, true, &SLICE[..]),
                (1, 1, true, &SLICE[..]),
                (0, 1, true, &SLICE[..]),
            ],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(d.stats().late_or_duplicate, 2);
    }

    #[test]
    fn malformed_packet_breaks_the_access_unit_it_belongs_to() {
        let mut d = Depacketizer::new(Codec::H264);
        let mut out = Vec::new();
        d.push(pkt(50, 700, false, &FU_START[..]), &mut out)
            .unwrap();
        assert_eq!(
            d.push(pkt(51, 700, false, &[]), &mut out),
            Err(PayloadError::Empty)
        );
        d.push(pkt(52, 700, true, &FU_END[..]), &mut out).unwrap();
        assert!(out.is_empty());
        let stats = d.stats();
        assert_eq!(stats.malformed, 1);
        assert_eq!(stats.partial_discarded, 1);
        assert_eq!(stats.access_units, 0);
    }

    #[test]
    fn invalid_h264_payloads_are_rejected_with_a_reason() {
        let mut d = Depacketizer::new(Codec::H264);
        let mut out = Vec::new();
        let cases: [(&[u8], PayloadError); 8] = [
            (&[], PayloadError::Empty),
            (&[0x19, 0x00, 0x01, 0x65], PayloadError::UnsupportedType(25)),
            (&[0x00], PayloadError::UnsupportedType(0)),
            (&[0xC1, 0x9A], PayloadError::ForbiddenBit),
            (&[0x7C, 0xC5, 0x88], PayloadError::BadFragment),
            (&[0x7C, 0x85], PayloadError::Truncated),
            (
                &[0x18, 0x00, 0x00, 0x00, 0x01, 0x65],
                PayloadError::BadFragment,
            ),
            (&[0x18, 0x00, 0x05, 0x65], PayloadError::Truncated),
        ];
        for (i, (payload, expected)) in cases.into_iter().enumerate() {
            let seq = u16::try_from(i + 1).unwrap();
            assert_eq!(d.push(pkt(seq, 1, false, payload), &mut out), Err(expected));
        }
        // A nested aggregate is refused.
        assert_eq!(
            d.push(pkt(9, 1, false, &[0x18, 0x00, 0x02, 0x18, 0x00]), &mut out),
            Err(PayloadError::BadFragment)
        );
        assert!(out.is_empty());
        assert_eq!(d.stats().malformed, 9);
    }

    #[test]
    fn invalid_h265_payloads_are_rejected_with_a_reason() {
        let mut d = Depacketizer::new(Codec::H265);
        let mut out = Vec::new();
        assert_eq!(
            d.push(pkt(1, 1, false, &[0x64, 0x01, 0x00]), &mut out),
            Err(PayloadError::UnsupportedType(50))
        );
        assert_eq!(
            d.push(pkt(2, 1, false, &[0x7E, 0x01, 0x00]), &mut out),
            Err(PayloadError::UnsupportedType(63))
        );
        assert_eq!(
            d.push(pkt(3, 1, false, &[0x62, 0x01, 0xD3, 0x00]), &mut out),
            Err(PayloadError::BadFragment)
        );
        assert_eq!(
            d.push(pkt(4, 1, false, &[0x26]), &mut out),
            Err(PayloadError::Truncated)
        );
        assert_eq!(
            d.push(pkt(5, 1, false, &[0x60, 0x01, 0x00, 0x09, 0x42]), &mut out),
            Err(PayloadError::Truncated)
        );
        assert_eq!(
            d.push(
                pkt(6, 1, false, &[0x60, 0x01, 0x00, 0x03, 0x62, 0x01, 0x13]),
                &mut out
            ),
            Err(PayloadError::BadFragment)
        );
        assert_eq!(
            d.push(pkt(7, 1, false, &[0x80, 0x01]), &mut out),
            Err(PayloadError::ForbiddenBit)
        );
        assert!(out.is_empty());
    }

    #[test]
    fn fragment_continuation_without_start_is_dropped() {
        let mut d = Depacketizer::new(Codec::H264);
        let out = feed(
            &mut d,
            &[(1, 9, false, &FU_MIDDLE[..]), (2, 9, true, &FU_END[..])],
        );
        assert!(out.is_empty());
        assert_eq!(d.stats().unstarted_dropped, 2);
    }

    #[test]
    fn oversized_access_unit_is_discarded_whole() {
        let limits = Limits {
            max_access_unit_bytes: 20,
            ..Limits::default()
        };
        let mut d = Depacketizer::with_limits(Codec::H264, limits);
        let out = feed(&mut d, &[(1, 1, true, &SPS_1080[..])]);
        assert!(out.is_empty());
        assert_eq!(d.stats().oversized_discarded, 1);
        assert_eq!(d.geometry(), None);
    }

    #[test]
    fn access_unit_packet_limit_discards_the_unit() {
        let limits = Limits {
            max_access_unit_packets: 2,
            ..Limits::default()
        };
        let mut d = Depacketizer::with_limits(Codec::H264, limits);
        let out = feed(
            &mut d,
            &[
                (1, 1, false, &FU_START[..]),
                (2, 1, false, &FU_MIDDLE[..]),
                (3, 1, true, &FU_END[..]),
            ],
        );
        assert!(out.is_empty());
        assert_eq!(d.stats().oversized_discarded, 1);
        assert_eq!(d.stats().access_units, 0);
    }

    #[test]
    fn reorder_window_stays_within_its_packet_limit() {
        let limits = Limits {
            max_reorder_packets: 2,
            ..Limits::default()
        };
        let mut d = Depacketizer::with_limits(Codec::H264, limits);
        let mut out = Vec::new();
        d.push(pkt(1, 1, true, &SLICE[..]), &mut out).unwrap();
        // Sequence 2 never arrives. Same timestamp, so the gap is not given up on yet.
        for seq in 3..=8 {
            d.push(pkt(seq, 1, false, &SLICE[..]), &mut out).unwrap();
            assert!(d.waiting_packets() <= 2);
        }
        assert!(d.stats().lost_packets >= 1);
    }

    #[test]
    fn reset_clears_state_but_keeps_counters() {
        let mut d = Depacketizer::new(Codec::H264);
        let out = feed(
            &mut d,
            &[(1, 1, false, &SPS_1080[..]), (2, 1, true, &IDR[..])],
        );
        assert_eq!(out.len(), 1);
        assert!(d.geometry().is_some());
        d.reset();
        assert_eq!(d.geometry(), None);
        assert_eq!(d.waiting_packets(), 0);
        assert_eq!(d.stats().access_units, 1);
        let out = feed(&mut d, &[(100, 5, true, &SLICE[..])]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rtp_timestamp, 5);
    }

    #[test]
    fn h264_sps_parses_coded_and_display_size() {
        assert_eq!(parse_sps(Codec::H264, &SPS_1080[..]), Some(geometry_1080()));
        // Truncated SPS bodies must fail without panicking.
        assert_eq!(parse_sps(Codec::H264, &SPS_1080[..4]), None);
        for n in 0..SPS_1080.len() {
            let _ = parse_sps(Codec::H264, &SPS_1080[..n]);
        }
    }

    #[test]
    fn h265_sps_parses_coded_and_display_size() {
        let sps = h265_sps(1920, 1088, 4);
        assert_eq!(parse_sps(Codec::H265, &sps[..]), Some(geometry_1080()));
        let uncropped = h265_sps(1280, 720, 0);
        assert_eq!(
            parse_sps(Codec::H265, &uncropped[..]),
            Some(Geometry {
                coded_width: 1280,
                coded_height: 720,
                display_width: 1280,
                display_height: 720,
            })
        );
    }

    #[test]
    fn geometry_rejects_zero_oversized_and_over_cropped_values() {
        assert_eq!(geometry_from(0, 1088, 0, 0), None);
        assert_eq!(geometry_from(16_400, 1088, 0, 0), None);
        assert_eq!(geometry_from(1920, 1088, 1920, 0), None);
        assert_eq!(geometry_from(1920, 1088, 0, 1088), None);
        assert_eq!(
            geometry_from(16_384, 16_384, 0, 0),
            Some(Geometry {
                coded_width: 16_384,
                coded_height: 16_384,
                display_width: 16_384,
                display_height: 16_384,
            })
        );
    }

    #[test]
    fn h265_access_unit_from_parameter_sets_and_irap_slice() {
        let sps = h265_sps(1920, 1088, 4);
        let vps = [0x40, 0x01, 0x0C];
        let pps = [0x44, 0x01, 0xC1];
        let idr = [0x26, 0x01, 0x80, 0x11];
        let mut d = Depacketizer::new(Codec::H265);
        let out = feed(
            &mut d,
            &[
                (1, 100, false, &vps[..]),
                (2, 100, false, &sps[..]),
                (3, 100, false, &pps[..]),
                (4, 100, true, &idr[..]),
            ],
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].keyframe);
        assert_eq!(
            out[0].annex_b,
            annex(&[&vps[..], &sps[..], &pps[..], &idr[..]])
        );
        assert_eq!(out[0].geometry, Some(geometry_1080()));
    }

    #[test]
    fn h265_trailing_slice_is_not_a_keyframe() {
        let mut d = Depacketizer::new(Codec::H265);
        let out = feed(&mut d, &[(1, 1, true, &[0x02, 0x01, 0x80, 0x00][..])]);
        assert_eq!(out.len(), 1);
        assert!(!out[0].keyframe);
    }

    #[test]
    fn h265_aggregation_packet_releases_its_units() {
        let sps = h265_sps(1920, 1088, 4);
        let idr = [0x26, 0x01, 0x80, 0x11];
        let mut ap = vec![0x60, 0x01];
        for nal in [&sps[..], &idr[..]] {
            ap.extend_from_slice(&u16::try_from(nal.len()).unwrap().to_be_bytes());
            ap.extend_from_slice(nal);
        }
        let mut d = Depacketizer::new(Codec::H265);
        let out = feed(&mut d, &[(1, 8, true, &ap[..])]);
        assert_eq!(out.len(), 1);
        assert!(out[0].keyframe);
        assert_eq!(out[0].annex_b, annex(&[&sps[..], &idr[..]]));
        assert_eq!(out[0].geometry, Some(geometry_1080()));
    }

    #[test]
    fn h265_fu_reconstructs_the_nal_header() {
        // FU payload header 62 01 (type 49), FU header S|IDR_W_RADL, then E|IDR_W_RADL.
        let start = [0x62, 0x01, 0x93, 0x80, 0x11];
        let middle = [0x62, 0x01, 0x13, 0x22];
        let end = [0x62, 0x01, 0x53, 0x33];
        let mut d = Depacketizer::new(Codec::H265);
        let out = feed(
            &mut d,
            &[
                (1, 9, false, &start[..]),
                (2, 9, false, &middle[..]),
                (3, 9, true, &end[..]),
            ],
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].keyframe);
        assert_eq!(
            out[0].annex_b,
            annex(&[&[0x26, 0x01, 0x80, 0x11, 0x22, 0x33]])
        );
    }

    #[test]
    fn random_packets_never_panic_and_released_units_are_well_formed() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) as u32
        };
        for codec in [Codec::H264, Codec::H265] {
            let limits = Limits {
                max_access_unit_bytes: 1 << 14,
                max_access_unit_packets: 32,
                max_reorder_packets: 8,
                max_reorder_bytes: 4096,
            };
            let first_bytes: [u8; 8] = match codec {
                Codec::H264 => [0x65, 0x41, 0x7C, 0x7C, 0x18, 0x67, 0x68, 0x19],
                Codec::H265 => [0x26, 0x02, 0x62, 0x62, 0x60, 0x42, 0x44, 0x64],
            };
            let mut d = Depacketizer::with_limits(codec, limits);
            let mut seq = 0u16;
            let mut ts = 0u32;
            let mut released = 0u64;
            let mut out = Vec::new();
            for _ in 0..20_000 {
                seq = if next() % 8 == 0 {
                    seq.wrapping_add(2)
                } else {
                    seq.wrapping_add(1)
                };
                if next() % 4 == 0 {
                    ts = ts.wrapping_add(3000);
                }
                let len = (next() % 48) as usize;
                let mut payload: Vec<u8> = (0..len).map(|_| next() as u8).collect();
                if next() % 2 == 0 {
                    if let Some(first) = payload.first_mut() {
                        *first = first_bytes[(next() % 8) as usize];
                    }
                }
                let marker = next() % 3 == 0;
                let _ = d.push(pkt(seq, ts, marker, &payload[..]), &mut out);
                assert!(d.waiting_packets() <= limits.max_reorder_packets);
                if next() % 500 == 0 {
                    d.flush(&mut out);
                }
                for au in out.drain(..) {
                    released += 1;
                    assert!(au.annex_b.starts_with(&START_CODE));
                    assert!(au.annex_b.len() <= limits.max_access_unit_bytes);
                    // No empty NAL: two start codes back to back.
                    assert!(!au
                        .annex_b
                        .windows(8)
                        .any(|w| w[..4] == START_CODE && w[4..] == START_CODE));
                }
            }
            d.flush(&mut out);
            for au in out.drain(..) {
                released += 1;
                assert!(au.annex_b.starts_with(&START_CODE));
            }
            assert_eq!(d.stats().access_units, released);
            assert!(released > 0);
        }
    }
}
