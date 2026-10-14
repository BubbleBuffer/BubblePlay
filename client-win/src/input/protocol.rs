//! Browser -> host input packets: encoders, a validator, and the pure helpers
//! the browser keeps beside them (sequence, letterboxed absolute mapping, wheel
//! carry).
//!
//! Byte layouts follow fixtures/input-contract.json and worker/public/input.js.
//! Everything here is pure: no Win32, no sockets, no clocks.

#[cfg(test)]
use std::fmt;

use super::keyboard::PhysicalKey;

pub const TYPE_KEY_DOWN: u8 = 0;
pub const TYPE_KEY_UP: u8 = 1;
pub const TYPE_BUTTONS: u8 = 4;
#[cfg(test)]
pub const TYPE_RELEASE_ALL: u8 = 6;
pub const TYPE_MOTION: u8 = 7;
pub const TYPE_ABSOLUTE: u8 = 8;
pub const TYPE_BUTTONS_AT: u8 = 9;
pub const TYPE_CURSOR_SYNC: u8 = 10;
#[cfg(test)]
pub const TYPE_TEXT: u8 = 11;
#[cfg(test)]
pub const TYPE_CLIPBOARD: u8 = 12;
pub const TYPE_SCAN_KEY: u8 = 13;

pub const SCAN_FLAG_UP: u8 = 1;
pub const SCAN_FLAG_EXTENDED: u8 = 2;
#[cfg(test)]
pub const CLIPBOARD_FLAG_PASTE: u8 = 1;
#[cfg(test)]
pub const CLIPBOARD_HEADER_BYTES: usize = 8;
#[cfg(test)]
pub const CLIPBOARD_CHUNK_BYTES: usize = 8 * 1024;
#[cfg(test)]
pub const CLIPBOARD_MAX_CHUNKS: u16 = 128;
#[cfg(test)]
pub const MAX_TEXT_BYTES: usize = 512;

/// One decoded browser -> host packet. Borrowed fields point into the input slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub enum Packet<'a> {
    Key {
        vk: u16,
        up: bool,
    },
    Buttons(u8),
    ReleaseAll,
    Motion {
        seq: u16,
        dx: i16,
        dy: i16,
        wheel_x: i16,
        wheel_y: i16,
    },
    Absolute {
        seq: u16,
        x: u16,
        y: u16,
    },
    ButtonsAt {
        seq: u16,
        x: u16,
        y: u16,
        mask: u8,
    },
    CursorSync {
        seq: u16,
    },
    Text(&'a str),
    Clipboard(ClipboardChunk<'a>),
    ScanKey {
        key: PhysicalKey,
        up: bool,
    },
}

/// One bounded piece of clipboard text (type 12). Chunks are UTF-8 slices of the
/// whole text, so a chunk may end inside a code point; only the reassembled text is UTF-8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub struct ClipboardChunk<'a> {
    pub paste: bool,
    pub id: u16,
    pub index: u16,
    pub count: u16,
    pub bytes: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub enum PacketError {
    Empty,
    UnknownType(u8),
    Length { kind: u8, len: usize },
    Flags { kind: u8, flags: u8 },
    Scan,
    Text,
    Clipboard,
}

#[cfg(test)]
impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty input packet"),
            Self::UnknownType(kind) => write!(f, "unknown input packet type {kind}"),
            Self::Length { kind, len } => {
                write!(f, "input packet type {kind} has invalid length {len}")
            }
            Self::Flags { kind, flags } => {
                write!(f, "input packet type {kind} has unknown flags {flags:#04x}")
            }
            Self::Scan => f.write_str("scan key has neither a scan code nor a usable VK"),
            Self::Text => f.write_str("text packet is not 1..=512 bytes of UTF-8"),
            Self::Clipboard => f.write_str("clipboard chunk has an invalid index or count"),
        }
    }
}

#[cfg(test)]
impl std::error::Error for PacketError {}

// ---- encoders ---------------------------------------------------------------

/// Key down or up by VK (type 0 / 1). Legacy hosts and VK-only keys use this.
pub fn key(vk: u16, up: bool) -> [u8; 3] {
    let [lo, hi] = vk.to_le_bytes();
    [if up { TYPE_KEY_UP } else { TYPE_KEY_DOWN }, lo, hi]
}

pub fn buttons(mask: u8) -> [u8; 2] {
    [TYPE_BUTTONS, mask]
}

#[cfg(test)]
pub fn release_all() -> [u8; 1] {
    [TYPE_RELEASE_ALL]
}

/// Relative motion with wheel distance (type 7). Wheel values are WHEEL_DELTA units.
pub fn motion(seq: u16, dx: i16, dy: i16, wheel_x: i16, wheel_y: i16) -> [u8; 11] {
    let [s0, s1] = seq.to_le_bytes();
    let [dx0, dx1] = dx.to_le_bytes();
    let [dy0, dy1] = dy.to_le_bytes();
    let [wx0, wx1] = wheel_x.to_le_bytes();
    let [wy0, wy1] = wheel_y.to_le_bytes();
    [TYPE_MOTION, s0, s1, dx0, dx1, dy0, dy1, wx0, wx1, wy0, wy1]
}

