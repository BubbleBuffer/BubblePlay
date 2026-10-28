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

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bp_device_key::DeviceId;
use serde_json::{Map, Value};

use crate::limits::{
    ADMIT_MAX, ADMIT_VIEWERS_MAX, AUTH_MAX, CTL_DATA_MAX, CTL_SEQ_MAX, FILE_HEADER_BYTES,
    FILE_LIMIT, MEDIA_CONTROL_MAX, MEDIA_HEADER_BYTES, MEDIA_LIMIT, PAIR_MAX, SIGNAL_BINARY_MAX,
    SIGNAL_LIMIT,
};

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
// Go's map helpers
// ---------------------------------------------------------------------------

/// Go's str helper: the value if it is a JSON string, otherwise the empty
/// string.
fn text<'a>(m: &'a Map<String, Value>, key: &str) -> &'a str {
    m.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Go's isBoolean helper: true only for a JSON true or false.
fn is_boolean(m: &Map<String, Value>, key: &str) -> bool {
    matches!(m.get(key), Some(Value::Bool(_)))
}

/// Go's has helper.
fn has(m: &Map<String, Value>, key: &str) -> bool {
    m.contains_key(key)
}

/// Go's number helper: a finite JSON number. Missing keys, null, non-numbers
/// and numbers that do not fit a float64 (which Go's encoding/json rejects)
/// all yield None.
fn number(m: &Map<String, Value>, key: &str) -> Option<f64> {
    m.get(key)?.as_f64().filter(|value| value.is_finite())
}

/// Go's integer helper: a finite number in min..=max with no fractional part.
///
/// trunc mirrors Go's "value == math.Trunc(value)".
fn integer(m: &Map<String, Value>, key: &str, min: f64, max: f64) -> bool {
    match number(m, key) {
        Some(value) => value >= min && value <= max && value.trunc() == value,
        None => false,
    }
}

/// Go's uint32Value helper: integer in 0..=4294967295.
fn uint32_value(m: &Map<String, Value>, key: &str) -> bool {
    integer(m, key, 0.0, 4_294_967_295.0)
}

/// Go's object helper: the value if it is a JSON object. null, arrays and
/// scalars are None, just as Go's decoder leaves the map nil.
fn object<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    m.get(key)?.as_object()
}

/// Go's only helper: every key of the object is in keys.
fn only(m: &Map<String, Value>, keys: &[&str]) -> bool {
    m.keys().all(|key| keys.contains(&key.as_str()))
}

/// Go's exact helper: the object has exactly these keys.
fn exact(m: &Map<String, Value>, keys: &[&str]) -> bool {
    m.len() == keys.len() && only(m, keys)
}

