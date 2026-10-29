//! File transfer protocol v1 (pure logic, no sockets or files).
//!
//! The intent and wire contract live in `docs/architecture/file-transfer.md`; the
//! shared vectors are `fixtures/file-transfer-contract.json`, tested from Rust
//! (here), JS (`worker/public/file-transfer.js`) and Kotlin.
//!
//! Every file-socket message is binary: a 16-byte header followed by a payload.
//!
//! | off | size | field                                      |
//! |-----|------|--------------------------------------------|
//! | 0   | 4    | magic ASCII `BPF1`                         |
//! | 4   | 1    | type (1 OFFER .. 8 REQUEST)                |
//! | 5   | 1    | flags = 0                                  |
//! | 6   | 2    | reserved = 0                               |
//! | 8   | 4    | transferId, little endian                  |
//! | 12  | 4    | seq, little endian (DATA chunk index, else 0) |
//!
//! The per-transfer state machines (`Incoming`, `SendWindow`) are shared by
//! the host socket thread and the tests; they never touch the file system.

use serde::{Deserialize, Serialize};

/// `fileTransfer` capability and `protocol` value of `file-offer`.
pub const PROTOCOL_VERSION: u8 = 1;
pub const MAGIC: [u8; 4] = *b"BPF1";
pub const HEADER_BYTES: usize = 16;
pub const MAX_DATA_BYTES: usize = 64 * 1024;
/// OFFER / CANCEL / RESULT / REQUEST payloads are small UTF-8 JSON objects.
pub const MAX_CONTROL_PAYLOAD_BYTES: usize = 4 * 1024;
pub const MAX_MESSAGE_BYTES: usize = HEADER_BYTES + MAX_DATA_BYTES;
/// Sender keeps at most this many bytes un-ACKed.
pub const WINDOW_BYTES: u64 = 1024 * 1024;
/// Receiver ACKs at least every this many bytes ...
pub const ACK_EVERY_BYTES: u64 = 256 * 1024;
/// ... or this often while bytes are un-ACKed.
pub const ACK_EVERY_MS: u64 = 100;
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_BATCH_FILES: u32 = 64;
/// UTF-8 bytes of a file name inside OFFER and file-offer.
pub const MAX_NAME_BYTES: usize = 1024;
pub const MAX_MIME_BYTES: usize = 255;
pub const MAX_REASON_BYTES: usize = 256;
pub const IDLE_CLOSE_MS: u64 = 60_000;
pub const SHA256_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Offer = 1,
    Accept = 2,
    Data = 3,
    Ack = 4,
    Done = 5,
    Cancel = 6,
    Result = 7,
    Request = 8,
}

impl PacketType {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Offer,
            2 => Self::Accept,
            3 => Self::Data,
            4 => Self::Ack,
            5 => Self::Done,
            6 => Self::Cancel,
            7 => Self::Result,
            8 => Self::Request,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Offer => "OFFER",
            Self::Accept => "ACCEPT",
            Self::Data => "DATA",
            Self::Ack => "ACK",
            Self::Done => "DONE",
            Self::Cancel => "CANCEL",
            Self::Result => "RESULT",
            Self::Request => "REQUEST",
        }
    }
}

/// Why a packet was rejected. The string codes are part of the fixture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    Short,
    TooLarge,
    Magic,
    Type,
    Flags,
    Reserved,
    Seq,
    Payload,
    Json,
}

