//! WSS media protocol v4 wire format, control validation and the receive gate.
//!
//! Mirrors `worker/public/media-v4.js` (browser receiver) and
//! `host/src/wss_v4.rs` (host). The shared fixtures in `fixtures/` are the
//! contract; the tests at the bottom replay them against this module.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand_core::{OsRng, RngCore};
use serde_json::{json, Map, Value};

/// The PC the fake relay reports in its `/ws` hello-ok.
pub const FAKE_PC: &str = "77VH-7M56-8GJK-Y12J";

pub const V4_MAGIC: u32 = 0x4250_5634;
pub const V4_VERSION: u8 = 4;
pub const V4_HEADER_BYTES: usize = 28;
pub const V4_MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
pub const V4_MAX_CONTROL_BYTES: usize = 512;
pub const V4_FLAG_KEY: u8 = 0x01;
pub const V4_FLAG_REFINE: u8 = 0x02;
const V4_KNOWN_FLAGS: u8 = V4_FLAG_KEY | V4_FLAG_REFINE;
pub const CODEC_HEVC: u8 = 1;
pub const CODEC_H264: u8 = 2;

/// Header fields of one host -> client binary message (28 bytes, big endian).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHeader {
    pub key: bool,
    pub refine: bool,
    pub codec: u8,
    pub generation: u32,
    pub seq: u32,
    pub capture_ts_us: u64,
    pub width: u16,
    pub height: u16,
}

/// Build one BPV4 packet: 28-byte header followed by one Annex-B access unit.
pub fn encode_frame(header: &FrameHeader, payload: &[u8]) -> Result<Vec<u8>, &'static str> {
    if payload.is_empty() || payload.len() > V4_MAX_PAYLOAD_BYTES {
        return Err("v4 access unit size out of range");
    }
    if header.codec != CODEC_HEVC && header.codec != CODEC_H264 {
        return Err("v4 codec out of range");
    }
    let mut flags = 0u8;
    if header.key {
        flags |= V4_FLAG_KEY;
    }
    if header.refine {
        flags |= V4_FLAG_REFINE;
    }
    let mut packet = Vec::with_capacity(V4_HEADER_BYTES + payload.len());
    packet.extend_from_slice(&V4_MAGIC.to_be_bytes());
    packet.push(V4_VERSION);
    packet.push(flags);
    packet.push(header.codec);
    packet.push(0);
    packet.extend_from_slice(&header.generation.to_be_bytes());
    packet.extend_from_slice(&header.seq.to_be_bytes());
    packet.extend_from_slice(&header.capture_ts_us.to_be_bytes());
    packet.extend_from_slice(&header.width.to_be_bytes());
    packet.extend_from_slice(&header.height.to_be_bytes());
    packet.extend_from_slice(payload);
    Ok(packet)
}

/// Parse one BPV4 packet exactly as the browser's `parseFrameV4` does.
pub fn decode_frame(packet: &[u8]) -> Result<(FrameHeader, &[u8]), &'static str> {
    if packet.len() <= V4_HEADER_BYTES || packet.len() > V4_HEADER_BYTES + V4_MAX_PAYLOAD_BYTES {
        return Err("v4 packet size out of range");
    }
    let u32_at = |at: usize| {
        u32::from_be_bytes([packet[at], packet[at + 1], packet[at + 2], packet[at + 3]])
    };
    let u16_at = |at: usize| u16::from_be_bytes([packet[at], packet[at + 1]]);
    if u32_at(0) != V4_MAGIC || packet[4] != V4_VERSION {
        return Err("v4 magic or version mismatch");
    }
    let flags = packet[5];
    if flags & !V4_KNOWN_FLAGS != 0 {
        return Err("v4 unknown flags");
    }
    if packet[6] != CODEC_HEVC && packet[6] != CODEC_H264 {
        return Err("v4 codec out of range");
    }
    if packet[7] != 0 {
        return Err("v4 reserved byte nonzero");
    }
    let header = FrameHeader {
        key: flags & V4_FLAG_KEY != 0,
        refine: flags & V4_FLAG_REFINE != 0,
        codec: packet[6],
        generation: u32_at(8),
        seq: u32_at(12),
        capture_ts_us: u64::from_be_bytes(packet[16..24].try_into().expect("8 bytes")),
        width: u16_at(24),
        height: u16_at(26),
    };
    Ok((header, &packet[V4_HEADER_BYTES..]))
}