/// Go's boundedString helper: a JSON string whose byte length is min..=max.
///
/// Go also checks utf8.ValidString; a &str is always valid UTF-8, so that
/// check is implicit here.
fn bounded_string(m: &Map<String, Value>, key: &str, min: usize, max: usize) -> bool {
    match m.get(key).and_then(Value::as_str) {
        Some(value) => value.len() >= min && value.len() <= max,
        None => false,
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
fn is_keyframe_reason(value: &str) -> bool {
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
            SignalKind::Offer | SignalKind::Ice => true,
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

/// Decode a text message the way Go's json.Unmarshal into a map does. A JSON
/// object is returned; null decodes to a nil map, which every validator drops;
/// anything else (not JSON, or an array, string or number) is a decode error
/// that closes the socket.
fn decode_object(raw: &str) -> Result<Value, Invalid> {
    match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(Value::Null) => Err(Invalid::Drop),
        _ => Err(Invalid::Close),
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

// ---------------------------------------------------------------------------
// validSignalMessage
// ---------------------------------------------------------------------------

/// The /ws allowlist (decision 15): Some(kind) when the relay accepts the message
/// from this side, None when it must be dropped. Every other type, including the
/// pre-cutover types, is None and gets the unknown-type handling.
fn valid_signal_message(
    m: &Map<String, Value>,
    host: bool,
    viewer: bool,
    length: usize,
) -> Option<SignalKind> {
    if length > SIGNAL_LIMIT {
        return None;
    }
    match text(m, "type") {
        "bye" => (m.len() == 1).then_some(SignalKind::Bye),
        // Phase 2 and pairing. Exact keys, a direction and a size cap for each.
        "admit" => (host && valid_admit(m, length)).then_some(SignalKind::Admit),
        "pair-open" => (host && exact(m, &["type", "admit"]) && b64url_is(m, "admit", 32))
            .then_some(SignalKind::PairOpen),
        "pair-close" => (host && exact(m, &["type"])).then_some(SignalKind::PairClose),
        "auth-hello" => {
            (viewer && length <= AUTH_MAX && valid_auth_hello(m)).then_some(SignalKind::AuthHello)
        }
        "auth-host" => {
            (host && length <= AUTH_MAX && valid_auth_host(m)).then_some(SignalKind::AuthHost)
        }
        "auth-finish" => (viewer
            && length <= PAIR_MAX
            && exact(m, &["type", "signature"])
            && b64url_is(m, "signature", 64))
        .then_some(SignalKind::AuthFinish),
        "auth-bind" => ((host || viewer)
            && length <= PAIR_MAX
            && exact(m, &["type", "signature"])
            && b64url_is(m, "signature", 64))
        .then_some(SignalKind::AuthBind),
        "pair-proof" => ((host || viewer)
            && length <= PAIR_MAX
            && exact(m, &["type", "mac"])
            && b64url_is(m, "mac", 32))
        .then_some(SignalKind::PairProof),
        "pair-done" => {
            (host && length <= PAIR_MAX && exact(m, &["type"])).then_some(SignalKind::PairDone)
        }
        "pair-denied" => {
            (host && length <= PAIR_MAX && exact(m, &["type"])).then_some(SignalKind::PairDenied)
        }
        "ctl" => ((host || viewer) && valid_ctl(m)).then_some(SignalKind::Ctl),
        _ => None,
    }
}

/// True when `text` is a canonical device ID: it parses, and it prints back to
/// the same characters (hyphenated, upper case).
fn canonical_id(text: &str) -> bool {
    DeviceId::parse(text).is_ok_and(|id| id.to_string() == text)
}

/// The string at `key`, or None when it is missing or not a string.
fn str_of<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    m.get(key).and_then(Value::as_str)
}

/// True when `key` is a canonical unpadded base64url string of exactly `len`
/// bytes (the same rule the relay applies to every key and signature).
fn b64url_is(m: &Map<String, Value>, key: &str, len: usize) -> bool {
    let Some(text) = str_of(m, key) else {
        return false;
    };
    match URL_SAFE_NO_PAD.decode(text) {
        Ok(bytes) => bytes.len() == len && URL_SAFE_NO_PAD.encode(&bytes) == text,
        Err(_) => false,
    }
}

/// Go-style admit: exact keys, a boolean remotePairing, and at most
/// ADMIT_VIEWERS_MAX distinct canonical viewer IDs in a message of at most
/// ADMIT_MAX bytes.
fn valid_admit(m: &Map<String, Value>, length: usize) -> bool {
    if length > ADMIT_MAX || !exact(m, &["type", "viewers", "remotePairing"]) {
        return false;
    }
    if !is_boolean(m, "remotePairing") {
        return false;
    }
    let Some(viewers) = m.get("viewers").and_then(Value::as_array) else {
        return false;
    };
    if viewers.len() > ADMIT_VIEWERS_MAX {
        return false;
    }
    let mut seen = std::collections::BTreeSet::new();
    viewers.iter().all(|value| {
        value
            .as_str()
            .is_some_and(|id| canonical_id(id) && seen.insert(id))
    })
}

/// auth-hello: the viewer's canonical ID, its public key, a nonce and an
/// ephemeral key (32 bytes each), and caps: at most 32 names of 1 to 64 characters
/// from [a-z0-9-].
fn valid_auth_hello(m: &Map<String, Value>) -> bool {
    if !exact(m, &["type", "id", "public", "nonce", "ephemeral", "caps"]) {
        return false;
    }
    if !str_of(m, "id").is_some_and(canonical_id) {
        return false;
    }
    if !(b64url_is(m, "public", 32) && b64url_is(m, "nonce", 32) && b64url_is(m, "ephemeral", 32)) {
        return false;
    }
    let Some(caps) = m.get("caps").and_then(Value::as_array) else {
        return false;
    };
    caps.len() <= 32
        && caps.iter().all(|cap| {
            cap.as_str()
                .is_some_and(|name| (1..=64).contains(&name.len()) && is_cap_name(name))
        })
}

/// auth-host: the same fields as auth-hello with a 64-byte signature in place of caps.
fn valid_auth_host(m: &Map<String, Value>) -> bool {
    exact(
        m,
        &["type", "id", "public", "nonce", "ephemeral", "signature"],
    ) && str_of(m, "id").is_some_and(canonical_id)
        && b64url_is(m, "public", 32)
        && b64url_is(m, "nonce", 32)
        && b64url_is(m, "ephemeral", 32)
        && b64url_is(m, "signature", 64)
}

/// A capability name: lower-case letters, digits and hyphens.
fn is_cap_name(name: &str) -> bool {
    name.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// ctl: exact keys; kind is 1 to 32 characters from [a-z0-9-]; seq is an integer
/// from 0 to 2^53 - 1; data is unpadded base64url of at most CTL_DATA_MAX
/// characters. The relay does not track seq.
fn valid_ctl(m: &Map<String, Value>) -> bool {
    if !exact(m, &["type", "kind", "seq", "data"]) {
        return false;
    }
    let kind_ok =
        str_of(m, "kind").is_some_and(|kind| (1..=32).contains(&kind.len()) && is_cap_name(kind));
    if !kind_ok || !integer(m, "seq", 0.0, CTL_SEQ_MAX) {
        return false;
    }
    match str_of(m, "data") {
        Some(data) => data.len() <= CTL_DATA_MAX && URL_SAFE_NO_PAD.decode(data).is_ok(),
        None => false,
    }
}

/// Go's validMediaControl: media-control text on a media socket.
fn valid_media_control(m: &Map<String, Value>, length: usize) -> bool {
    if length > MEDIA_CONTROL_MAX {
        return false;
    }
    match text(m, "type") {
        "ack" => {
            only(m, &["type", "generation", "seq", "recvTsMs", "decodedSeq"])
                && uint32_value(m, "generation")
                && uint32_value(m, "seq")
                && number(m, "recvTsMs").is_some_and(|stamp| stamp >= 0.0)
                && (!has(m, "decodedSeq") || uint32_value(m, "decodedSeq"))
        }
        "keyframe" => exact(m, &["type", "reason"]) && is_keyframe_reason(text(m, "reason")),
        _ => false,
    }
}

/// Go's validDrop: the pointer a drag was released at.
fn valid_drop(m: &Map<String, Value>) -> bool {
    exact(m, &["x", "y", "generation", "viewportRevision"])
        && integer(m, "x", 0.0, 65535.0)
        && integer(m, "y", 0.0, 65535.0)
        && integer(m, "generation", 1.0, 4_294_967_295.0)
        && uint32_value(m, "viewportRevision")
}

/// Go's validFileMeta: the metadata payload of an OFFER packet.
fn valid_file_meta(m: &Map<String, Value>) -> bool {
    if !bounded_string(m, "name", 1, 1024)
        || !integer(m, "size", 0.0, 2.0 * 1024.0 * 1024.0 * 1024.0)
        || !bounded_string(m, "mime", 0, 255)
        || !integer(m, "mtimeMs", 0.0, 9_007_199_254_740_991.0)
        || !uint32_value(m, "batchId")
        || !integer(m, "batchCount", 1.0, 64.0)
    {
        return false;
    }
    let count = number(m, "batchCount").unwrap_or_default();
    if !integer(m, "batchIndex", 0.0, count - 1.0) {
        return false;
    }
    match text(m, "purpose") {
        "save" | "paste" => !has(m, "drop"),
        "drop" => object(m, "drop").is_some_and(valid_drop),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object_of(raw: &str) -> Map<String, Value> {
        match serde_json::from_str::<Value>(raw).expect("json") {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn field<'a>(map: &'a Map<String, Value>, key: &str) -> &'a Value {
        map.get(key).expect("field")
    }

    // Port of the wrong-JSON-type loop in relay/server_test.go
    // (TestBrowserGeneratedProtocolFixtures).
    #[test]
    fn wrong_json_types_fail_typed_helpers() {
        for (raw, expect_integer, expect_boolean, expect_string) in [
            ("null", false, false, false),
            ("\"0\"", false, false, true),
            ("false", false, true, false),
            ("[]", false, false, false),
            ("{}", false, false, false),
            ("0", true, false, false),
        ] {
            let value = serde_json::from_str::<Value>(raw).expect("json");
            let mut map = Map::new();
            map.insert("value".into(), value);
            assert_eq!(integer(&map, "value", 0.0, 100.0), expect_integer, "{raw}");
            assert_eq!(is_boolean(&map, "value"), expect_boolean, "{raw}");
            assert_eq!(bounded_string(&map, "value", 0, 10), expect_string, "{raw}");
        }
        let map = object_of("{\"value\": null}");
        assert!(!integer(&map, "value", 0.0, 100.0));
        assert!(!is_boolean(&map, "value"));
        assert!(!bounded_string(&map, "value", 0, 100));
    }

    // Go decodes JSON numbers into float64, so all of these behave like Go.
    #[test]
    fn numbers_follow_go_float64_semantics() {
        let map = object_of(
            r#"{"whole":1.0,"exponent":1e3,"negativeZero":-0,"fraction":1.5,
               "uint32":4294967295,"tooBig":4294967296,"negative":-1,
               "biggest":9007199254740991,"rounded":9007199254740993,
               "huge":123456789012345678901234567890}"#,
        );
        assert!(integer(&map, "whole", 1.0, 1.0));
        assert_eq!(number(&map, "exponent"), Some(1000.0));
        assert!(integer(&map, "negativeZero", 0.0, 4.0));
        assert!(!integer(&map, "fraction", 0.0, 10.0));
        assert!(uint32_value(&map, "uint32"));
        assert!(!uint32_value(&map, "tooBig"));
        assert!(!uint32_value(&map, "negative"));
        assert!(integer(&map, "biggest", 0.0, 9_007_199_254_740_991.0));
        // 2^53+1 rounds to 2^53, which is above the limit, as in Go.
        assert_eq!(number(&map, "rounded"), Some(9_007_199_254_740_992.0));
        assert!(!integer(&map, "rounded", 0.0, 9_007_199_254_740_991.0));
        assert!(integer(&map, "huge", 0.0, 1e30));
        // Go's ParseFloat fails on overflow (the field reads as absent) and
        // silently underflows to 0. The document itself stays valid, because
        // only a validator that reads the field decodes it.
        let extreme = object_of(r#"{"outOfRange":1e400,"underflow":1e-400,"negOverflow":-1e400}"#);
        assert_eq!(number(&extreme, "outOfRange"), None);
        assert_eq!(number(&extreme, "negOverflow"), None);
        assert!(!integer(&extreme, "outOfRange", 0.0, 1e308));
        assert_eq!(number(&extreme, "underflow"), Some(0.0));
        assert!(integer(&extreme, "underflow", 0.0, 0.0));
    }

    #[test]
    fn exact_and_only_compare_the_key_set() {
        let map = object_of(r#"{"type":"bye","action":"deny"}"#);
        assert!(only(&map, &["type", "action"]));
        assert!(!only(&map, &["type"]));
        assert!(exact(&map, &["type", "action"]));
        assert!(!exact(&map, &["type"]));
        assert!(!exact(&map, &["type", "action", "extra"]));
    }

    #[test]
    fn bounded_string_counts_bytes() {
        let mut map = Map::new();
        map.insert("value".into(), Value::String("e".repeat(96)));
        assert!(bounded_string(&map, "value", 0, 96));
        map.insert("value".into(), Value::String("\u{e9}".repeat(48)));
        assert_eq!(text(&map, "value").len(), 96);
        assert!(bounded_string(&map, "value", 0, 96));
        map.insert("value".into(), Value::String("\u{e9}".repeat(49)));
        assert!(!bounded_string(&map, "value", 0, 96));
        assert!(!bounded_string(&map, "missing", 0, 100));
    }

    #[test]
    fn keyframe_reason_matches_go_pattern() {
        assert!(is_keyframe_reason("decoder:lost"));
        assert!(is_keyframe_reason(&"a".repeat(64)));
        assert!(!is_keyframe_reason(&"a".repeat(65)));
        assert!(!is_keyframe_reason(""));
        assert!(!is_keyframe_reason("bad reason!"));
        assert!(!is_keyframe_reason("\u{e9}"));
    }

    #[test]
    fn epoch_matches_go_pattern() {
        assert!(validate_epoch("0123456789abcdef0123456789abcdef"));
        assert!(!validate_epoch("0123456789ABCDEF0123456789ABCDEF"));
        assert!(!validate_epoch("0123456789abcdef0123456789abcdeg"));
        assert!(!validate_epoch(""));
        assert!(!validate_epoch("0123456789abcdef0123456789abcdef0"));
    }

    #[test]
    fn text_helper_returns_empty_for_non_strings() {
        let map = object_of(r#"{"s":"x","n":1,"b":true,"z":null}"#);
        assert_eq!(text(&map, "s"), "x");
        assert_eq!(text(&map, "n"), "");
        assert_eq!(text(&map, "b"), "");
        assert_eq!(text(&map, "z"), "");
        assert_eq!(text(&map, "missing"), "");
        assert_eq!(field(&map, "n"), &Value::from(1));
    }
}
