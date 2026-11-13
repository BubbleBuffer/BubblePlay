//! File-socket packets (BPF1) and file-socket text.

use serde_json::{Map, Value};

use super::fields::*;
use super::valid::*;
use super::{Invalid, Side, decode_object};
use crate::limits::{FILE_HEADER_BYTES, FILE_LIMIT};

/// A validated file packet (BPF1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilePacket<'a> {
    /// Packet type.
    pub kind: FileKind,
    /// Bytes after the 16-byte header.
    pub payload: &'a [u8],
}

/// File packet types (Go's validFilePacket).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// 1: file metadata.
    Offer,
    /// 2: transfer accepted.
    Accept,
    /// 3: file bytes.
    Data,
    /// 4: received byte total.
    Ack,
    /// 5: transfer finished, SHA-256 of the file.
    Done,
    /// 6: transfer cancelled.
    Cancel,
    /// 7: receiver result.
    Result,
    /// 8: viewer asks the host for a file.
    Request,
}

impl FileKind {
    /// The header byte for this type.
    pub fn code(self) -> u8 {
        match self {
            FileKind::Offer => 1,
            FileKind::Accept => 2,
            FileKind::Data => 3,
            FileKind::Ack => 4,
            FileKind::Done => 5,
            FileKind::Cancel => 6,
            FileKind::Result => 7,
            FileKind::Request => 8,
        }
    }

    /// The type for a header byte, None outside 1..=8.
    pub fn from_code(code: u8) -> Option<FileKind> {
        Some(match code {
            1 => FileKind::Offer,
            2 => FileKind::Accept,
            3 => FileKind::Data,
            4 => FileKind::Ack,
            5 => FileKind::Done,
            6 => FileKind::Cancel,
            7 => FileKind::Result,
            8 => FileKind::Request,
            _ => return None,
        })
    }
}

/// What a file socket does with a text message that is a ping: Go answers it
/// with pong and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileText {
    /// ping: answer with pong, never relayed.
    Ping,
}

/// Validate a text message on a file socket. Go's forward answers ping and
/// closes the socket on any other text (or on text that is not JSON).
pub fn validate_file_text(raw: &str) -> Result<FileText, Invalid> {
    if raw.len() > FILE_LIMIT {
        return Err(Invalid::Close);
    }
    // Go closes on null too: the file branch returns false for any text that
    // is not a ping, and a null message is not one.
    match decode_object(raw) {
        Ok(value) => match value.as_object() {
            Some(message) if text(message, "type") == "ping" => Ok(FileText::Ping),
            _ => Err(Invalid::Close),
        },
        Err(_) => Err(Invalid::Close),
    }
}

/// Validate a binary frame on a file socket (Go's validFilePacket).
///
/// The side is the socket that sent the packet: REQUEST is only accepted from
/// the viewer.
pub fn validate_file_binary(side: Side, bytes: &[u8]) -> Result<FilePacket<'_>, Invalid> {
    // Every failure here closes the file socket (Go: validFilePacket false).
    if bytes.len() < FILE_HEADER_BYTES || bytes.len() > FILE_LIMIT {
        return Err(Invalid::Close);
    }
    if &bytes[..4] != b"BPF1" || bytes[5] != 0 || bytes[6] != 0 || bytes[7] != 0 {
        return Err(Invalid::Close);
    }
    let kind = FileKind::from_code(bytes[4]).ok_or(Invalid::Close)?;
    let control_seq = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    if kind != FileKind::Data && control_seq != 0 {
        return Err(Invalid::Close);
    }
    let payload = &bytes[FILE_HEADER_BYTES..];
    let packet = FilePacket { kind, payload };
    let valid = match kind {
        FileKind::Accept => payload.is_empty(),
        FileKind::Data => !payload.is_empty(),
        FileKind::Ack => {
            payload.len() == 8
                && u64::from_le_bytes(payload.try_into().unwrap_or_default())
                    <= 9_007_199_254_740_991
        }
        FileKind::Done => payload.len() == 32,
        FileKind::Offer => payload_check(payload, valid_file_meta),
        FileKind::Cancel => payload_check(payload, |m| bounded_string(m, "reason", 0, 256)),
        FileKind::Result => payload_check(payload, |m| {
            is_boolean(m, "ok")
                && (!has(m, "error")
                    || matches!(m.get("error"), Some(Value::Null))
                    || bounded_string(m, "error", 0, 256))
        }),
        FileKind::Request => payload_check(payload, |m| {
            side == Side::Viewer && uint32_value(m, "offerId") && uint32_value(m, "id")
        }),
    };
    if valid {
        Ok(packet)
    } else {
        Err(Invalid::Close)
    }
}

/// Check a control packet payload: 1..=4096 UTF-8 bytes holding a JSON
/// object (Go requires a non-nil map).
fn payload_check<F>(payload: &[u8], check: F) -> bool
where
    F: FnOnce(&Map<String, Value>) -> bool,
{
    if payload.is_empty() || payload.len() > 4096 {
        return false;
    }
    let Ok(body) = std::str::from_utf8(payload) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    match value.as_object() {
        Some(message) => check(message),
        None => false,
    }
}