/// Epochs are 32 lowercase hex characters (`/^[a-f0-9]{32}$/`).
pub fn is_valid_epoch(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A fresh epoch. Not cryptographic: it only has to differ from every epoch
/// this process issued, so it mixes std's randomly keyed hasher, the clock,
/// and a counter.
pub fn new_epoch(counter: u64) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut out = String::with_capacity(32);
    for lane in 0..2u64 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(counter);
        hasher.write_u64(lane);
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        let value = hasher.finish();
        out.push_str(&format!("{value:016x}"));
    }
    out
}

/// A browser -> host control message after the same checks as
/// `validateClientMessageV4` and `parse_client_message`.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    Ack {
        generation: u32,
        seq: u32,
        recv_ts_ms: f64,
        decoded_seq: Option<u32>,
    },
    Keyframe {
        reason: String,
    },
    Ping,
}

pub fn parse_client_message(text: &str) -> Result<ClientMessage, &'static str> {
    if text.len() > V4_MAX_CONTROL_BYTES {
        return Err("control message too large");
    }
    let value: Value = serde_json::from_str(text).map_err(|_| "control message is not JSON")?;
    let object = value
        .as_object()
        .ok_or("control message is not an object")?;
    match object.get("type").and_then(Value::as_str) {
        Some("ack") => {
            if !only_keys(
                object,
                &["type", "generation", "seq", "recvTsMs", "decodedSeq"],
            ) {
                return Err("ack has unknown keys");
            }
            let generation = uint32_field(object.get("generation")).ok_or("ack generation")?;
            let seq = uint32_field(object.get("seq")).ok_or("ack seq")?;
            let recv_ts_ms = object
                .get("recvTsMs")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or("ack recvTsMs")?;
            let decoded_seq = match object.get("decodedSeq") {
                None => None,
                Some(v) => Some(uint32_field(Some(v)).ok_or("ack decodedSeq")?),
            };
            Ok(ClientMessage::Ack {
                generation,
                seq,
                recv_ts_ms,
                decoded_seq,
            })
        }
        Some("keyframe") => {
            if !only_keys(object, &["type", "reason"]) {
                return Err("keyframe has unknown keys");
            }
            let reason = object
                .get("reason")
                .and_then(Value::as_str)
                .ok_or("keyframe reason")?;
            if !valid_reason(reason) {
                return Err("keyframe reason invalid");
            }
            Ok(ClientMessage::Keyframe {
                reason: reason.to_owned(),
            })
        }
        Some("ping") => {
            if !only_keys(object, &["type"]) {
                return Err("ping has unknown keys");
            }
            Ok(ClientMessage::Ping)
        }
        _ => Err("unknown control type"),
    }
}

fn uint32_field(value: Option<&Value>) -> Option<u32> {
    value
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
}

fn only_keys(object: &Map<String, Value>, allowed: &[&str]) -> bool {
    object.keys().all(|key| allowed.contains(&key.as_str()))
}