/// Absolute position in the host's 0..=65535 space (type 8).
pub fn absolute(seq: u16, x: u16, y: u16) -> [u8; 7] {
    let [s0, s1] = seq.to_le_bytes();
    let [x0, x1] = x.to_le_bytes();
    let [y0, y1] = y.to_le_bytes();
    [TYPE_ABSOLUTE, s0, s1, x0, x1, y0, y1]
}

/// Position plus button mask in one packet (type 9).
pub fn buttons_at(seq: u16, x: u16, y: u16, mask: u8) -> [u8; 8] {
    let [s0, s1] = seq.to_le_bytes();
    let [x0, x1] = x.to_le_bytes();
    let [y0, y1] = y.to_le_bytes();
    [TYPE_BUTTONS_AT, s0, s1, x0, x1, y0, y1, mask]
}

/// Asks the host to report the cursor for `seq` (type 10).
pub fn cursor_sync(seq: u16) -> [u8; 3] {
    let [s0, s1] = seq.to_le_bytes();
    [TYPE_CURSOR_SYNC, s0, s1]
}

/// Text typed by a secure-desktop-safe path (type 11): 1..=512 UTF-8 bytes.
#[cfg(test)]
pub fn text(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    if !(1..=MAX_TEXT_BYTES).contains(&bytes.len()) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(TYPE_TEXT);
    out.extend_from_slice(bytes);
    Some(out)
}

/// Physical key by set-1 scan code (type 13). A `scan` of 0 is the VK fallback.
pub fn scan_key(key: PhysicalKey, up: bool) -> [u8; 6] {
    debug_assert!(
        key.scan <= 0xFF,
        "scan codes are one byte; 0 selects the VK"
    );
    let flags =
        (if up { SCAN_FLAG_UP } else { 0 }) | (if key.extended { SCAN_FLAG_EXTENDED } else { 0 });
    let [s0, s1] = key.scan.to_le_bytes();
    let [v0, v1] = key.vk.to_le_bytes();
    [TYPE_SCAN_KEY, flags, s0, s1, v0, v1]
}

/// One clipboard chunk (type 12). Test-only: runtime clipboard is outside this module.
#[cfg(test)]
pub fn clipboard_chunk(id: u16, index: u16, count: u16, bytes: &[u8], paste: bool) -> Vec<u8> {
    let [i0, i1] = id.to_le_bytes();
    let [x0, x1] = index.to_le_bytes();
    let [c0, c1] = count.to_le_bytes();
    let mut out = vec![
        TYPE_CLIPBOARD,
        if paste { CLIPBOARD_FLAG_PASTE } else { 0 },
        i0,
        i1,
        x0,
        x1,
        c0,
        c1,
    ];
    out.extend_from_slice(bytes);
    out
}

// ---- validator --------------------------------------------------------------

/// Validates and decodes one browser -> host packet with the host's rules.
/// Accepts exactly what the host accepts for the browser's packet types;
/// legacy host-only types (2, 3, 5) and host -> browser types (128, 129) are not input packets here.
#[cfg(test)]
pub fn parse(data: &[u8]) -> Result<Packet<'_>, PacketError> {
    let &kind = data.first().ok_or(PacketError::Empty)?;
    match kind {
        TYPE_KEY_DOWN | TYPE_KEY_UP => {
            let d = fixed(data, 3)?;
            Ok(Packet::Key {
                vk: u16_at(d, 1),
                up: kind == TYPE_KEY_UP,
            })
        }
        TYPE_BUTTONS => Ok(Packet::Buttons(fixed(data, 2)?[1])),
        TYPE_RELEASE_ALL => fixed(data, 1).map(|_| Packet::ReleaseAll),
        TYPE_MOTION => {
            let d = fixed(data, 11)?;
            Ok(Packet::Motion {
                seq: u16_at(d, 1),
                dx: i16_at(d, 3),
                dy: i16_at(d, 5),
                wheel_x: i16_at(d, 7),
                wheel_y: i16_at(d, 9),
            })
        }
        TYPE_ABSOLUTE => {
            let d = fixed(data, 7)?;
            Ok(Packet::Absolute {
                seq: u16_at(d, 1),
                x: u16_at(d, 3),
                y: u16_at(d, 5),
            })
        }
        TYPE_BUTTONS_AT => {
            let d = fixed(data, 8)?;
            Ok(Packet::ButtonsAt {
                seq: u16_at(d, 1),
                x: u16_at(d, 3),
                y: u16_at(d, 5),
                mask: d[7],
            })
        }
        TYPE_CURSOR_SYNC => Ok(Packet::CursorSync {
            seq: u16_at(fixed(data, 3)?, 1),
        }),
        TYPE_TEXT => {
            let body = &data[1..];
            if !(1..=MAX_TEXT_BYTES).contains(&body.len()) {
                return Err(PacketError::Text);
            }
            std::str::from_utf8(body)
                .map(Packet::Text)
                .map_err(|_| PacketError::Text)
        }
        TYPE_CLIPBOARD => parse_clipboard(data),
        TYPE_SCAN_KEY => parse_scan_key(data),
        other => Err(PacketError::UnknownType(other)),
    }
}

