//! Message validators, ported from relay/protocol.go.
//!
//! The Go relay decodes every text message into a map[string]json.RawMessage
//! and inspects individual keys through small helpers (str, boolean, number,
//! integer, boundedString, object, only, exact, ...). This module keeps that
//! structure so the two implementations can be compared line by line.
//!
//! Two Go details are load bearing and are reproduced on purpose:
//!
//! * JSON numbers are float64. 1.0, 1e3 and -0 are integers, 1.5 is not, -0
//!   satisfies "value >= 0", and bound checks see the same rounding
//!   encoding/json applies, so the 2^53 limits behave identically.
//! * Lengths are byte lengths, not character counts: of the whole message, of
//!   a string field, and of a binary frame.
//! * Numbers are kept as their literal text (serde_json's arbitrary_precision)
//!   and converted to f64 only when a validator reads them, as Go does. A
//!   literal beyond float64 range is "not a number" in the field that reads it
//!   and does not make the whole document invalid.
//!
//! Nothing here panics or touches the world outside its arguments.

use serde_json::Value;

mod fields;
mod file;
mod valid;

use fields::*;
pub use file::{FileKind, FilePacket, FileText, validate_file_binary, validate_file_text};
use valid::*;

use crate::limits::{MEDIA_HEADER_BYTES, MEDIA_LIMIT, SIGNAL_BINARY_MAX, SIGNAL_LIMIT};

/// A message the relay refuses. Go does one of two things with it, and the
/// adapter must do the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// Go ignores the message and keeps the socket open (validSignalMessage
    /// false, a media-host text, a media frame outside 28..=MEDIA_LIMIT, ...).
    Drop,
    /// Go closes the socket ("invalid packet" / read limit): text that is not a
    /// JSON object, a file-socket text that is not ping, a binary frame that
    /// breaks the file packet rules, or any frame over the socket's read limit.
    /// For a file packet Go also closes the other end of the file pair.
    Close,
}

impl Invalid {
    /// True when the adapter must close the socket.
    pub fn closes_socket(self) -> bool {
        self == Invalid::Close
    }
}

impl core::fmt::Display for Invalid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Invalid::Drop => f.write_str("invalid relay message (dropped)"),
            Invalid::Close => f.write_str("invalid relay message (socket closed)"),
        }
    }
}

impl std::error::Error for Invalid {}

/// The end of a socket a message came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// The PC: /host-ws, /host-media-ws, /host-file-ws.
    Host,
    /// The browser: /ws, /media-ws, /file-ws.
    Viewer,
}

impl Side {
    /// The (host, viewer) flags: exactly one is true for a known side.
    fn flags(self) -> (bool, bool) {
        match self {
            Side::Host => (true, false),
            Side::Viewer => (false, true),
        }
    }
}

// ---------------------------------------------------------------------------
// Go's regular expressions
// ---------------------------------------------------------------------------

/// Go's epochPattern ("^[a-f0-9]{32}$", relay/server.go): the media epoch the
/// relay hands to a viewer and compares against.
pub fn validate_epoch(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

/// Go's "^[A-Za-z0-9_.:-]{1,64}$" (media-control keyframe reason).
pub(super) fn is_keyframe_reason(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

/// The type of a validated signaling message.
///
/// After the phase-2 cutover a signaling socket accepts only the types listed
/// here (decision 15 in dk2-decisions.md). Any other type fails
/// validate_signal_text and gets the unknown-type handling: dropped, socket kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignalKind {
    /// The relay's own ping: answered with pong.
    Ping,
    /// Either side: closes the socket.
    Bye,
    /// Not accepted any more. Named only by SignalMessage::bufferable, which
    /// the pending buffer still consults.
    Offer,
    /// Not accepted any more (see Offer).
    Ice,
    /// Not accepted any more (see Offer).
    FirstFrame,
    /// Not accepted any more (see Offer).
    MediaControl,
    /// Host to relay: the viewer IDs allowed to join. Consumed, never forwarded.
    Admit,
    /// Host to relay: opens a pairing window. Consumed, never forwarded.
    PairOpen,
    /// Host to relay: closes the pairing window. Consumed, never forwarded.
    PairClose,
    /// Viewer to host: the phase-2 handshake's first message.
    AuthHello,
    /// Host to viewer: the host's half of the phase-2 handshake.
    AuthHost,
    /// Viewer to host: the viewer's signature.
    AuthFinish,
    /// Either side: the binding signature.
    AuthBind,
    /// Either side: the pairing MAC.
    PairProof,
    /// Host to viewer: pairing succeeded.
    PairDone,
    /// Host to viewer: pairing was refused.
    PairDenied,
    /// Either side: a control message with an opaque, base64url payload.
    Ctl,
}

impl SignalKind {
    /// The wire name of the message type.
    pub fn as_str(self) -> &'static str {
        match self {
            SignalKind::Ping => "ping",
            SignalKind::Bye => "bye",
            SignalKind::Offer => "offer",
            SignalKind::Ice => "ice",
            SignalKind::FirstFrame => "first-frame",
            SignalKind::MediaControl => "media-control",
            SignalKind::Admit => "admit",
            SignalKind::PairOpen => "pair-open",
            SignalKind::PairClose => "pair-close",
            SignalKind::AuthHello => "auth-hello",
            SignalKind::AuthHost => "auth-host",
            SignalKind::AuthFinish => "auth-finish",
            SignalKind::AuthBind => "auth-bind",
            SignalKind::PairProof => "pair-proof",
            SignalKind::PairDone => "pair-done",
            SignalKind::PairDenied => "pair-denied",
            SignalKind::Ctl => "ctl",
        }
    }

    /// True for the types the router handles itself instead of only forwarding.
    ///
    /// Go's forward answers ping with pong and turns bye into a close. Admit,
    /// pair-open and pair-close are host-to-relay messages the relay consumes.
    pub fn consumed_by_relay(self) -> bool {
        matches!(
            self,
            SignalKind::Ping
                | SignalKind::Bye
                | SignalKind::Admit
                | SignalKind::PairOpen
                | SignalKind::PairClose
        )
    }
}