fn valid_reason(reason: &str) -> bool {
    (1..=64).contains(&reason.len())
        && reason
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// The `/ws` hello-ok sent after a valid `register-proof`: the media epoch, the
/// PC and the media ticket the viewer presents on `/media-ws`.
pub fn hello_ok_signal(media_epoch: &str, ticket: &str) -> String {
    json!({
        "type": "hello-ok",
        "pc": FAKE_PC,
        "mediaEpoch": media_epoch,
        "iceServers": [],
        "ticket": ticket,
    })
    .to_string()
}

/// `{"type":"register-challenge","nonce"}` with the nonce as unpadded base64url.
pub fn register_challenge(nonce: &[u8; 32]) -> String {
    json!({"type": "register-challenge", "nonce": URL_SAFE_NO_PAD.encode(nonce)}).to_string()
}

/// The signature of a `register-proof`, or `None` for any other message or a
/// signature that is not 64 bytes of unpadded base64url.
pub fn register_proof_signature(text: &str) -> Option<[u8; 64]> {
    let value: Value = serde_json::from_str(text).ok()?;
    if value.get("type")?.as_str()? != "register-proof" {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(value.get("signature")?.as_str()?).ok()?;
    bytes.try_into().ok()
}

/// Unpadded base64url decoding, `None` when the text is not valid.
pub fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

/// Unpadded base64url encoding, the form the relay uses for nonces and tickets.
pub fn b64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 32 bytes from the operating system's RNG. Panics if it is unavailable: a
/// test relay that cannot get entropy should stop, not issue weak nonces.
pub fn random32() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

pub fn media_hello_ok(registered_at_ms: u64) -> String {
    json!({"type": "hello-ok", "registeredAtMs": registered_at_ms}).to_string()
}

pub fn media_peer(present: bool) -> String {
    json!({"type": "media-peer", "present": present}).to_string()
}

pub fn pong() -> String {
    json!({"type": "pong"}).to_string()
}

/// `host-capabilities` advertising optional `audio:1`. Sent after `hello-ok`
/// only when the fake runs with `--audio`.
pub fn host_capabilities_audio() -> String {
    json!({
        "type": "host-capabilities",
        "serviceMode": false,
        "coldBoot": false,
        "secureDesktop": false,
        "secureText": false,
        "audio": 1
    })
    .to_string()
}

/// First-frame handshake notices from the host (`admitted`, `ready`, `failed`).
/// The WebRTC admission, sent once the keyframe's first RTP packet has left.
/// `rtpTimestamp` is that packet's header timestamp, which the browser checks
/// presented frames against.
pub fn first_frame_admitted_rtc(
    request_id: &str,
    generation: u32,
    rtp_timestamp: u32,
    codec: &str,
    epoch: Option<&str>,
) -> String {
    let mut message = json!({
        "type": "first-frame",
        "protocol": 1,
        "phase": "admitted",
        "requestId": request_id,
        "transport": "webrtc",
        "generation": generation,
        "rtpTimestamp": rtp_timestamp,
        "codec": codec,
    });
    if let Some(epoch) = epoch {
        message["epoch"] = json!(epoch);
    }
    message.to_string()
}

pub fn first_frame(
    phase: &str,
    request_id: &str,
    generation: Option<u32>,
    reason: Option<&str>,
) -> String {
    let mut message =
        json!({"type": "first-frame", "protocol": 1, "phase": phase, "requestId": request_id});
    if let Some(generation) = generation {
        message["generation"] = json!(generation);
    }
    if let Some(reason) = reason {
        message["reason"] = json!(reason);
    }
    message.to_string()
}

/// Receive-side ordering gate, mirroring `V4ReceiveGate` in media-v4.js. The
/// fake host uses it in tests to play the browser's side of the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiveGate {
    pub generation: Option<u32>,
    pub expected_seq: Option<u32>,
    pub waiting_for_key: bool,
    pub pending_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateVerdict {
    pub decode: bool,
    pub request_keyframe: Option<String>,
}

impl ReceiveGate {
    pub fn new() -> Self {
        Self {
            generation: None,
            expected_seq: None,
            waiting_for_key: true,
            pending_reason: "connect".into(),
        }
    }

    pub fn accept(&mut self, header: &FrameHeader) -> GateVerdict {
        if header.key {
            self.generation = Some(header.generation);
            self.expected_seq = Some(header.seq.wrapping_add(1));
            self.waiting_for_key = false;
            self.pending_reason.clear();
            return GateVerdict {
                decode: true,
                request_keyframe: None,
            };
        }
        let reason = if self.waiting_for_key {
            Some(if self.pending_reason.is_empty() {
                "awaiting-keyframe".to_owned()
            } else {
                self.pending_reason.clone()
            })
        } else if self.generation != Some(header.generation) {
            Some("generation-change".to_owned())
        } else if self.expected_seq != Some(header.seq) {
            Some("sequence-gap".to_owned())
        } else {
            None
        };
        if let Some(reason) = reason {
            self.waiting_for_key = true;
            self.pending_reason = reason.clone();
            return GateVerdict {
                decode: false,
                request_keyframe: Some(reason),
            };
        }
        self.expected_seq = Some(header.seq.wrapping_add(1));
        GateVerdict {
            decode: true,
            request_keyframe: None,
        }
    }
}

impl Default for ReceiveGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTRACT: &str = include_str!("../../../../fixtures/wss-v4-contract.json");
    const EPOCHS: &str = include_str!("../../../../fixtures/wss-contract.json");

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn encode_then_decode_round_trips_every_field() {
        let header = FrameHeader {
            key: true,
            refine: false,
            codec: CODEC_H264,
            generation: 7,
            seq: 0xffff_fffe,
            capture_ts_us: 16_666_666,
            width: 1920,
            height: 1080,
        };
        let packet = encode_frame(&header, &[0, 0, 0, 1, 0x67]).unwrap();
        assert_eq!(packet.len(), V4_HEADER_BYTES + 5);
        let (decoded, payload) = decode_frame(&packet).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(payload, &[0, 0, 0, 1, 0x67]);
    }

    #[test]
    fn encode_refuses_empty_oversized_and_unknown_codec() {
        let header = FrameHeader {
            key: false,
            refine: false,
            codec: CODEC_HEVC,
            generation: 1,
            seq: 1,
            capture_ts_us: 0,
            width: 1,
            height: 1,
        };
        assert!(encode_frame(&header, &[]).is_err());
        assert!(encode_frame(&header, &vec![0u8; V4_MAX_PAYLOAD_BYTES + 1]).is_err());
        let bad = FrameHeader { codec: 3, ..header };
        assert!(encode_frame(&bad, &[1]).is_err());
    }

    #[test]
    fn fixture_packets_match_the_contract() {
        let fixture: Value = serde_json::from_str(CONTRACT).unwrap();
        let mut checked = 0;
        for case in fixture["packets"].as_array().unwrap() {
            let hex = case["hex"].as_str().unwrap();
            let bytes = unhex(hex);
            let accepted = case["accepted"].as_bool().unwrap();
            match decode_frame(&bytes) {
                Ok((header, payload)) => {
                    assert!(accepted, "{} should be refused", case["name"]);
                    assert_eq!(
                        header.key,
                        case["key"].as_bool().unwrap(),
                        "{}",
                        case["name"]
                    );
                    assert_eq!(
                        header.refine,
                        case["refine"].as_bool().unwrap(),
                        "{}",
                        case["name"]
                    );
                    assert_eq!(
                        u64::from(header.codec),
                        case["codec"].as_u64().unwrap(),
                        "{}",
                        case["name"]
                    );
                    assert_eq!(
                        u64::from(header.generation),
                        case["generation"].as_u64().unwrap(),
                        "{}",
                        case["name"]
                    );
                    assert_eq!(
                        u64::from(header.seq),
                        case["seq"].as_u64().unwrap(),
                        "{}",
                        case["name"]
                    );
                    assert_eq!(
                        header.capture_ts_us.to_string(),
                        case["captureTsUs"].as_str().unwrap(),
                        "{}",
                        case["name"]
                    );
                    assert_eq!(
                        u64::from(header.width),
                        case["width"].as_u64().unwrap(),
                        "{}",
                        case["name"]
                    );
                    assert_eq!(
                        u64::from(header.height),
                        case["height"].as_u64().unwrap(),
                        "{}",
                        case["name"]
                    );
                    assert_eq!(
                        payload,
                        unhex(case["payloadHex"].as_str().unwrap()).as_slice()
                    );
                }
                Err(reason) => {
                    assert!(!accepted, "{} should be accepted: {reason}", case["name"]);
                }
            }
            checked += 1;
        }
        assert_eq!(checked, 9);
    }

    #[test]
    fn fixture_client_messages_match_the_contract() {
        let fixture: Value = serde_json::from_str(CONTRACT).unwrap();
        let mut checked = 0;
        for case in fixture["clientMessages"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            let accepted = case["accepted"].as_bool().unwrap();
            let parsed = parse_client_message(text);
            assert_eq!(parsed.is_ok(), accepted, "{text}: {parsed:?}");
            if let Ok(message) = parsed {
                let kind = match message {
                    ClientMessage::Ack { .. } => "ack",
                    ClientMessage::Keyframe { .. } => "keyframe",
                    ClientMessage::Ping => "ping",
                };
                assert_eq!(kind, case["kind"].as_str().unwrap(), "{text}");
            }
            checked += 1;
        }
        assert_eq!(checked, 12);
    }

    #[test]
    fn fixture_host_messages_match_the_contract() {
        // The fake host only ever sends hello-ok, media-peer and pong; the
        // fixture's host list is the relay-side ping/hello check.
        let fixture: Value = serde_json::from_str(CONTRACT).unwrap();
        for case in fixture["hostMessages"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            let value: Value = serde_json::from_str(text).unwrap();
            let kind = value["type"].as_str().unwrap();
            let accepted = case["accepted"].as_bool().unwrap();
            let relay_accepts_from_host = matches!(
                (kind, value.get("role").and_then(Value::as_str)),
                ("ping", None) | ("hello", Some("media-host"))
            );
            assert_eq!(relay_accepts_from_host, accepted, "{text}");
        }
    }

    #[test]
    fn host_capabilities_audio_matches_the_audio_fixture() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../../fixtures/audio-contract.json")).unwrap();
        let text = fixture["signaling"]["hostCapabilities"]["text"]
            .as_str()
            .unwrap();
        let expected: Value = serde_json::from_str(text).unwrap();
        let sent: Value = serde_json::from_str(&host_capabilities_audio()).unwrap();
        assert_eq!(sent, expected);
    }

    #[test]
    fn epoch_contract_matches_the_validator() {
        let fixture: Value = serde_json::from_str(EPOCHS).unwrap();
        for case in fixture["epochs"].as_array().unwrap() {
            let value = case["value"].as_str().unwrap();
            assert_eq!(
                is_valid_epoch(value),
                case["accepted"].as_bool().unwrap(),
                "{value:?}"
            );
        }
    }

    #[test]
    fn fresh_epochs_are_lowerhex_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for counter in 0..2000 {
            let epoch = new_epoch(counter);
            assert!(is_valid_epoch(&epoch), "{epoch}");
            assert!(seen.insert(epoch));
        }
    }

    #[test]
    fn keyframe_and_ack_messages_reject_bad_shapes() {
        assert!(parse_client_message(
            r#"{"type":"ack","generation":7,"seq":1,"recvTsMs":1,"x":1}"#
        )
        .is_err());
        assert!(parse_client_message(r#"{"type":"ack","generation":7,"seq":1}"#).is_err());
        assert!(parse_client_message(
            r#"{"type":"ack","generation":4294967295,"seq":0,"recvTsMs":0}"#
        )
        .is_ok());
        assert!(parse_client_message(r#"{"type":"keyframe","reason":"sequence-gap"}"#).is_ok());
        assert!(parse_client_message(r#"{"type":"keyframe","reason":""}"#).is_err());
        assert!(parse_client_message(&format!(
            "{{\"type\":\"ping\",\"pad\":\"{}\"}}",
            "x".repeat(600)
        ))
        .is_err());
    }

    #[test]
    fn receive_gate_requests_a_keyframe_on_gap_and_generation_change() {
        let mut gate = ReceiveGate::new();
        let header = |key, generation, seq| FrameHeader {
            key,
            refine: false,
            codec: CODEC_H264,
            generation,
            seq,
            capture_ts_us: 0,
            width: 1920,
            height: 1080,
        };
        assert!(gate.accept(&header(true, 3, 10)).decode);
        assert!(gate.accept(&header(false, 3, 11)).decode);
        let gap = gate.accept(&header(false, 3, 13));
        assert!(!gap.decode);
        assert_eq!(gap.request_keyframe.as_deref(), Some("sequence-gap"));
        // Deltas stay dropped until the next keyframe, which resets the chain.
        assert!(!gate.accept(&header(false, 3, 14)).decode);
        assert!(gate.accept(&header(true, 3, 15)).decode);
        assert!(gate.accept(&header(false, 3, 16)).decode);
        let generation = gate.accept(&header(false, 4, 17));
        assert_eq!(
            generation.request_keyframe.as_deref(),
            Some("generation-change")
        );
    }
}