impl Reject {
    pub fn code(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::TooLarge => "too-large",
            Self::Magic => "magic",
            Self::Type => "type",
            Self::Flags => "flags",
            Self::Reserved => "reserved",
            Self::Seq => "seq",
            Self::Payload => "payload",
            Self::Json => "json",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    Save,
    Paste,
    Drop,
}

/// Desktop coordinates normalized to unsigned Q16 (0..=65535).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropPoint {
    pub x: u16,
    pub y: u16,
    pub generation: u32,
    #[serde(rename = "viewportRevision")]
    pub viewport_revision: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OfferMeta {
    pub name: String,
    pub size: u64,
    pub mime: String,
    #[serde(rename = "mtimeMs")]
    pub mtime_ms: u64,
    pub purpose: Purpose,
    #[serde(rename = "batchId")]
    pub batch_id: u32,
    #[serde(rename = "batchIndex")]
    pub batch_index: u32,
    #[serde(rename = "batchCount")]
    pub batch_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop: Option<DropPoint>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CancelPayload {
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultPayload {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestPayload {
    #[serde(rename = "offerId")]
    pub offer_id: u32,
    pub id: u32,
}

/// A decoded, validated packet. Control payloads are already parsed.
#[derive(Clone, Debug, PartialEq)]
pub enum Body<'a> {
    Offer(OfferMeta),
    Accept,
    Data(&'a [u8]),
    Ack(u64),
    Done([u8; SHA256_BYTES]),
    Cancel(CancelPayload),
    Result(ResultPayload),
    Request(RequestPayload),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Packet<'a> {
    pub transfer_id: u32,
    pub seq: u32,
    pub body: Body<'a>,
}

impl Packet<'_> {
    pub fn kind(&self) -> PacketType {
        match self.body {
            Body::Offer(_) => PacketType::Offer,
            Body::Accept => PacketType::Accept,
            Body::Data(_) => PacketType::Data,
            Body::Ack(_) => PacketType::Ack,
            Body::Done(_) => PacketType::Done,
            Body::Cancel(_) => PacketType::Cancel,
            Body::Result(_) => PacketType::Result,
            Body::Request(_) => PacketType::Request,
        }
    }
}

pub fn encode_raw(kind: PacketType, transfer_id: u32, seq: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_BYTES + payload.len());
    out.extend_from_slice(&MAGIC);
    out.push(kind as u8);
    out.push(0);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&transfer_id.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Encode a packet. Panics only on a programming error (an invalid body the
/// decoder would reject), which the tests cover.
pub fn encode(packet: &Packet<'_>) -> Vec<u8> {
    let payload: Vec<u8> = match &packet.body {
        Body::Offer(meta) => json(meta),
        Body::Accept => Vec::new(),
        Body::Data(bytes) => bytes.to_vec(),
        Body::Ack(total) => total.to_le_bytes().to_vec(),
        Body::Done(hash) => hash.to_vec(),
        Body::Cancel(cancel) => json(cancel),
        Body::Result(result) => json(result),
        Body::Request(request) => json(request),
    };
    let seq = if packet.kind() == PacketType::Data {
        packet.seq
    } else {
        0
    };
    encode_raw(packet.kind(), packet.transfer_id, seq, &payload)
}

fn json<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("file transfer payloads always serialize")
}

pub fn decode(bytes: &[u8]) -> Result<Packet<'_>, Reject> {
    if bytes.len() < HEADER_BYTES {
        return Err(Reject::Short);
    }
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(Reject::TooLarge);
    }
    if bytes[0..4] != MAGIC {
        return Err(Reject::Magic);
    }
    let kind = PacketType::from_u8(bytes[4]).ok_or(Reject::Type)?;
    if bytes[5] != 0 {
        return Err(Reject::Flags);
    }
    if bytes[6] != 0 || bytes[7] != 0 {
        return Err(Reject::Reserved);
    }
    let transfer_id = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let seq = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let payload = &bytes[HEADER_BYTES..];
    if kind != PacketType::Data && seq != 0 {
        return Err(Reject::Seq);
    }
    let body = match kind {
        PacketType::Accept => {
            if !payload.is_empty() {
                return Err(Reject::Payload);
            }
            Body::Accept
        }
        PacketType::Data => {
            if payload.is_empty() {
                return Err(Reject::Payload);
            }
            Body::Data(payload)
        }
        PacketType::Ack => {
            let total: [u8; 8] = payload.try_into().map_err(|_| Reject::Payload)?;
            Body::Ack(u64::from_le_bytes(total))
        }
        PacketType::Done => {
            let hash: [u8; SHA256_BYTES] = payload.try_into().map_err(|_| Reject::Payload)?;
            Body::Done(hash)
        }
        PacketType::Offer | PacketType::Cancel | PacketType::Result | PacketType::Request => {
            if payload.is_empty() || payload.len() > MAX_CONTROL_PAYLOAD_BYTES {
                return Err(Reject::Payload);
            }
            decode_control(kind, payload)?
        }
    };
    Ok(Packet {
        transfer_id,
        seq,
        body,
    })
}

fn decode_control(kind: PacketType, payload: &[u8]) -> Result<Body<'static>, Reject> {
    let text = std::str::from_utf8(payload).map_err(|_| Reject::Json)?;
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| Reject::Json)?;
    if !value.is_object() {
        return Err(Reject::Json);
    }
    match kind {
        PacketType::Offer => {
            let meta: OfferMeta = serde_json::from_value(value).map_err(|_| Reject::Json)?;
            if !valid_offer_meta(&meta) {
                return Err(Reject::Json);
            }
            Ok(Body::Offer(meta))
        }
        PacketType::Cancel => {
            let cancel: CancelPayload = serde_json::from_value(value).map_err(|_| Reject::Json)?;
            if cancel.reason.len() > MAX_REASON_BYTES {
                return Err(Reject::Json);
            }
            Ok(Body::Cancel(cancel))
        }
        PacketType::Result => {
            let result: ResultPayload = serde_json::from_value(value).map_err(|_| Reject::Json)?;
            if result
                .error
                .as_ref()
                .is_some_and(|error| error.len() > MAX_REASON_BYTES)
            {
                return Err(Reject::Json);
            }
            Ok(Body::Result(result))
        }
        PacketType::Request => {
            let request: RequestPayload =
                serde_json::from_value(value).map_err(|_| Reject::Json)?;
            Ok(Body::Request(request))
        }
        _ => unreachable!("only control packets carry JSON"),
    }
}

pub fn valid_offer_meta(meta: &OfferMeta) -> bool {
    !meta.name.is_empty()
        && meta.name.len() <= MAX_NAME_BYTES
        && meta.size <= MAX_FILE_BYTES
        && meta.mime.len() <= MAX_MIME_BYTES
        && meta.batch_count >= 1
        && meta.batch_count <= MAX_BATCH_FILES
        && meta.batch_index < meta.batch_count
        && ((meta.purpose == Purpose::Drop) == meta.drop.is_some())
        && meta.drop.is_none_or(|point| point.generation != 0)
}

