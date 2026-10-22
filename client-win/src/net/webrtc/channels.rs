//! Input data-channel routing and framing for the native client.
//!
//! Mirrors `worker/public/receiver.js`. Types 7 (motion and wheel) and 8
//! (absolute) travel on `input-motion`. Every other type travels on
//! `input-control`. Type 7 stays on the motion lane even for discrete wheel
//! notches, because the browser sends those on the motion lane as well.
//! Packet layouts follow `fixtures/input-contract.json` and
//! `client-win/src/input/protocol.rs`.

use std::fmt;

pub const CONTROL_LABEL: &str = "input-control";
pub const MOTION_LABEL: &str = "input-motion";

const TYPE_KEY_DOWN: u8 = 0;
const TYPE_KEY_UP: u8 = 1;
const TYPE_BUTTONS: u8 = 4;
const TYPE_RELEASE_ALL: u8 = 6;
const TYPE_MOTION: u8 = 7;
const TYPE_ABSOLUTE: u8 = 8;
const TYPE_BUTTONS_AT: u8 = 9;
const TYPE_CURSOR_SYNC: u8 = 10;
const TYPE_TEXT: u8 = 11;
const TYPE_CLIPBOARD: u8 = 12;
const TYPE_SCAN_KEY: u8 = 13;

const TEXT_MAX_BODY: usize = 512;
const CLIPBOARD_HEADER: usize = 8;
const CLIPBOARD_MAX_FRAME: usize = CLIPBOARD_HEADER + 8192;
const CLIPBOARD_MAX_CHUNKS: u16 = 128;
const CLIPBOARD_FLAG_PASTE: u8 = 1;
const SCAN_FLAG_UP: u8 = 1;
const SCAN_FLAG_EXTENDED: u8 = 2;

/// The data channel a packet travels on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Ordered, reliable: keys, buttons, text, clipboard, cursor sync.
    Control,
    /// Unordered, no retransmits: motion and absolute pointer packets.
    Motion,
}

impl Lane {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Control => CONTROL_LABEL,
            Self::Motion => MOTION_LABEL,
        }
    }
}

/// Lane for an outbound packet type. Unknown types map to `Control`, but
/// [`frame_outbound`] rejects them before a lane is used.
pub fn lane_for_packet(kind: u8) -> Lane {
    match kind {
        TYPE_MOTION | TYPE_ABSOLUTE => Lane::Motion,
        _ => Lane::Control,
    }
}

/// Lane for a data channel label, or `None` for any other label.
pub fn lane_for_label(label: &str) -> Option<Lane> {
    match label {
        CONTROL_LABEL => Some(Lane::Control),
        MOTION_LABEL => Some(Lane::Motion),
        _ => None,
    }
}

/// Accepts an inbound message only when it is binary and on one of the two
/// input labels. Other channels and text messages are ignored.
pub fn accept_inbound(label: &str, binary: bool) -> Option<Lane> {
    if binary {
        lane_for_label(label)
    } else {
        None
    }
}

/// A validated outbound packet with its lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outbound<'a> {
    pub lane: Lane,
    pub kind: u8,
    pub bytes: &'a [u8],
}

/// Validates one client-to-host packet and picks its lane.
pub fn frame_outbound(bytes: &[u8]) -> Result<Outbound<'_>, FrameError> {
    let kind = validate(bytes)?;
    Ok(Outbound {
        lane: lane_for_packet(kind),
        kind,
        bytes,
    })
}