#[cfg(test)]
fn fixed(data: &[u8], len: usize) -> Result<&[u8], PacketError> {
    if data.len() == len {
        Ok(data)
    } else {
        Err(PacketError::Length {
            kind: data[0],
            len: data.len(),
        })
    }
}

#[cfg(test)]
fn u16_at(d: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([d[at], d[at + 1]])
}

#[cfg(test)]
fn i16_at(d: &[u8], at: usize) -> i16 {
    i16::from_le_bytes([d[at], d[at + 1]])
}

#[cfg(test)]
fn parse_scan_key(data: &[u8]) -> Result<Packet<'_>, PacketError> {
    let d = fixed(data, 6)?;
    let flags = d[1];
    if flags & !(SCAN_FLAG_UP | SCAN_FLAG_EXTENDED) != 0 {
        return Err(PacketError::Flags {
            kind: TYPE_SCAN_KEY,
            flags,
        });
    }
    let scan = u16_at(d, 2);
    let vk = u16_at(d, 4);
    if scan > 0xFF || (scan == 0 && !(1..=0xFF).contains(&vk)) {
        return Err(PacketError::Scan);
    }
    Ok(Packet::ScanKey {
        key: PhysicalKey {
            scan,
            extended: flags & SCAN_FLAG_EXTENDED != 0,
            vk,
        },
        up: flags & SCAN_FLAG_UP != 0,
    })
}

#[cfg(test)]
fn parse_clipboard(data: &[u8]) -> Result<Packet<'_>, PacketError> {
    if !(CLIPBOARD_HEADER_BYTES..=CLIPBOARD_HEADER_BYTES + CLIPBOARD_CHUNK_BYTES)
        .contains(&data.len())
    {
        return Err(PacketError::Length {
            kind: TYPE_CLIPBOARD,
            len: data.len(),
        });
    }
    let flags = data[1];
    if flags & !CLIPBOARD_FLAG_PASTE != 0 {
        return Err(PacketError::Flags {
            kind: TYPE_CLIPBOARD,
            flags,
        });
    }
    let (id, index, count) = (u16_at(data, 2), u16_at(data, 4), u16_at(data, 6));
    if count == 0 || count > CLIPBOARD_MAX_CHUNKS || index >= count {
        return Err(PacketError::Clipboard);
    }
    Ok(Packet::Clipboard(ClipboardChunk {
        paste: flags & CLIPBOARD_FLAG_PASTE != 0,
        id,
        index,
        count,
        bytes: &data[CLIPBOARD_HEADER_BYTES..],
    }))
}

// ---- sequence ---------------------------------------------------------------

/// The browser's shared input sequence: one wrapping u16 per epoch, starting at
/// 0 like `InputController.seq`. A new epoch restarts it, because the host drops
/// its sequence state with the session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sequence {
    epoch: Option<String>,
    next: u16,
}

impl Sequence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes `epoch` current. Returns true when it changed, which restarts the sequence at 0.
    pub fn enter_epoch(&mut self, epoch: &str) -> bool {
        if self.epoch.as_deref() == Some(epoch) {
            return false;
        }
        self.epoch = Some(epoch.to_owned());
        self.next = 0;
        true
    }

    /// Takes the next number, wrapping 65535 -> 0.
    pub fn alloc(&mut self) -> u16 {
        let seq = self.next;
        self.next = seq.wrapping_add(1);
        seq
    }

    /// Gives back `seq` when its packet was refused. Only the newest number can
    /// be returned; a later number leaves a harmless gap, as `releaseSequence` does.
    pub fn release(&mut self, seq: u16) {
        if self.next == seq.wrapping_add(1) {
            self.next = seq;
        }
    }
}

// ---- absolute mapping -------------------------------------------------------

/// A rectangle in window pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The largest rect with the source's aspect ratio, centred in `bounds`: the
/// picture inside a letterboxed (or pillarboxed) window. `None` when any size is
/// empty or non-finite.
#[cfg(test)]
pub fn letterbox(bounds: Rect, source_w: f64, source_h: f64) -> Option<Rect> {
    let positive = |v: f64| v.is_finite() && v > 0.0;
    if !([bounds.w, bounds.h, source_w, source_h]
        .into_iter()
        .all(positive)
        && bounds.x.is_finite()
        && bounds.y.is_finite())
    {
        return None;
    }
    let scale = (bounds.w / source_w).min(bounds.h / source_h);
    let (w, h) = (source_w * scale, source_h * scale);
    Some(Rect {
        x: bounds.x + (bounds.w - w) / 2.0,
        y: bounds.y + (bounds.h - h) / 2.0,
        w,
        h,
    })
}