/// Number of DATA chunks a file of `size` bytes is sent in.
pub fn chunk_count(size: u64) -> u64 {
    size.div_ceil(MAX_DATA_BYTES as u64)
}

/// Receiver-side failure codes (`RESULT.error` / `CANCEL.reason`). Part of
/// the fixture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferError {
    SeqGap,
    SizeMismatch,
    HashMismatch,
    Unexpected,
    TooLarge,
}

impl TransferError {
    pub fn code(self) -> &'static str {
        match self {
            Self::SeqGap => "seq-gap",
            Self::SizeMismatch => "size-mismatch",
            Self::HashMismatch => "hash-mismatch",
            Self::Unexpected => "unexpected",
            Self::TooLarge => "too-large",
        }
    }
}

/// One incoming file: tracks DATA order, byte count and when to ACK. The
/// caller writes bytes and computes the hash; this only decides.
#[derive(Debug)]
pub struct Incoming {
    pub transfer_id: u32,
    pub size: u64,
    next_seq: u32,
    received: u64,
    acked: u64,
    last_ack_ms: u64,
}

impl Incoming {
    pub fn new(transfer_id: u32, meta: &OfferMeta) -> Result<Self, TransferError> {
        if meta.size > MAX_FILE_BYTES {
            return Err(TransferError::TooLarge);
        }
        Ok(Self {
            transfer_id,
            size: meta.size,
            next_seq: 0,
            received: 0,
            acked: 0,
            last_ack_ms: 0,
        })
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    /// Account for one DATA packet (after the caller decoded it). The caller
    /// writes the bytes only when this returns Ok.
    pub fn on_data(&mut self, seq: u32, len: usize) -> Result<(), TransferError> {
        if seq != self.next_seq {
            return Err(TransferError::SeqGap);
        }
        let received = self.received + len as u64;
        if received > self.size {
            return Err(TransferError::SizeMismatch);
        }
        self.next_seq = self.next_seq.wrapping_add(1);
        self.received = received;
        Ok(())
    }

    /// The total to ACK now, if one is due (every 256 KiB, every 100 ms
    /// while bytes are un-ACKed, and once the whole file arrived).
    pub fn ack_due(&mut self, now_ms: u64) -> Option<u64> {
        if self.received == self.acked {
            return None;
        }
        let due = self.received - self.acked >= ACK_EVERY_BYTES
            || now_ms.saturating_sub(self.last_ack_ms) >= ACK_EVERY_MS
            || self.received == self.size;
        if !due {
            return None;
        }
        self.acked = self.received;
        self.last_ack_ms = now_ms;
        Some(self.received)
    }

    /// DONE arrived with `expected`; `actual` is the receiver's own hash.
    pub fn on_done(
        &self,
        expected: &[u8; SHA256_BYTES],
        actual: &[u8; SHA256_BYTES],
    ) -> Result<(), TransferError> {
        if self.received != self.size {
            return Err(TransferError::SizeMismatch);
        }
        if expected != actual {
            return Err(TransferError::HashMismatch);
        }
        Ok(())
    }
}

/// Sender-side flow control: at most `WINDOW_BYTES` un-ACKed.
#[derive(Debug, Default)]
pub struct SendWindow {
    sent: u64,
    acked: u64,
}

impl SendWindow {
    pub fn sent(&self) -> u64 {
        self.sent
    }

    pub fn acked(&self) -> u64 {
        self.acked
    }

    pub fn can_send(&self, len: usize) -> bool {
        self.sent - self.acked + len as u64 <= WINDOW_BYTES
    }

    pub fn on_sent(&mut self, len: usize) {
        self.sent += len as u64;
    }

    /// ACKs carry a running total; they must not go backwards or exceed
    /// what was sent.
    pub fn on_ack(&mut self, total: u64) -> Result<(), TransferError> {
        if total < self.acked || total > self.sent {
            return Err(TransferError::Unexpected);
        }
        self.acked = total;
        Ok(())
    }
}

/// Pace file bytes so video keeps priority. `cap_bps == 0` means no cap.
#[derive(Debug)]
pub struct RateLimiter {
    tokens: f64,
    last_ms: u64,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    /// Burst allowance: 50 ms worth of bytes, never less than one chunk.
    const BURST_MS: f64 = 50.0;

    pub fn new() -> Self {
        Self {
            tokens: MAX_DATA_BYTES as f64,
            last_ms: 0,
        }
    }