/// A validated signaling message: its type plus the parsed JSON.
///
/// The adapter forwards raw() verbatim (Go never re-encodes forwarded
/// messages) and reads the parsed fields for its own decisions.
#[derive(Clone, Debug)]
pub struct SignalMessage<'a> {
    raw: &'a str,
    value: Value,
    kind: SignalKind,
}

impl<'a> SignalMessage<'a> {
    /// The message type.
    pub fn kind(&self) -> SignalKind {
        self.kind
    }

    /// The message exactly as it arrived; forward these bytes unchanged.
    pub fn raw(&self) -> &'a str {
        self.raw
    }

    /// The parsed JSON object.
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// A string field of the message, or the empty string (Go's str helper).
    pub fn field(&self, key: &str) -> &str {
        self.value
            .as_object()
            .map(|m| text(m, key))
            .unwrap_or_default()
    }

    /// The action of a media-control or file-drag-preview message.
    pub fn action(&self) -> &str {
        self.field("action")
    }

    /// The phase of a first-frame message.
    pub fn phase(&self) -> &str {
        self.field("phase")
    }

    /// Go's bufferable helper: keep this viewer message while no host is
    /// connected.
    pub fn bufferable(&self) -> bool {
        match self.kind {
            // auth-hello is held only for the next host registration, which
            // sends peer-joined first. The host verifies it as if it had arrived live.
            SignalKind::Offer | SignalKind::Ice | SignalKind::AuthHello => true,
            SignalKind::FirstFrame => self.phase() == "request",
            SignalKind::MediaControl => matches!(self.action(), "start" | "monitors" | "follow"),
            _ => false,
        }
    }

    /// See SignalKind::consumed_by_relay.
    pub fn consumed_by_relay(&self) -> bool {
        self.kind.consumed_by_relay()
    }
}

/// A validated media-socket text message.
#[derive(Clone, Debug, PartialEq)]
pub enum MediaControl {
    /// ping: answered with pong, never relayed. Go matches ping before the
    /// media-socket role check, so either side may send it.
    Ping,
    /// ack: recvTsMs is a finite number that may be fractional
    /// (server_test.go requires a 12.3 ack to survive).
    Ack {
        /// Capture generation being acknowledged.
        generation: u32,
        /// Frame sequence number being acknowledged.
        seq: u32,
        /// Viewer receive time in milliseconds; fractional values are kept.
        recv_ts_ms: f64,
        /// Highest decoded sequence, when the viewer reported one.
        decoded_seq: Option<u32>,
    },
    /// keyframe: the viewer asks the host for a fresh key frame.
    Keyframe {
        /// Reason token, already matched against Go's pattern.
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Validate a text message on a signaling socket.
///
/// Returns the message type and the parsed body so the router can act on it.
/// The epoch is kept for call compatibility. No allowed type checks it any
/// more (the removed media-control, file-drag-preview and first-frame rules
/// were the only users).
///
/// A message longer than SIGNAL_LIMIT is rejected: that is the socket's read
/// limit, so such a message never reaches the Go validator either.
pub fn validate_signal_text<'a>(
    side: Side,
    raw: &'a str,
    _epoch: &str,
) -> Result<SignalMessage<'a>, Invalid> {
    validate_signal_text_for_role(Some(side), raw, _epoch)
}