/// Maps a window point to the host's 0..=65535 absolute space through the
/// picture rect. Points outside the picture (letterbox bars, the rest of the
/// window) return `None`: there is no remote pixel there, so no absolute move is sent.
pub fn absolute_point(picture: Rect, px: f64, py: f64) -> Option<(u16, u16)> {
    let inside = px >= picture.x
        && px <= picture.x + picture.w
        && py >= picture.y
        && py <= picture.y + picture.h;
    if !inside || !(picture.w > 0.0 && picture.h > 0.0) {
        return None;
    }
    Some((
        axis(px - picture.x, picture.w),
        axis(py - picture.y, picture.h),
    ))
}

fn axis(offset: f64, extent: f64) -> u16 {
    ((offset / extent).clamp(0.0, 1.0) * 65535.0).round() as u16
}

// ---- wheel ------------------------------------------------------------------

/// DOM `deltaMode`: 0 pixels, 1 lines, 2 pages. Unknown modes count as pixels, as in input.js.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub enum DeltaMode {
    Pixel,
    Line,
    Page,
}

#[cfg(test)]
impl DeltaMode {
    pub fn from_dom(mode: u32) -> Self {
        match mode {
            1 => Self::Line,
            2 => Self::Page,
            _ => Self::Pixel,
        }
    }
}

/// Converts DOM wheel deltas to WHEEL_DELTA units (120 per notch) and keeps the
/// fraction of each axis, so smooth or trackpad scrolling loses no distance.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg(test)]
pub struct WheelAccumulator {
    pub pixels_per_notch: f64,
    pub lines_per_notch: f64,
    pub pages_per_notch: f64,
    x: f64,
    y: f64,
}

#[cfg(test)]
impl Default for WheelAccumulator {
    fn default() -> Self {
        Self::new(100.0, 3.0, 1.0 / 3.0)
    }
}