    /// Milliseconds to wait before `len` bytes may go out (0 = now). Spends
    /// the tokens when it returns 0.
    pub fn wait_ms(&mut self, now_ms: u64, cap_bps: u64, len: usize) -> u64 {
        if cap_bps == 0 {
            self.last_ms = now_ms;
            return 0;
        }
        let bytes_per_ms = cap_bps as f64 / 8_000.0;
        let burst = (bytes_per_ms * Self::BURST_MS).max(MAX_DATA_BYTES as f64);
        let elapsed = now_ms.saturating_sub(self.last_ms) as f64;
        self.last_ms = now_ms;
        self.tokens = (self.tokens + elapsed * bytes_per_ms).min(burst);
        if self.tokens >= len as f64 {
            self.tokens -= len as f64;
            return 0;
        }
        (((len as f64 - self.tokens) / bytes_per_ms).ceil() as u64).max(1)
    }
}

/// The file share of the link: what video does not use, at least 10 % of
/// the capacity. `None` capacity (link not constrained, or no media
/// session) means no cap.
pub fn file_rate_cap_bps(capacity_bps: Option<u64>, video_bps: u64) -> u64 {
    match capacity_bps {
        Some(capacity) if capacity > 0 => capacity.saturating_sub(video_bps).max(capacity / 10),
        _ => 0,
    }
}

/// Make `name` safe as a single Windows file name: no path separators or
/// control/reserved characters, no reserved device names, no trailing dots
/// or spaces, at most 200 UTF-16 units. Never returns an empty name.
pub fn sanitize_file_name(name: &str) -> String {
    // A sender may still put a path into the name: keep only the last part.
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out: String = base
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            ch if (ch as u32) < 0x20 || ch == '\u{7f}' => '_',
            // Bidi overrides can disguise an extension (e.g. "fdp.exe").
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => '_',
            ch => ch,
        })
        .collect();
    let trimmed_len = out.trim_end_matches(['.', ' ']).len();
    out.truncate(trimmed_len);
    let trimmed_start = out.len() - out.trim_start_matches(' ').len();
    out.drain(..trimmed_start);
    if out.is_empty() || out.chars().all(|ch| ch == '.') {
        out = "file".to_string();
    }
    let stem_upper = out
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    const RESERVED: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];
    // COM0-9 / LPT0-9, plus the superscript digits Windows also reserves.
    let numbered = |prefix: &str| {
        stem_upper.strip_prefix(prefix).is_some_and(|rest| {
            let mut chars = rest.chars();
            matches!(
                (chars.next(), chars.next()),
                (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
            )
        })
    };
    let reserved = RESERVED.contains(&stem_upper.as_str()) || numbered("COM") || numbered("LPT");
    if reserved {
        out.insert(0, '_');
    }
    truncate_utf16(&out, 200)
}

/// Shorten to at most `max_units` UTF-16 units, keeping the extension.
fn truncate_utf16(name: &str, max_units: usize) -> String {
    let units = |text: &str| text.encode_utf16().count();
    if units(name) <= max_units {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 && units(&name[dot..]) <= 16 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    let budget = max_units - units(ext);
    let mut kept = String::new();
    let mut used = 0;
    for ch in stem.chars() {
        if used + ch.len_utf16() > budget {
            break;
        }
        used += ch.len_utf16();
        kept.push(ch);
    }
    let kept = kept.trim_end_matches(['.', ' ']).to_string();
    format!("{kept}{ext}")
}

/// The n-th collision variant: "name (n).ext".
pub fn collision_name(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({n}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({n})"),
    }
}

/// Signaling `file-offer` (host -> client on `/ws`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileOfferItem {
    pub id: u32,
    pub name: String,
    pub size: u64,
}