/// Validate a text message on a signaling socket, given Go's role string.
///
/// Pass Some(side) for a signaling peer (host or viewer) and None for any
/// other socket role (media, file, version probe), where both flags are false.
pub fn validate_signal_text_for_role<'a>(
    side: Option<Side>,
    raw: &'a str,
    _epoch: &str,
) -> Result<SignalMessage<'a>, Invalid> {
    if raw.len() > SIGNAL_LIMIT {
        return Err(Invalid::Close);
    }
    let value = decode_object(raw)?;
    let message = value.as_object().ok_or(Invalid::Close)?;
    // Go's forward answers ping before consulting the validators, and ping is
    // not one of the types validSignalMessage accepts.
    if text(message, "type") == "ping" {
        return Ok(SignalMessage {
            raw,
            value,
            kind: SignalKind::Ping,
        });
    }
    let (host, viewer) = match side {
        Some(side) => side.flags(),
        None => (false, false),
    };
    let kind = valid_signal_message(message, host, viewer, raw.len()).ok_or(Invalid::Drop)?;
    Ok(SignalMessage { raw, value, kind })
}

/// Validate a text message on a media socket: ping, or a media-control message
/// from the viewer.
///
/// The epoch is accepted for symmetry with validate_signal_text but is not
/// used: Go validates media-socket text with validMediaControl, which has no
/// epoch rule. Epoch matching belongs to signaling media-control messages.
pub fn validate_media_text(side: Side, raw: &str, _epoch: &str) -> Result<MediaControl, Invalid> {
    if raw.len() > MEDIA_LIMIT {
        return Err(Invalid::Close);
    }
    let value = decode_object(raw)?;
    let message = value.as_object().ok_or(Invalid::Close)?;
    // Go answers ping before the media branch, so either side may send it.
    if text(message, "type") == "ping" {
        return Ok(MediaControl::Ping);
    }
    // Go drops text from a media host: only the viewer may send media-control
    // (its role check, then validMediaControl).
    if side != Side::Viewer || !valid_media_control(message, raw.len()) {
        return Err(Invalid::Drop);
    }
    match text(message, "type") {
        "ack" => Ok(MediaControl::Ack {
            generation: number(message, "generation").unwrap_or_default() as u32,
            seq: number(message, "seq").unwrap_or_default() as u32,
            recv_ts_ms: number(message, "recvTsMs").unwrap_or_default(),
            decoded_seq: has(message, "decodedSeq")
                .then_some(number(message, "decodedSeq").unwrap_or_default() as u32),
        }),
        _ => Ok(MediaControl::Keyframe {
            reason: text(message, "reason").to_owned(),
        }),
    }
}

/// Validate a binary frame on a media socket. Only the host sends frames, and
/// only frames longer than the 28-byte header are relayed.
///
/// The frame contents (the BPV4 and BPA1 headers, codec, flags) are the
/// viewer's business: see fixtures/wss-v4-contract.json and
/// fixtures/audio-contract.json.
pub fn validate_media_binary(side: Side, bytes: &[u8]) -> Result<(), Invalid> {
    if bytes.len() > MEDIA_LIMIT {
        return Err(Invalid::Close);
    }
    if side != Side::Host || bytes.len() <= MEDIA_HEADER_BYTES {
        return Err(Invalid::Drop);
    }
    Ok(())
}

/// Validate a binary frame on a signaling socket: 1..=16384 bytes. A frame
/// over the socket's 512 KiB read limit closes it; any other size outside the
/// range is dropped.
pub fn validate_signal_binary(bytes: &[u8]) -> Result<(), Invalid> {
    if bytes.len() > SIGNAL_LIMIT {
        return Err(Invalid::Close);
    }
    if bytes.is_empty() || bytes.len() > SIGNAL_BINARY_MAX {
        return Err(Invalid::Drop);
    }
    Ok(())
}

/// Decode a text message the way Go's json.Unmarshal into a map does. A JSON
/// object is returned; null decodes to a nil map, which every validator drops;
/// anything else (not JSON, or an array, string or number) is a decode error
/// that closes the socket.
pub(super) fn decode_object(raw: &str) -> Result<Value, Invalid> {
    match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(Value::Null) => Err(Invalid::Drop),
        _ => Err(Invalid::Close),
    }
}

#[cfg(test)]
mod tests;