/// Why an outbound packet was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    Empty,
    UnknownType(u8),
    Length { kind: u8, len: usize },
    Flags { kind: u8, flags: u8 },
    Scan,
    Text,
    Clipboard,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty input packet"),
            Self::UnknownType(t) => write!(f, "unknown input packet type {t}"),
            Self::Length { kind, len } => {
                write!(f, "input packet type {kind} has invalid length {len}")
            }
            Self::Flags { kind, flags } => {
                write!(f, "input packet type {kind} has invalid flags {flags:#x}")
            }
            Self::Scan => f.write_str("invalid scan key packet"),
            Self::Text => f.write_str("invalid text packet"),
            Self::Clipboard => f.write_str("invalid clipboard packet"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Checks length, flags and ranges for one packet. Returns its type byte.
fn validate(bytes: &[u8]) -> Result<u8, FrameError> {
    let &kind = bytes.first().ok_or(FrameError::Empty)?;
    let checked = match kind {
        TYPE_KEY_DOWN | TYPE_KEY_UP => fixed(bytes, 3),
        TYPE_BUTTONS => fixed(bytes, 2),
        TYPE_RELEASE_ALL => fixed(bytes, 1),
        TYPE_MOTION => fixed(bytes, 11),
        TYPE_ABSOLUTE => fixed(bytes, 7),
        TYPE_BUTTONS_AT => fixed(bytes, 8),
        TYPE_CURSOR_SYNC => fixed(bytes, 3),
        TYPE_TEXT => validate_text(bytes),
        TYPE_CLIPBOARD => validate_clipboard(bytes),
        TYPE_SCAN_KEY => validate_scan_key(bytes),
        other => return Err(FrameError::UnknownType(other)),
    };
    checked.map(|()| kind)
}

fn fixed(bytes: &[u8], len: usize) -> Result<(), FrameError> {
    if bytes.len() == len {
        Ok(())
    } else {
        Err(FrameError::Length {
            kind: bytes[0],
            len: bytes.len(),
        })
    }
}

/// Text is one type byte, then 1..=512 bytes of UTF-8.
fn validate_text(bytes: &[u8]) -> Result<(), FrameError> {
    let body = &bytes[1..];
    if !(1..=TEXT_MAX_BODY).contains(&body.len()) {
        return Err(FrameError::Text);
    }
    std::str::from_utf8(body)
        .map(|_| ())
        .map_err(|_| FrameError::Text)
}

/// Clipboard is an 8-byte header and a chunk of up to 8192 bytes. The chunk is
/// not checked as UTF-8, because a chunk may split a character.
fn validate_clipboard(bytes: &[u8]) -> Result<(), FrameError> {
    if !(CLIPBOARD_HEADER..=CLIPBOARD_MAX_FRAME).contains(&bytes.len()) {
        return Err(FrameError::Length {
            kind: TYPE_CLIPBOARD,
            len: bytes.len(),
        });
    }
    let flags = bytes[1];
    if flags & !CLIPBOARD_FLAG_PASTE != 0 {
        return Err(FrameError::Flags {
            kind: TYPE_CLIPBOARD,
            flags,
        });
    }
    let index = u16_at(bytes, 4);
    let count = u16_at(bytes, 6);
    if count == 0 || count > CLIPBOARD_MAX_CHUNKS || index >= count {
        return Err(FrameError::Clipboard);
    }
    Ok(())
}

/// Scan key is 6 bytes. A scan code of 0 selects the VK fallback, which needs a
/// VK in 1..=255. Any other scan code must be one byte.
fn validate_scan_key(bytes: &[u8]) -> Result<(), FrameError> {
    fixed(bytes, 6)?;
    let flags = bytes[1];
    if flags & !(SCAN_FLAG_UP | SCAN_FLAG_EXTENDED) != 0 {
        return Err(FrameError::Flags {
            kind: TYPE_SCAN_KEY,
            flags,
        });
    }
    let scan = u16_at(bytes, 2);
    let vk = u16_at(bytes, 4);
    if scan > 0xFF || (scan == 0 && !(1..=0xFF).contains(&vk)) {
        return Err(FrameError::Scan);
    }
    Ok(())
}

/// Little-endian u16 at `at`. Callers have checked the length.
fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../../fixtures/input-contract.json");

    fn fixture() -> serde_json::Value {
        serde_json::from_str(FIXTURE).unwrap()
    }

    fn bytes_of(value: &serde_json::Value) -> Vec<u8> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|b| u8::try_from(b.as_u64().unwrap()).unwrap())
            .collect()
    }

    fn scan_frame(flags: u8, scan: u16, vk: u16) -> [u8; 6] {
        let [s0, s1] = scan.to_le_bytes();
        let [v0, v1] = vk.to_le_bytes();
        [TYPE_SCAN_KEY, flags, s0, s1, v0, v1]
    }

    fn clipboard_frame(flags: u8, id: u16, index: u16, count: u16, chunk: &[u8]) -> Vec<u8> {
        let mut out = vec![TYPE_CLIPBOARD, flags];
        for v in [id, index, count] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(chunk);
        out
    }

    #[test]
    fn labels_match_the_browser_channels() {
        assert_eq!(CONTROL_LABEL, "input-control");
        assert_eq!(MOTION_LABEL, "input-motion");
        assert_eq!(Lane::Control.label(), CONTROL_LABEL);
        assert_eq!(Lane::Motion.label(), MOTION_LABEL);
    }

    #[test]
    fn only_motion_and_absolute_use_the_motion_lane() {
        for kind in 0..=255u8 {
            let expected = if kind == TYPE_MOTION || kind == TYPE_ABSOLUTE {
                Lane::Motion
            } else {
                Lane::Control
            };
            assert_eq!(lane_for_packet(kind), expected, "type {kind}");
        }
    }

    #[test]
    fn wheel_motion_stays_on_the_motion_lane() {
        // Discrete notch (wheel y = 120) and smooth wheel (y = 300) are both type 7.
        let notch = [7, 1, 0, 0, 0, 0, 0, 0, 0, 120, 0];
        assert_eq!(frame_outbound(&notch).unwrap().lane, Lane::Motion);
        let smooth = [7, 2, 0, 0, 0, 0, 0, 0, 0, 0x2C, 0x01];
        assert_eq!(frame_outbound(&smooth).unwrap().lane, Lane::Motion);
    }

    #[test]
    fn label_lookup_and_inbound_filter() {
        assert_eq!(lane_for_label("input-control"), Some(Lane::Control));
        assert_eq!(lane_for_label("input-motion"), Some(Lane::Motion));
        assert_eq!(lane_for_label("chat"), None);
        assert_eq!(accept_inbound(CONTROL_LABEL, true), Some(Lane::Control));
        assert_eq!(accept_inbound(MOTION_LABEL, true), Some(Lane::Motion));
        assert_eq!(accept_inbound(CONTROL_LABEL, false), None);
        assert_eq!(accept_inbound(MOTION_LABEL, false), None);
        assert_eq!(accept_inbound("chat", true), None);
    }

    #[test]
    fn every_packet_type_frames_on_its_lane() {
        let cases: [(&[u8], u8, Lane); 11] = [
            (&[0, 0x41, 0x00], 0, Lane::Control), // key down, vk 65
            (&[1, 0x41, 0x00], 1, Lane::Control), // key up
            (&[4, 0x03], 4, Lane::Control),       // buttons mask
            (&[6], 6, Lane::Control),             // release all
            (&[7, 1, 0, 2, 0, 0xFE, 0xFF, 0, 0, 0, 0], 7, Lane::Motion), // motion dx 2, dy -2
            (&[8, 3, 0, 0x40, 0x01, 0x80, 0x00], 8, Lane::Motion), // absolute 320, 128
            (&[9, 4, 0, 0x40, 0x01, 0x80, 0x00, 1], 9, Lane::Control), // buttons at
            (&[10, 5, 0], 10, Lane::Control),     // cursor sync
            (&[11, b'h', b'i'], 11, Lane::Control), // text
            (&[12, 0, 9, 0, 0, 0, 1, 0, b'x'], 12, Lane::Control), // clipboard chunk
            (&[13, 0, 0x1E, 0, 0x41, 0], 13, Lane::Control), // scan key KeyA down
        ];
        for (bytes, kind, lane) in cases {
            let out = frame_outbound(bytes).unwrap_or_else(|e| panic!("type {kind}: {e}"));
            assert_eq!(out.kind, kind);
            assert_eq!(out.lane, lane);
            assert_eq!(out.bytes, bytes);
        }
    }

    #[test]
    fn fixed_lengths_are_exact() {
        assert_eq!(
            frame_outbound(&[0, 0]).unwrap_err(),
            FrameError::Length { kind: 0, len: 2 }
        );
        assert_eq!(
            frame_outbound(&[4, 0, 0]).unwrap_err(),
            FrameError::Length { kind: 4, len: 3 }
        );
        assert_eq!(
            frame_outbound(&[6, 0]).unwrap_err(),
            FrameError::Length { kind: 6, len: 2 }
        );
        assert_eq!(
            frame_outbound(&[7; 12]).unwrap_err(),
            FrameError::Length { kind: 7, len: 12 }
        );
        assert_eq!(
            frame_outbound(&[8; 6]).unwrap_err(),
            FrameError::Length { kind: 8, len: 6 }
        );
        assert_eq!(
            frame_outbound(&[9; 9]).unwrap_err(),
            FrameError::Length { kind: 9, len: 9 }
        );
        assert_eq!(
            frame_outbound(&[10, 0]).unwrap_err(),
            FrameError::Length { kind: 10, len: 2 }
        );
        assert_eq!(
            frame_outbound(&[13, 0, 0, 0, 0]).unwrap_err(),
            FrameError::Length { kind: 13, len: 5 }
        );
    }

    #[test]
    fn unknown_and_host_only_types_are_rejected() {
        for kind in [2u8, 3, 5, 14, 128, 129, 255] {
            assert_eq!(
                frame_outbound(&[kind, 0, 0]).unwrap_err(),
                FrameError::UnknownType(kind)
            );
        }
        assert_eq!(frame_outbound(&[]).unwrap_err(), FrameError::Empty);
    }

    #[test]
    fn text_bounds_and_utf8() {
        let mut max = vec![TYPE_TEXT];
        max.extend_from_slice(&[b'a'; 512]);
        assert!(frame_outbound(&max).is_ok());
        let mut over = vec![TYPE_TEXT];
        over.extend_from_slice(&[b'a'; 513]);
        assert_eq!(frame_outbound(&over).unwrap_err(), FrameError::Text);
        assert_eq!(frame_outbound(&[TYPE_TEXT]).unwrap_err(), FrameError::Text);
        assert_eq!(
            frame_outbound(&[TYPE_TEXT, 0xFF]).unwrap_err(),
            FrameError::Text
        );
        assert!(frame_outbound(&[TYPE_TEXT, 0xC3, 0xA9]).is_ok());
    }

    #[test]
    fn clipboard_bounds_flags_and_chunk_indexes() {
        assert!(frame_outbound(&clipboard_frame(1, 7, 0, 1, &[])).is_ok());
        let full = clipboard_frame(0, 1, 127, 128, &[0u8; 8192]);
        assert_eq!(full.len(), 8200);
        assert!(frame_outbound(&full).is_ok());
        let too_big = clipboard_frame(0, 1, 0, 1, &[0u8; 8193]);
        assert_eq!(
            frame_outbound(&too_big).unwrap_err(),
            FrameError::Length {
                kind: TYPE_CLIPBOARD,
                len: 8201
            }
        );
        assert_eq!(
            frame_outbound(&clipboard_frame(0, 1, 0, 129, &[])).unwrap_err(),
            FrameError::Clipboard
        );
        assert_eq!(
            frame_outbound(&clipboard_frame(0, 1, 0, 0, &[])).unwrap_err(),
            FrameError::Clipboard
        );
        assert_eq!(
            frame_outbound(&clipboard_frame(0, 1, 1, 1, &[])).unwrap_err(),
            FrameError::Clipboard
        );
        assert_eq!(
            frame_outbound(&clipboard_frame(2, 1, 0, 1, &[])).unwrap_err(),
            FrameError::Flags {
                kind: TYPE_CLIPBOARD,
                flags: 2
            }
        );
        assert_eq!(
            frame_outbound(&[TYPE_CLIPBOARD, 0, 0, 0, 0, 0, 0]).unwrap_err(),
            FrameError::Length {
                kind: TYPE_CLIPBOARD,
                len: 7
            }
        );
    }

    #[test]
    fn scan_key_flags_range_and_vk_fallback() {
        assert_eq!(
            frame_outbound(&scan_frame(0, 0x100, 0)).unwrap_err(),
            FrameError::Scan
        );
        assert_eq!(
            frame_outbound(&scan_frame(0, 0, 0)).unwrap_err(),
            FrameError::Scan
        );
        assert_eq!(
            frame_outbound(&scan_frame(0, 0, 256)).unwrap_err(),
            FrameError::Scan
        );
        assert!(frame_outbound(&scan_frame(0, 0, 255)).is_ok());
        assert_eq!(
            frame_outbound(&scan_frame(4, 0x1E, 0x41)).unwrap_err(),
            FrameError::Flags {
                kind: TYPE_SCAN_KEY,
                flags: 4
            }
        );
        assert!(frame_outbound(&scan_frame(SCAN_FLAG_UP | SCAN_FLAG_EXTENDED, 0x1E, 0x41)).is_ok());
    }

    #[test]
    fn fixture_valid_packets_frame_and_decode_to_declared_fields() {
        let fx = fixture();
        for packet in fx["packets"].as_array().unwrap() {
            let name = packet["name"].as_str().unwrap();
            let bytes = bytes_of(&packet["bytes"]);
            let out = frame_outbound(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(out.bytes, &bytes[..], "{name}");
            if let Some(key) = packet.get("key") {
                assert_eq!(out.kind, TYPE_SCAN_KEY, "{name}");
                assert_eq!(out.lane, Lane::Control, "{name}");
                let flags = bytes[1];
                let scan = u16_at(&bytes, 2);
                let vk = u16_at(&bytes, 4);
                assert_eq!(u64::from(scan), key["scan"].as_u64().unwrap_or(0), "{name}");
                assert_eq!(
                    flags & SCAN_FLAG_EXTENDED != 0,
                    key["extended"].as_bool().unwrap_or(false),
                    "{name}"
                );
                assert_eq!(
                    flags & SCAN_FLAG_UP != 0,
                    packet["up"].as_bool().unwrap(),
                    "{name}"
                );
                if let Some(v) = key.get("vk") {
                    assert_eq!(u64::from(vk), v.as_u64().unwrap(), "{name}");
                }
            }
            if let Some(clip) = packet.get("clipboard") {
                assert_eq!(out.kind, TYPE_CLIPBOARD, "{name}");
                assert_eq!(out.lane, Lane::Control, "{name}");
                assert_eq!(
                    u64::from(u16_at(&bytes, 2)),
                    clip["id"].as_u64().unwrap(),
                    "{name}"
                );
                assert_eq!(
                    u64::from(u16_at(&bytes, 4)),
                    clip["index"].as_u64().unwrap(),
                    "{name}"
                );
                assert_eq!(
                    u64::from(u16_at(&bytes, 6)),
                    clip["count"].as_u64().unwrap(),
                    "{name}"
                );
                assert_eq!(
                    bytes[1] & CLIPBOARD_FLAG_PASTE != 0,
                    clip["paste"].as_bool().unwrap(),
                    "{name}"
                );
                assert_eq!(
                    std::str::from_utf8(&bytes[CLIPBOARD_HEADER..]).unwrap(),
                    clip["text"].as_str().unwrap(),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn fixture_invalid_packets_fail_with_the_expected_reason() {
        let fx = fixture();
        let invalid = fx["invalidPackets"].as_array().unwrap();
        let reasons = [
            FrameError::Flags {
                kind: TYPE_SCAN_KEY,
                flags: 4,
            },
            FrameError::Scan,
            FrameError::Scan,
            FrameError::Clipboard,
            FrameError::Clipboard,
            FrameError::Flags {
                kind: TYPE_CLIPBOARD,
                flags: 2,
            },
        ];
        assert_eq!(invalid.len(), reasons.len());
        for (packet, reason) in invalid.iter().zip(reasons) {
            let bytes = bytes_of(packet);
            assert_eq!(frame_outbound(&bytes).unwrap_err(), reason, "{bytes:?}");
        }
    }

    #[test]
    fn fixture_scan_codes_frame_on_the_control_lane() {
        let fx = fixture();
        let codes = fx["scanCodes"].as_object().unwrap();
        assert!(!codes.is_empty());
        for (name, code) in codes {
            let scan = u16::try_from(code["scan"].as_u64().unwrap()).unwrap();
            let flags = if code["extended"].as_bool().unwrap() {
                SCAN_FLAG_EXTENDED
            } else {
                0
            };
            let bytes = scan_frame(flags, scan, 0);
            let frame = frame_outbound(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(frame.lane, Lane::Control, "{name}");
            assert_eq!(frame.kind, TYPE_SCAN_KEY, "{name}");
        }
        for name in fx["mustBeExtended"].as_array().unwrap() {
            let name = name.as_str().unwrap();
            assert_eq!(codes[name]["extended"].as_bool(), Some(true), "{name}");
        }
    }

    #[test]
    fn fixture_vk_only_keys_frame_with_scan_zero() {
        let fx = fixture();
        for (name, vk) in fx["vkOnly"].as_object().unwrap() {
            let vk = u16::try_from(vk.as_u64().unwrap()).unwrap();
            let bytes = scan_frame(0, 0, vk);
            let frame = frame_outbound(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(frame.lane, Lane::Control, "{name}");
        }
    }

    #[test]
    fn constants_match_the_fixture_limits() {
        let fx = fixture();
        let clip = &fx["clipboardPacket"];
        assert_eq!(clip["headerBytes"].as_u64(), Some(CLIPBOARD_HEADER as u64));
        assert_eq!(clip["chunkBytes"].as_u64(), Some(8192));
        assert_eq!(
            clip["maxChunks"].as_u64(),
            Some(u64::from(CLIPBOARD_MAX_CHUNKS))
        );
        assert_eq!(
            clip["flags"]["paste"].as_u64(),
            Some(u64::from(CLIPBOARD_FLAG_PASTE))
        );
        assert_eq!(
            fx["scanKeyPacket"]["type"].as_u64(),
            Some(u64::from(TYPE_SCAN_KEY))
        );
        assert_eq!(fx["scanKeyPacket"]["length"].as_u64(), Some(6));
        assert_eq!(
            fx["scanKeyPacket"]["flags"]["up"].as_u64(),
            Some(u64::from(SCAN_FLAG_UP))
        );
        assert_eq!(
            fx["scanKeyPacket"]["flags"]["extended"].as_u64(),
            Some(u64::from(SCAN_FLAG_EXTENDED))
        );
    }

    #[test]
    fn errors_have_readable_messages() {
        assert_eq!(
            FrameError::UnknownType(2).to_string(),
            "unknown input packet type 2"
        );
        assert_eq!(
            FrameError::Flags { kind: 13, flags: 4 }.to_string(),
            "input packet type 13 has invalid flags 0x4"
        );
    }
}