pub fn file_offer_message(offer_id: u32, items: &[FileOfferItem], source: &str) -> String {
    serde_json::json!({
        "type": "file-offer",
        "protocol": PROTOCOL_VERSION,
        "offerId": offer_id,
        "items": items,
        "source": source,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../fixtures/file-transfer-contract.json")).unwrap()
    }

    fn hex(value: &str) -> Vec<u8> {
        (0..value.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&value[at..at + 2], 16).unwrap())
            .collect()
    }

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn fixture_limits_match() {
        let contract = fixture();
        let limits = &contract["limits"];
        assert_eq!(contract["magic"], "BPF1");
        assert_eq!(contract["headerBytes"], HEADER_BYTES);
        assert_eq!(limits["maxDataBytes"], MAX_DATA_BYTES);
        assert_eq!(limits["maxControlPayloadBytes"], MAX_CONTROL_PAYLOAD_BYTES);
        assert_eq!(limits["maxMessageBytes"], MAX_MESSAGE_BYTES);
        assert_eq!(limits["windowBytes"], WINDOW_BYTES);
        assert_eq!(limits["ackEveryBytes"], ACK_EVERY_BYTES);
        assert_eq!(limits["ackEveryMs"], ACK_EVERY_MS);
        assert_eq!(limits["maxFileBytes"], MAX_FILE_BYTES);
        assert_eq!(limits["maxBatchFiles"], MAX_BATCH_FILES);
        assert_eq!(limits["maxNameBytes"], MAX_NAME_BYTES);
        assert_eq!(limits["idleCloseMs"], IDLE_CLOSE_MS);
        for (name, value) in contract["types"].as_object().unwrap() {
            let kind = PacketType::from_u8(value.as_u64().unwrap() as u8).unwrap();
            assert_eq!(kind.name(), name);
        }
        let header = &contract["header"];
        assert_eq!(header["type"], 4);
        assert_eq!(header["transferId"], 8);
        assert_eq!(header["seq"], 12);
    }

    #[test]
    fn fixture_packets_round_trip() {
        let contract = fixture();
        for case in contract["packets"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let bytes = hex(case["hex"].as_str().unwrap());
            let packet = decode(&bytes).unwrap_or_else(|error| panic!("{name}: {error:?}"));
            assert_eq!(packet.kind().name(), case["type"], "{name}");
            assert_eq!(packet.transfer_id as u64, case["transferId"], "{name}");
            assert_eq!(packet.seq as u64, case["seq"], "{name}");
            assert_eq!(
                to_hex(&bytes[HEADER_BYTES..]),
                case["payloadHex"].as_str().unwrap(),
                "{name}"
            );
            let decoded = &case["decoded"];
            match &packet.body {
                Body::Offer(meta) => {
                    assert_eq!(serde_json::to_value(meta).unwrap(), *decoded, "{name}")
                }
                Body::Cancel(cancel) => {
                    assert_eq!(serde_json::to_value(cancel).unwrap(), *decoded)
                }
                Body::Result(result) => {
                    assert_eq!(serde_json::to_value(result).unwrap(), *decoded)
                }
                Body::Request(request) => {
                    assert_eq!(serde_json::to_value(request).unwrap(), *decoded)
                }
                Body::Ack(total) => assert_eq!(*total, decoded["total"].as_u64().unwrap()),
                Body::Done(hash) => assert_eq!(to_hex(hash), decoded["sha256"]),
                Body::Data(data) => assert_eq!(to_hex(data), decoded["dataHex"]),
                Body::Accept => assert!(decoded.is_null()),
            }
            assert_eq!(encode(&packet), bytes, "{name} re-encodes byte-exact");
        }
    }

    #[test]
    fn fixture_rejected_packets_fail_with_their_reason() {
        let contract = fixture();
        for case in contract["rejected"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let bytes = match case["hex"].as_str() {
                Some(text) => hex(text),
                None => {
                    // Oversize vectors are described, not spelled out.
                    let mut bytes = hex(case["headerHex"].as_str().unwrap());
                    bytes.resize(case["totalBytes"].as_u64().unwrap() as usize, 0xab);
                    bytes
                }
            };
            let error = decode(&bytes).expect_err(name);
            assert_eq!(error.code(), case["reason"], "{name}");
        }
    }

    #[test]
    fn fixture_receiver_sessions() {
        let contract = fixture();
        for session in contract["receiverSessions"].as_array().unwrap() {
            let name = session["name"].as_str().unwrap();
            let mut incoming: Option<Incoming> = None;
            let mut hasher = crate::file_hash::Sha256::new().unwrap();
            let mut outcome = "ok".to_string();
            for step in session["packets"].as_array().unwrap() {
                let bytes = hex(step.as_str().unwrap());
                let packet = decode(&bytes).unwrap();
                let result = match packet.body {
                    Body::Offer(meta) => Incoming::new(packet.transfer_id, &meta).map(|value| {
                        incoming = Some(value);
                    }),
                    Body::Data(data) => {
                        let state = incoming.as_mut().unwrap();
                        state.on_data(packet.seq, data.len()).map(|()| {
                            hasher.update(data);
                        })
                    }
                    Body::Done(expected) => {
                        let actual = std::mem::replace(
                            &mut hasher,
                            crate::file_hash::Sha256::new().unwrap(),
                        )
                        .finish();
                        incoming.as_ref().unwrap().on_done(&expected, &actual)
                    }
                    _ => Err(TransferError::Unexpected),
                };
                if let Err(error) = result {
                    outcome = error.code().to_string();
                    break;
                }
            }
            assert_eq!(outcome, session["outcome"].as_str().unwrap(), "{name}");
        }
    }

    #[test]
    fn fixture_signaling_offers_parse() {
        let contract = fixture();
        for case in contract["signaling"]["fileOffer"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            assert_eq!(
                valid_file_offer_text(text),
                case["accepted"].as_bool().unwrap(),
                "{text}"
            );
        }
        let items = [FileOfferItem {
            id: 1,
            name: "report.pdf".into(),
            size: 1234,
        }];
        let built = file_offer_message(7, &items, "clipboard");
        assert!(valid_file_offer_text(&built));
        assert_eq!(
            serde_json::from_str::<Value>(&built).unwrap(),
            serde_json::from_str::<Value>(
                contract["signaling"]["fileOffer"][0]["text"]
                    .as_str()
                    .unwrap()
            )
            .unwrap()
        );
    }

    /// Mirrors `valid_file_meta` in relay-rs/core/src/protocol.rs.
    fn valid_file_offer_text(text: &str) -> bool {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return false;
        };
        let Some(items) = value["items"].as_array() else {
            return false;
        };
        value["type"] == "file-offer"
            && value["protocol"] == 1
            && value["offerId"]
                .as_u64()
                .is_some_and(|id| id <= u32::MAX as u64)
            && matches!(value["source"].as_str(), Some("clipboard" | "sendto"))
            && items.len() <= MAX_BATCH_FILES as usize
            && items.iter().all(|item| {
                item["id"].as_u64().is_some_and(|id| id <= u32::MAX as u64)
                    && item["name"]
                        .as_str()
                        .is_some_and(|name| !name.is_empty() && name.len() <= MAX_NAME_BYTES)
                    && item["size"]
                        .as_u64()
                        .is_some_and(|size| size <= MAX_FILE_BYTES)
            })
    }

    #[test]
    fn window_and_ack_flow_control() {
        let mut window = SendWindow::default();
        let mut sent_chunks = 0;
        while window.can_send(MAX_DATA_BYTES) {
            window.on_sent(MAX_DATA_BYTES);
            sent_chunks += 1;
        }
        assert_eq!(sent_chunks as u64 * MAX_DATA_BYTES as u64, WINDOW_BYTES);
        assert!(!window.can_send(1));
        assert_eq!(
            window.on_ack(WINDOW_BYTES + 1),
            Err(TransferError::Unexpected)
        );
        window.on_ack(ACK_EVERY_BYTES).unwrap();
        assert_eq!(
            window.on_ack(ACK_EVERY_BYTES - 1),
            Err(TransferError::Unexpected)
        );
        assert!(window.can_send(ACK_EVERY_BYTES as usize));
        assert!(!window.can_send(ACK_EVERY_BYTES as usize + 1));

        // Receiver: ACK every 256 KiB, every 100 ms, and at the end.
        let meta = OfferMeta {
            name: "a".into(),
            size: 600 * 1024,
            mime: String::new(),
            mtime_ms: 0,
            purpose: Purpose::Save,
            batch_id: 1,
            batch_index: 0,
            batch_count: 1,
            drop: None,
        };
        let mut incoming = Incoming::new(1, &meta).unwrap();
        let mut acks = Vec::new();
        let mut now = 1_000;
        for seq in 0..5 {
            incoming.on_data(seq, MAX_DATA_BYTES).unwrap();
            if let Some(total) = incoming.ack_due(now) {
                acks.push(total);
            }
            now += 1;
        }
        // First DATA is >100 ms after the (zero) last ACK, then 256 KiB more.
        assert_eq!(acks, vec![65_536, 5 * 65_536]);
        incoming.on_data(5, MAX_DATA_BYTES).unwrap();
        assert_eq!(incoming.ack_due(now), None);
        assert_eq!(incoming.ack_due(now + ACK_EVERY_MS), Some(6 * 65_536));
        for seq in 6..9 {
            incoming.on_data(seq, MAX_DATA_BYTES).unwrap();
        }
        incoming.on_data(9, 600 * 1024 - 9 * 65_536).unwrap();
        assert_eq!(incoming.ack_due(now + ACK_EVERY_MS + 1), Some(600 * 1024));
        assert_eq!(
            incoming.on_data(10, 1),
            Err(TransferError::SizeMismatch),
            "bytes past the offered size"
        );
    }

    #[test]
    fn rate_cap_leaves_video_its_share() {
        assert_eq!(file_rate_cap_bps(None, 5_000_000), 0);
        assert_eq!(file_rate_cap_bps(Some(10_000_000), 4_000_000), 6_000_000);
        assert_eq!(file_rate_cap_bps(Some(10_000_000), 9_900_000), 1_000_000);
        let mut limiter = RateLimiter::new();
        assert_eq!(limiter.wait_ms(0, 0, MAX_DATA_BYTES), 0);
        // 8 Mbps = 1000 bytes/ms: the first chunk rides the burst, the next
        // must wait about 65 ms.
        let mut limiter = RateLimiter::new();
        assert_eq!(limiter.wait_ms(1, 8_000_000, MAX_DATA_BYTES), 0);
        let wait = limiter.wait_ms(1, 8_000_000, MAX_DATA_BYTES);
        assert!((60..=70).contains(&wait), "{wait}");
        assert_eq!(limiter.wait_ms(1 + wait, 8_000_000, MAX_DATA_BYTES), 0);
    }

    #[test]
    fn names_are_sanitised() {
        let cases = [
            ("report.pdf", "report.pdf"),
            ("..\\..\\Windows\\evil.exe", "evil.exe"),
            ("a/b/c.txt", "c.txt"),
            ("what?.txt", "what_.txt"),
            ("CON", "_CON"),
            ("con.txt", "_con.txt"),
            ("LPT1.log", "_LPT1.log"),
            ("COM10.txt", "COM10.txt"),
            ("trailing. . ", "trailing"),
            ("  lead.txt", "lead.txt"),
            ("...", "file"),
            ("", "file"),
            ("..", "file"),
            ("tab\there.txt", "tab_here.txt"),
            ("gnp\u{202e}.exe", "gnp_.exe"),
            ("a:b|c<d>e\"f*g", "a_b_c_d_e_f_g"),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_file_name(input), expected, "{input:?}");
        }
        let long = format!("{}.txt", "x".repeat(400));
        let short = sanitize_file_name(&long);
        assert_eq!(short.encode_utf16().count(), 200);
        assert!(short.ends_with(".txt"));
        assert_eq!(collision_name("a.txt", 2), "a (2).txt");
        assert_eq!(collision_name("README", 3), "README (3)");
        assert_eq!(collision_name(".env", 1), ".env (1)");
    }

    #[test]
    fn decoder_rejects_inconsistent_control_payloads() {
        let offer = |json: &str| encode_raw(PacketType::Offer, 1, 0, json.as_bytes());
        let ok = r#"{"name":"a","size":1,"mime":"","mtimeMs":0,"purpose":"save","batchId":1,"batchIndex":0,"batchCount":1}"#;
        assert!(decode(&offer(ok)).is_ok());
        for bad in [
            r#"{"name":"","size":1,"mime":"","mtimeMs":0,"purpose":"save","batchId":1,"batchIndex":0,"batchCount":1}"#,
            r#"{"name":"a","size":2147483649,"mime":"","mtimeMs":0,"purpose":"save","batchId":1,"batchIndex":0,"batchCount":1}"#,
            r#"{"name":"a","size":1,"mime":"","mtimeMs":0,"purpose":"open","batchId":1,"batchIndex":0,"batchCount":1}"#,
            r#"{"name":"a","size":1,"mime":"","mtimeMs":0,"purpose":"save","batchId":1,"batchIndex":1,"batchCount":1}"#,
            r#"{"name":"a","size":1,"mime":"","mtimeMs":0,"purpose":"save","batchId":1,"batchIndex":0,"batchCount":65}"#,
            r#"{"name":"a","size":1.5,"mime":"","mtimeMs":0,"purpose":"save","batchId":1,"batchIndex":0,"batchCount":1}"#,
            r#"[1]"#,
        ] {
            assert_eq!(decode(&offer(bad)), Err(Reject::Json), "{bad}");
        }
        assert_eq!(
            decode(&encode_raw(PacketType::Accept, 1, 0, b"x")),
            Err(Reject::Payload)
        );
        assert_eq!(
            decode(&encode_raw(PacketType::Ack, 1, 0, &[0; 7])),
            Err(Reject::Payload)
        );
        assert_eq!(
            decode(&encode_raw(PacketType::Done, 1, 0, &[0; 31])),
            Err(Reject::Payload)
        );
        assert_eq!(
            decode(&encode_raw(PacketType::Data, 1, 0, &[])),
            Err(Reject::Payload)
        );
        assert_eq!(
            decode(&encode_raw(PacketType::Ack, 1, 3, &[0; 8])),
            Err(Reject::Seq)
        );
    }

    /// Prints the fixture body; run with
    /// `cargo test --lib file_transfer::tests::print_fixture -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn print_fixture() {
        let mut packets = Vec::new();
        let offer = OfferMeta {
            name: "Screenshot 2026-10-01.png".into(),
            size: 5,
            mime: "image/png".into(),
            mtime_ms: 1_790_000_000_000,
            purpose: Purpose::Paste,
            batch_id: 3,
            batch_index: 0,
            batch_count: 1,
            drop: None,
        };
        let sha_hello = {
            let mut hasher = crate::file_hash::Sha256::new().unwrap();
            hasher.update(b"hello");
            hasher.finish()
        };
        let cases: Vec<(&str, Packet, Value)> = vec![
            (
                "offer",
                Packet {
                    transfer_id: 7,
                    seq: 0,
                    body: Body::Offer(offer.clone()),
                },
                serde_json::to_value(&offer).unwrap(),
            ),
            (
                "accept",
                Packet {
                    transfer_id: 7,
                    seq: 0,
                    body: Body::Accept,
                },
                Value::Null,
            ),
            (
                "data",
                Packet {
                    transfer_id: 7,
                    seq: 0,
                    body: Body::Data(b"hello"),
                },
                serde_json::json!({"dataHex": to_hex(b"hello")}),
            ),
            (
                "data-seq-1",
                Packet {
                    transfer_id: 0x0102_0304,
                    seq: 1,
                    body: Body::Data(&[0xff]),
                },
                serde_json::json!({"dataHex": "ff"}),
            ),
            (
                "ack",
                Packet {
                    transfer_id: 7,
                    seq: 0,
                    body: Body::Ack(5),
                },
                serde_json::json!({"total": 5}),
            ),
            (
                "ack-large",
                Packet {
                    transfer_id: 7,
                    seq: 0,
                    body: Body::Ack(MAX_FILE_BYTES),
                },
                serde_json::json!({"total": MAX_FILE_BYTES}),
            ),
            (
                "done",
                Packet {
                    transfer_id: 7,
                    seq: 0,
                    body: Body::Done(sha_hello),
                },
                serde_json::json!({"sha256": to_hex(&sha_hello)}),
            ),
            (
                "cancel",
                Packet {
                    transfer_id: 7,
                    seq: 0,
                    body: Body::Cancel(CancelPayload {
                        reason: "user".into(),
                    }),
                },
                serde_json::json!({"reason": "user"}),
            ),
            (
                "result-ok",
                Packet {
                    transfer_id: 7,
                    seq: 0,
                    body: Body::Result(ResultPayload {
                        ok: true,
                        error: None,
                    }),
                },
                serde_json::json!({"ok": true}),
            ),
            (
                "result-error",
                Packet {
                    transfer_id: 7,
                    seq: 0,
                    body: Body::Result(ResultPayload {
                        ok: false,
                        error: Some("hash-mismatch".into()),
                    }),
                },
                serde_json::json!({"ok": false, "error": "hash-mismatch"}),
            ),
            (
                "request",
                Packet {
                    transfer_id: 9,
                    seq: 0,
                    body: Body::Request(RequestPayload { offer_id: 4, id: 2 }),
                },
                serde_json::json!({"offerId": 4, "id": 2}),
            ),
        ];
        for (name, packet, decoded) in cases {
            let bytes = encode(&packet);
            packets.push(serde_json::json!({
                "name": name,
                "hex": to_hex(&bytes),
                "type": packet.kind().name(),
                "transferId": packet.transfer_id,
                "seq": packet.seq,
                "payloadHex": to_hex(&bytes[HEADER_BYTES..]),
                "decoded": decoded,
            }));
        }
        let accept = to_hex(&encode_raw(PacketType::Accept, 7, 0, &[]));
        let mut rejected = Vec::new();
        let mut reject = |name: &str, bytes: Vec<u8>| {
            let reason = decode(&bytes).unwrap_err().code();
            rejected
                .push(serde_json::json!({"name": name, "hex": to_hex(&bytes), "reason": reason}));
        };
        let with = |at: usize, value: u8| {
            let mut bytes = hex(&accept);
            bytes[at] = value;
            bytes
        };
        reject("truncated-header", hex(&accept)[..15].to_vec());
        reject("bad-magic", with(3, b'2'));
        reject("unknown-type-0", with(4, 0));
        reject("unknown-type-9", with(4, 9));
        reject("nonzero-flags", with(5, 1));
        reject("nonzero-reserved", with(7, 1));
        reject("nonzero-seq-on-control", with(12, 1));
        reject(
            "accept-with-payload",
            encode_raw(PacketType::Accept, 7, 0, b"x"),
        );
        reject("empty-data", encode_raw(PacketType::Data, 7, 0, &[]));
        reject(
            "short-ack",
            encode_raw(PacketType::Ack, 7, 0, &[5, 0, 0, 0]),
        );
        reject("short-done", encode_raw(PacketType::Done, 7, 0, &[0; 16]));
        reject(
            "offer-not-json",
            encode_raw(PacketType::Offer, 7, 0, b"{name:"),
        );
        reject(
            "offer-missing-size",
            encode_raw(
                PacketType::Offer,
                7,
                0,
                br#"{"name":"a","mime":"","mtimeMs":0,"purpose":"save","batchId":1,"batchIndex":0,"batchCount":1}"#,
            ),
        );
        reject(
            "offer-too-big-file",
            encode_raw(
                PacketType::Offer,
                7,
                0,
                br#"{"name":"a","size":2147483649,"mime":"","mtimeMs":0,"purpose":"save","batchId":1,"batchIndex":0,"batchCount":1}"#,
            ),
        );
        reject(
            "offer-bad-purpose",
            encode_raw(
                PacketType::Offer,
                7,
                0,
                br#"{"name":"a","size":1,"mime":"","mtimeMs":0,"purpose":"open","batchId":1,"batchIndex":0,"batchCount":1}"#,
            ),
        );
        reject(
            "request-not-object",
            encode_raw(PacketType::Request, 9, 0, b"[4,2]"),
        );
        reject(
            "cancel-invalid-utf8",
            encode_raw(PacketType::Cancel, 7, 0, &[0x7b, 0xff, 0x7d]),
        );
        let header = encode_raw(PacketType::Data, 7, 0, &[]);
        let total = HEADER_BYTES + MAX_DATA_BYTES + 1;
        let mut oversize = header.clone();
        oversize.resize(total, 0xab);
        assert_eq!(decode(&oversize).unwrap_err(), Reject::TooLarge);
        rejected.push(serde_json::json!({
            "name": "oversize-data",
            "headerHex": to_hex(&header),
            "totalBytes": total,
            "reason": "too-large",
            "note": "header followed by 65537 bytes of 0xab",
        }));

        let offer_bytes = |size: u64| {
            to_hex(&encode(&Packet {
                transfer_id: 11,
                seq: 0,
                body: Body::Offer(OfferMeta {
                    name: "notes.txt".into(),
                    size,
                    mime: "text/plain".into(),
                    mtime_ms: 0,
                    purpose: Purpose::Save,
                    batch_id: 1,
                    batch_index: 0,
                    batch_count: 1,
                    drop: None,
                }),
            }))
        };
        let data = |seq: u32, bytes: &[u8]| {
            to_hex(&encode(&Packet {
                transfer_id: 11,
                seq,
                body: Body::Data(bytes),
            }))
        };
        let done = |hash: [u8; 32]| {
            to_hex(&encode(&Packet {
                transfer_id: 11,
                seq: 0,
                body: Body::Done(hash),
            }))
        };
        let sessions = serde_json::json!([
            {"name": "ok", "outcome": "ok",
             "packets": [offer_bytes(5), data(0, b"hel"), data(1, b"lo"), done(sha_hello)]},
            {"name": "data-seq-gap", "outcome": "seq-gap",
             "packets": [offer_bytes(5), data(0, b"hel"), data(2, b"lo")]},
            {"name": "data-past-size", "outcome": "size-mismatch",
             "packets": [offer_bytes(4), data(0, b"hel"), data(1, b"lo")]},
            {"name": "done-before-all-bytes", "outcome": "size-mismatch",
             "packets": [offer_bytes(5), data(0, b"hel"), done(sha_hello)]},
            {"name": "hash-mismatch", "outcome": "hash-mismatch",
             "packets": [offer_bytes(5), data(0, b"hel"), data(1, b"lp"), done(sha_hello)]},
        ]);
        let out = serde_json::json!({
            "packets": packets,
            "rejected": rejected,
            "receiverSessions": sessions,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    }
}