#[cfg(test)]
impl WheelAccumulator {
    pub fn new(pixels_per_notch: f64, lines_per_notch: f64, pages_per_notch: f64) -> Self {
        Self {
            pixels_per_notch,
            lines_per_notch,
            pages_per_notch,
            x: 0.0,
            y: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.x = 0.0;
        self.y = 0.0;
    }

    fn units(&self, delta: f64, mode: DeltaMode) -> f64 {
        if !delta.is_finite() {
            return 0.0;
        }
        let per_notch = match mode {
            DeltaMode::Pixel => self.pixels_per_notch,
            DeltaMode::Line => self.lines_per_notch,
            DeltaMode::Page => self.pages_per_notch,
        };
        delta * 120.0 / per_notch
    }

    /// Browser deltaY is positive downward and the Windows wheel is positive
    /// upward, so Y is negated. Returns the whole units now due.
    pub fn add(&mut self, delta_x: f64, delta_y: f64, mode: DeltaMode) -> (i32, i32) {
        self.x += self.units(delta_x, mode);
        self.y -= self.units(delta_y, mode);
        let (x, y) = (self.x.trunc(), self.y.trunc());
        self.x -= x;
        self.y -= y;
        (x as i32, y as i32)
    }
}

/// Carries sub-unit wheel distance on pointer motion, as `InputController.motion()` does.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[cfg(test)]
pub struct WheelResidue {
    x: f64,
    y: f64,
}

#[cfg(test)]
impl WheelResidue {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Adds wheel units (non-finite counts as 0) and returns the whole units now due.
    pub fn take(&mut self, wheel_x: f64, wheel_y: f64) -> (i32, i32) {
        (carry(&mut self.x, wheel_x), carry(&mut self.y, wheel_y))
    }
}

#[cfg(test)]
fn carry(residue: &mut f64, value: f64) -> i32 {
    let total = *residue + if value.is_finite() { value } else { 0.0 };
    let units = total.trunc();
    *residue = total - units;
    units as i32
}

/// Removes the largest i16 step from `pending`. Loop while `pending != 0` to send
/// a long distance as several packets (`pending.map(i16)`, then `pending -= chunk`).
#[cfg(test)]
pub fn take_i16(pending: &mut i32) -> i16 {
    let step = (*pending).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    *pending -= i32::from(step);
    step
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::keyboard::physical_key;
    use serde_json::Value;

    const FIXTURE: &str = include_str!("../../../fixtures/input-contract.json");

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).expect("fixture parses")
    }

    fn bytes_of(value: &Value) -> Vec<u8> {
        value
            .as_array()
            .expect("byte array")
            .iter()
            .map(|b| b.as_u64().expect("byte") as u8)
            .collect()
    }

    #[test]
    fn fixture_packets_decode_and_encode_back_byte_for_byte() {
        let fixture = fixture();
        for case in fixture["packets"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let bytes = bytes_of(&case["bytes"]);
            match parse(&bytes).unwrap_or_else(|error| panic!("{name}: {error}")) {
                Packet::ScanKey { key, up } => {
                    let expected = &case["key"];
                    assert_eq!(
                        key.scan,
                        expected["scan"].as_u64().unwrap_or(0) as u16,
                        "{name} scan"
                    );
                    assert_eq!(
                        key.extended,
                        expected["extended"].as_bool().unwrap_or(false),
                        "{name} extended"
                    );
                    if let Some(vk) = expected["vk"].as_u64() {
                        assert_eq!(key.vk, vk as u16, "{name} vk");
                    }
                    assert_eq!(up, case["up"].as_bool().unwrap(), "{name} up");
                    assert_eq!(scan_key(key, up).to_vec(), bytes, "{name} re-encode");
                }
                Packet::Clipboard(chunk) => {
                    let expected = &case["clipboard"];
                    assert_eq!(
                        chunk.id as u64,
                        expected["id"].as_u64().unwrap(),
                        "{name} id"
                    );
                    assert_eq!(
                        chunk.index as u64,
                        expected["index"].as_u64().unwrap(),
                        "{name} index"
                    );
                    assert_eq!(
                        chunk.count as u64,
                        expected["count"].as_u64().unwrap(),
                        "{name} count"
                    );
                    assert_eq!(
                        chunk.paste,
                        expected["paste"].as_bool().unwrap(),
                        "{name} paste"
                    );
                    assert_eq!(
                        chunk.bytes,
                        expected["text"].as_str().unwrap().as_bytes(),
                        "{name} text"
                    );
                    assert_eq!(
                        clipboard_chunk(
                            chunk.id,
                            chunk.index,
                            chunk.count,
                            chunk.bytes,
                            chunk.paste
                        ),
                        bytes,
                        "{name} re-encode"
                    );
                }
                other => panic!("{name}: unexpected packet {other:?}"),
            }
        }
    }

    #[test]
    fn fixture_packets_name_the_expected_keys() {
        // The hook path yields the same bytes the fixture lists for the arrow key.
        let arrow_left = physical_key(0x25, 0x4B, true).unwrap();
        assert_eq!(
            scan_key(arrow_left, false).to_vec(),
            vec![13, 2, 75, 0, 37, 0]
        );
        let altgr = physical_key(0xA5, 0x38, true).unwrap();
        assert_eq!(scan_key(altgr, true).to_vec(), vec![13, 3, 56, 0, 165, 0]);
        let shift = physical_key(0xA0, 0x2A, false).unwrap();
        assert_eq!(scan_key(shift, false).to_vec(), vec![13, 0, 42, 0, 160, 0]);
        let pause = physical_key(0x13, 0x45, false).unwrap();
        assert_eq!(scan_key(pause, false).to_vec(), vec![13, 0, 0, 0, 19, 0]);
    }

    #[test]
    fn fixture_invalid_packets_are_rejected_with_the_matching_reason() {
        let fixture = fixture();
        let invalid = fixture["invalidPackets"].as_array().unwrap();
        let expected = [
            PacketError::Flags { kind: 13, flags: 4 },
            PacketError::Scan,
            PacketError::Scan,
            PacketError::Clipboard,
            PacketError::Clipboard,
            PacketError::Flags { kind: 12, flags: 2 },
        ];
        assert_eq!(invalid.len(), expected.len());
        for (case, want) in invalid.iter().zip(expected) {
            let bytes = bytes_of(case);
            assert_eq!(parse(&bytes), Err(want), "{bytes:?}");
        }
    }

    #[test]
    fn scan_key_and_clipboard_encoders_match_fixture_bytes() {
        assert_eq!(
            scan_key(
                PhysicalKey {
                    scan: 75,
                    extended: true,
                    vk: 37
                },
                false
            ),
            [13, 2, 75, 0, 37, 0]
        );
        assert_eq!(
            scan_key(
                PhysicalKey {
                    scan: 56,
                    extended: true,
                    vk: 165
                },
                true
            ),
            [13, 3, 56, 0, 165, 0]
        );
        assert_eq!(
            scan_key(
                PhysicalKey {
                    scan: 0,
                    extended: false,
                    vk: 19
                },
                false
            ),
            [13, 0, 0, 0, 19, 0]
        );
        assert_eq!(
            clipboard_chunk(7, 0, 1, &text_bytes("hé"), true),
            vec![12, 1, 7, 0, 0, 0, 1, 0, 104, 195, 169],
        );
    }

    fn text_bytes(value: &str) -> Vec<u8> {
        value.as_bytes().to_vec()
    }

    #[test]
    fn browser_encoders_match_input_js_bytes() {
        // Expected bytes come from running worker/public/input.js `packet.*`.
        assert_eq!(key(0x25, false), [0, 0x25, 0]);
        assert_eq!(key(0xA5, true), [1, 0xA5, 0]);
        assert_eq!(buttons(3), [4, 3]);
        assert_eq!(release_all(), [6]);
        assert_eq!(
            motion(0x1234, -1, 2, 3, -4),
            [7, 0x34, 0x12, 0xFF, 0xFF, 2, 0, 3, 0, 0xFC, 0xFF]
        );
        assert_eq!(
            absolute(0xFFFF, 65535, 0),
            [8, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0]
        );
        assert_eq!(buttons_at(1, 100, 200, 5), [9, 1, 0, 100, 0, 200, 0, 5]);
        assert_eq!(cursor_sync(0x0102), [10, 2, 1]);
        assert_eq!(text("hé").unwrap(), vec![11, 104, 195, 169]);
    }

    #[test]
    fn encoders_round_trip_through_the_validator() {
        let key_packet = key(0x41, true);
        assert_eq!(parse(&key_packet), Ok(Packet::Key { vk: 0x41, up: true }));
        assert_eq!(parse(&buttons(1)), Ok(Packet::Buttons(1)));
        assert_eq!(parse(&release_all()), Ok(Packet::ReleaseAll));
        assert_eq!(
            parse(&motion(7, i16::MIN, i16::MAX, -120, 120)),
            Ok(Packet::Motion {
                seq: 7,
                dx: i16::MIN,
                dy: i16::MAX,
                wheel_x: -120,
                wheel_y: 120
            }),
        );
        assert_eq!(
            parse(&absolute(9, 1, 65534)),
            Ok(Packet::Absolute {
                seq: 9,
                x: 1,
                y: 65534
            })
        );
        assert_eq!(
            parse(&buttons_at(3, 4, 5, 6)),
            Ok(Packet::ButtonsAt {
                seq: 3,
                x: 4,
                y: 5,
                mask: 6
            })
        );
        assert_eq!(
            parse(&cursor_sync(65535)),
            Ok(Packet::CursorSync { seq: 65535 })
        );
        let typed = text("aé").unwrap();
        assert_eq!(parse(&typed), Ok(Packet::Text("aé")));
        let scan = PhysicalKey {
            scan: 0x1C,
            extended: true,
            vk: 0x0D,
        };
        assert_eq!(
            parse(&scan_key(scan, true)),
            Ok(Packet::ScanKey {
                key: scan,
                up: true
            })
        );
    }

    #[test]
    fn text_is_bounded_to_512_utf8_bytes() {
        assert_eq!(text(""), None);
        assert_eq!(text(&"a".repeat(512)).unwrap().len(), 513);
        assert_eq!(text(&"a".repeat(513)), None);
        // 256 two-byte code points are exactly 512 bytes.
        assert!(text(&"é".repeat(256)).is_some());
        assert!(text(&"é".repeat(257)).is_none());
        assert_eq!(parse(&[TYPE_TEXT]), Err(PacketError::Text));
        assert_eq!(parse(&[TYPE_TEXT, 0xFF]), Err(PacketError::Text));
        let mut too_long = vec![TYPE_TEXT];
        too_long.extend(std::iter::repeat_n(b'a', 513));
        assert_eq!(parse(&too_long), Err(PacketError::Text));
    }

    #[test]
    fn packet_lengths_are_exact() {
        assert_eq!(parse(&[]), Err(PacketError::Empty));
        assert_eq!(
            parse(&[TYPE_KEY_DOWN, 1]),
            Err(PacketError::Length { kind: 0, len: 2 })
        );
        assert_eq!(
            parse(&[TYPE_BUTTONS, 1, 2]),
            Err(PacketError::Length { kind: 4, len: 3 })
        );
        assert_eq!(
            parse(&[TYPE_RELEASE_ALL, 0]),
            Err(PacketError::Length { kind: 6, len: 2 })
        );
        assert_eq!(
            parse(&[TYPE_MOTION; 10]),
            Err(PacketError::Length { kind: 7, len: 10 })
        );
        assert_eq!(
            parse(&[TYPE_ABSOLUTE; 8]),
            Err(PacketError::Length { kind: 8, len: 8 })
        );
        assert_eq!(
            parse(&[TYPE_BUTTONS_AT; 7]),
            Err(PacketError::Length { kind: 9, len: 7 })
        );
        assert_eq!(
            parse(&[TYPE_CURSOR_SYNC, 1]),
            Err(PacketError::Length { kind: 10, len: 2 })
        );
        assert_eq!(
            parse(&[TYPE_SCAN_KEY, 0, 1, 0, 0]),
            Err(PacketError::Length { kind: 13, len: 5 })
        );
    }

    #[test]
    fn legacy_and_host_only_types_are_not_browser_input() {
        for kind in [2u8, 3, 5, 14, 128, 129, 255] {
            assert_eq!(
                parse(&[kind, 0, 0, 0, 0]),
                Err(PacketError::UnknownType(kind))
            );
        }
    }

    #[test]
    fn scan_key_edges() {
        // Flags other than UP and EXTENDED are unknown.
        assert_eq!(
            parse(&[TYPE_SCAN_KEY, 0b100, 1, 0, 0, 0]),
            Err(PacketError::Flags { kind: 13, flags: 4 })
        );
        // Scan 0 needs a usable VK (1..=0xFF).
        assert_eq!(
            parse(&[TYPE_SCAN_KEY, 0, 0, 0, 0xFF, 0]),
            Ok(Packet::ScanKey {
                key: PhysicalKey {
                    scan: 0,
                    extended: false,
                    vk: 0xFF
                },
                up: false,
            })
        );
        assert_eq!(
            parse(&[TYPE_SCAN_KEY, 0, 0, 0, 0x00, 0x01]),
            Err(PacketError::Scan)
        );
        // Scan codes stop at one byte.
        assert_eq!(
            parse(&[TYPE_SCAN_KEY, 0, 0, 1, 0x41, 0]),
            Err(PacketError::Scan)
        );
        // A scan code with a zero VK is valid: the host uses the scan.
        assert_eq!(
            parse(&[TYPE_SCAN_KEY, 3, 0x1D, 0, 0, 0]),
            Ok(Packet::ScanKey {
                key: PhysicalKey {
                    scan: 0x1D,
                    extended: true,
                    vk: 0
                },
                up: true,
            })
        );
    }

    #[test]
    fn clipboard_edges_follow_the_contract() {
        let full = clipboard_chunk(1, 0, 1, &vec![b'a'; CLIPBOARD_CHUNK_BYTES], false);
        assert!(
            parse(&full).is_ok(),
            "one full chunk is the largest legal chunk"
        );
        let oversized = clipboard_chunk(1, 0, 1, &vec![b'a'; CLIPBOARD_CHUNK_BYTES + 1], false);
        assert_eq!(
            parse(&oversized),
            Err(PacketError::Length {
                kind: 12,
                len: 8 + CLIPBOARD_CHUNK_BYTES + 1
            })
        );
        let last_index = clipboard_chunk(9, 127, 128, b"x", true);
        assert_eq!(
            parse(&last_index),
            Ok(Packet::Clipboard(ClipboardChunk {
                paste: true,
                id: 9,
                index: 127,
                count: 128,
                bytes: b"x"
            })),
        );
        assert_eq!(
            parse(&clipboard_chunk(9, 0, 129, b"x", false)),
            Err(PacketError::Clipboard)
        );
        assert_eq!(
            parse(&clipboard_chunk(9, 0, 0, b"", false)),
            Err(PacketError::Clipboard)
        );
        assert_eq!(
            parse(&clipboard_chunk(9, 2, 2, b"x", false)),
            Err(PacketError::Clipboard)
        );
        // A chunk may split a UTF-8 code point; only the reassembled text must be valid.
        let split = clipboard_chunk(4, 0, 2, &"é".as_bytes()[..1], false);
        assert!(parse(&split).is_ok());
        // Empty payloads are accepted by the host, so the validator accepts them too.
        assert!(parse(&clipboard_chunk(4, 0, 1, b"", false)).is_ok());
    }

    #[test]
    fn sequence_starts_at_zero_wraps_and_resets_per_epoch() {
        let mut seq = Sequence::new();
        assert_eq!(seq.alloc(), 0);
        assert_eq!(seq.alloc(), 1);
        assert!(seq.enter_epoch("a1"));
        assert_eq!(
            seq.alloc(),
            0,
            "first epoch restarts the browser's first value"
        );
        assert!(!seq.enter_epoch("a1"), "same epoch keeps counting");
        assert_eq!(seq.alloc(), 1);
        assert!(seq.enter_epoch("b2"));
        assert_eq!(seq.alloc(), 0);
    }

    #[test]
    fn sequence_wraps_at_u16_max() {
        let mut seq = Sequence::new();
        seq.enter_epoch("wrap");
        for expected in 0..=u16::MAX {
            assert_eq!(seq.alloc(), expected);
        }
        assert_eq!(seq.alloc(), 0);
        assert_eq!(seq.alloc(), 1);
    }

    #[test]
    fn sequence_release_only_returns_the_newest_number() {
        let mut seq = Sequence::new();
        seq.enter_epoch("r");
        let first = seq.alloc();
        seq.release(first);
        assert_eq!(seq.alloc(), first, "refused newest packet is reused");
        let kept = seq.alloc();
        let later = seq.alloc();
        seq.release(kept);
        assert_eq!(
            seq.alloc(),
            later.wrapping_add(1),
            "older release leaves a gap"
        );
        let mut wrapped = Sequence::new();
        wrapped.enter_epoch("w");
        for _ in 0..=u16::MAX {
            wrapped.alloc();
        }
        let zero = wrapped.alloc();
        wrapped.release(zero);
        assert_eq!(wrapped.alloc(), 0);
    }

    #[test]
    fn letterbox_centres_the_picture_and_maps_only_inside_it() {
        let window = Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 1000.0,
        };
        assert_eq!(
            letterbox(window, 1000.0, 500.0),
            Some(Rect {
                x: 0.0,
                y: 250.0,
                w: 1000.0,
                h: 500.0
            })
        );
        let offset = Rect {
            x: 10.0,
            y: 20.0,
            w: 1600.0,
            h: 900.0,
        };
        let picture = letterbox(offset, 640.0, 480.0).unwrap();
        assert_eq!(
            picture,
            Rect {
                x: 210.0,
                y: 20.0,
                w: 1200.0,
                h: 900.0
            }
        );
        assert_eq!(
            absolute_point(picture, 209.9, 100.0),
            None,
            "left pillarbox bar"
        );
        assert_eq!(
            absolute_point(picture, 100.0, 19.0),
            None,
            "above the picture"
        );
        assert_eq!(absolute_point(picture, 210.0, 20.0), Some((0, 0)));
        assert_eq!(absolute_point(picture, 1410.0, 920.0), Some((65535, 65535)));
        // Centre: 600 of 1200 px is exactly half, 32767.5, which rounds up as Math.round does.
        assert_eq!(absolute_point(picture, 810.0, 470.0), Some((32768, 32768)));
        assert_eq!(absolute_point(picture, f64::NAN, 100.0), None);
    }

    #[test]
    fn letterbox_rejects_empty_or_non_finite_sizes() {
        let window = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        assert_eq!(letterbox(window, 0.0, 480.0), None);
        assert_eq!(letterbox(window, 640.0, f64::INFINITY), None);
        assert_eq!(
            letterbox(
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 100.0
                },
                640.0,
                480.0
            ),
            None
        );
        let zero_picture = Rect {
            x: 5.0,
            y: 5.0,
            w: 0.0,
            h: 10.0,
        };
        assert_eq!(absolute_point(zero_picture, 5.0, 6.0), None);
    }

    #[test]
    fn wheel_converts_modes_and_flips_only_vertical() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(
            wheel.add(0.0, 100.0, DeltaMode::Pixel),
            (0, -120),
            "down is negative"
        );
        assert_eq!(
            wheel.add(-100.0, 0.0, DeltaMode::Pixel),
            (-120, 0),
            "X is not flipped"
        );
        assert_eq!(wheel.add(0.0, 1.0, DeltaMode::Line), (0, -40));
        let mut pages = WheelAccumulator::new(100.0, 3.0, 0.5);
        assert_eq!(pages.add(0.0, -1.0, DeltaMode::Page), (0, 240));
        assert_eq!(DeltaMode::from_dom(0), DeltaMode::Pixel);
        assert_eq!(DeltaMode::from_dom(1), DeltaMode::Line);
        assert_eq!(DeltaMode::from_dom(2), DeltaMode::Page);
        assert_eq!(DeltaMode::from_dom(9), DeltaMode::Pixel);
    }

    #[test]
    fn wheel_carries_the_fraction_until_a_whole_unit_is_due() {
        let mut wheel = WheelAccumulator::new(120.0, 3.0, 1.0 / 3.0);
        assert_eq!(wheel.add(0.0, 0.5, DeltaMode::Pixel), (0, 0));
        assert_eq!(wheel.add(0.0, 0.5, DeltaMode::Pixel), (0, -1));
        assert_eq!(wheel.add(0.0, 0.5, DeltaMode::Pixel), (0, 0));
    }

    #[test]
    fn wheel_reset_drops_the_residue_and_non_finite_deltas_count_as_zero() {
        let mut wheel = WheelAccumulator::new(120.0, 3.0, 1.0 / 3.0);
        assert_eq!(wheel.add(0.0, 0.5, DeltaMode::Pixel), (0, 0));
        wheel.reset();
        assert_eq!(wheel.add(0.0, 0.5, DeltaMode::Pixel), (0, 0));
        assert_eq!(wheel.add(0.0, 0.5, DeltaMode::Pixel), (0, -1));
        assert_eq!(wheel.add(f64::NAN, f64::INFINITY, DeltaMode::Pixel), (0, 0));
    }

    #[test]
    fn default_pixel_wheel_loses_no_distance_over_many_small_events() {
        // 100 events of 1 px at 100 px per notch is 120 units; the carry keeps the sum within one unit.
        let mut wheel = WheelAccumulator::default();
        let total: i32 = (0..100)
            .map(|_| wheel.add(0.0, 1.0, DeltaMode::Pixel).1)
            .sum();
        assert!((total + 120).abs() <= 1, "total {total}");
    }

    #[test]
    fn pointer_wheel_residue_carries_per_axis() {
        let mut residue = WheelResidue::default();
        assert_eq!(residue.take(0.5, -0.5), (0, 0));
        assert_eq!(residue.take(0.5, -0.5), (1, -1));
        assert_eq!(residue.take(0.5, 0.0), (0, 0));
        assert_eq!(residue.take(0.25, f64::NAN), (0, 0));
        assert_eq!(residue.take(0.25, 0.0), (1, 0));
        residue.reset();
        assert_eq!(residue.take(0.5, 0.0), (0, 0));
    }

    #[test]
    fn long_wheel_distance_splits_into_i16_packets() {
        let mut pending = 70_000;
        assert_eq!(take_i16(&mut pending), i16::MAX);
        assert_eq!(pending, 70_000 - 32_767);
        assert_eq!(take_i16(&mut pending), i16::MAX);
        assert_eq!(take_i16(&mut pending), 4_466);
        assert_eq!(pending, 0);
        let mut negative = -40_000;
        assert_eq!(take_i16(&mut negative), i16::MIN);
        assert_eq!(negative, -40_000 + 32_768);
    }
}
