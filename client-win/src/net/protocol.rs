//! WSS v4 wire contract: BPV4 access units and the JSON control messages on
//! `/ws` and `/media-ws`. Mirrors `worker/public/media-v4.js`,
//! `host/src/wss_v4.rs` and `relay/protocol.go`. The shared fixtures pin
//! the behaviour in the tests below.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Map, Value};

use super::config::Codec;

/// `"BPV4"` big-endian.
pub const V4_MAGIC: u32 = 0x4250_5634;
pub const V4_VERSION: u8 = 4;
pub const V4_HEADER_BYTES: usize = 28;
pub const V4_MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
pub const V4_MAX_CONTROL_BYTES: usize = 512;
pub const FLAG_KEY: u8 = 0x01;
pub const FLAG_REFINE: u8 = 0x02;
const KNOWN_FLAGS: u8 = FLAG_KEY | FLAG_REFINE;
pub const MAX_REASON_CHARS: usize = 64;

/// Browser values for `media-control start`, copied from `receiver.js`.
pub const START_QUALITY: &str = "high";
pub const START_STREAM_MODE: &str = "work";
pub const START_REASON: &str = "client-start";
/// Reason for the one media-control keyframe sent on a first-frame timeout.
pub const FIRST_FRAME_TIMEOUT_REASON: &str = "first-frame-timeout";

/// The decoded 28-byte BPV4 header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub key: bool,
    pub refine: bool,
    pub codec: Codec,
    pub generation: u32,
    pub seq: u32,
    pub capture_ts_us: u64,
    pub width: u16,
    pub height: u16,
}

/// True when a binary message starts with the BPA1 magic.
pub use crate::audio::is_audio_packet;

/// Parse one BPV4 access-unit message. The error names the refused rule.
pub fn parse_frame(packet: &[u8]) -> Result<(FrameHeader, &[u8]), &'static str> {
    if packet.len() <= V4_HEADER_BYTES {
        return Err("truncated-or-empty");
    }
    if packet.len() > V4_HEADER_BYTES + V4_MAX_PAYLOAD_BYTES {
        return Err("oversized");
    }
    if be_u32(&packet[0..4]) != V4_MAGIC {
        return Err("magic");
    }
    if packet[4] != V4_VERSION {
        return Err("version");
    }
    let flags = packet[5];
    if flags & !KNOWN_FLAGS != 0 {
        return Err("unknown-flags");
    }
    let codec = Codec::from_packet_code(packet[6]).ok_or("codec")?;
    if packet[7] != 0 {
        return Err("reserved");
    }
    let header = FrameHeader {
        key: flags & FLAG_KEY != 0,
        refine: flags & FLAG_REFINE != 0,
        codec,
        generation: be_u32(&packet[8..12]),
        seq: be_u32(&packet[12..16]),
        capture_ts_us: be_u64(&packet[16..24]),
        width: be_u16(&packet[24..26]),
        height: be_u16(&packet[26..28]),
    };
    Ok((header, &packet[V4_HEADER_BYTES..]))
}

fn be_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn be_u64(bytes: &[u8]) -> u64 {
    let mut array = [0u8; 8];
    array.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(array)
}

/// A validated client control message (what the relay and host accept from the client).
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub enum ClientControl {
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

/// Validate a client control message exactly as `validateClientMessageV4` and the relay do.
#[cfg(test)]
pub fn validate_client_control(text: &str) -> Option<ClientControl> {
    if text.len() > V4_MAX_CONTROL_BYTES {
        return None;
    }
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    match object.get("type")?.as_str()? {
        "ack" => {
            if !only_keys(
                object,
                &["type", "generation", "seq", "recvTsMs", "decodedSeq"],
            ) {
                return None;
            }
            let recv_ts_ms = object.get("recvTsMs")?.as_f64()?;
            if !recv_ts_ms.is_finite() || recv_ts_ms < 0.0 {
                return None;
            }
            let decoded_seq = match object.get("decodedSeq") {
                None => None,
                Some(value) => Some(uint32_value(value)?),
            };
            Some(ClientControl::Ack {
                generation: uint32_value(object.get("generation")?)?,
                seq: uint32_value(object.get("seq")?)?,
                recv_ts_ms,
                decoded_seq,
            })
        }
        "keyframe" => {
            if !only_keys(object, &["type", "reason"]) {
                return None;
            }
            let reason = object.get("reason")?.as_str()?;
            valid_reason(reason).then(|| ClientControl::Keyframe {
                reason: reason.to_owned(),
            })
        }
        "ping" => only_keys(object, &["type"]).then_some(ClientControl::Ping),
        _ => None,
    }
}

/// Host text the relay forwards onto the media socket. Only heartbeat-like messages pass.
pub fn validate_host_control(text: &str) -> bool {
    if text.len() > V4_MAX_CONTROL_BYTES {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("ping") => true,
        Some("hello") => object.get("role").and_then(Value::as_str) == Some("media-host"),
        _ => false,
    }
}

/// A relay-originated control message the client acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayControl {
    /// Handshake reply. `/ws` carries `mediaEpoch`; `/media-ws` does not.
    HelloOk {
        media_epoch: Option<String>,
    },
    /// Whether the host is on the other side of the media room.
    MediaPeer {
        present: bool,
    },
    Pong,
}

pub fn parse_relay_control(text: &str) -> Option<RelayControl> {
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    match object.get("type")?.as_str()? {
        "hello-ok" => Some(RelayControl::HelloOk {
            media_epoch: object
                .get("mediaEpoch")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        "media-peer" => Some(RelayControl::MediaPeer {
            present: object.get("present")?.as_bool()?,
        }),
        "pong" => Some(RelayControl::Pong),
        _ => None,
    }
}

/// `^[a-f0-9]{32}$`: the wire epoch from `hello-ok`.
pub fn valid_epoch(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// `^[A-Za-z0-9_-]{8,64}$`: a first-frame request id.
#[cfg(test)]
pub fn valid_request_id(value: &str) -> bool {
    (8..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// `^[A-Za-z0-9_.:-]{1,64}$`: a keyframe reason.
#[cfg(test)]
pub fn valid_reason(reason: &str) -> bool {
    (1..=MAX_REASON_CHARS).contains(&reason.len()) && reason.bytes().all(reason_byte)
}

fn reason_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-')
}

/// Replace unsafe characters with `-`, cap at 64, and fall back to `receiver`.
pub fn sanitize_reason(reason: &str) -> String {
    let safe: String = reason
        .chars()
        .map(|c| {
            if c.is_ascii() && reason_byte(c as u8) {
                c
            } else {
                '-'
            }
        })
        .take(MAX_REASON_CHARS)
        .collect();
    if safe.is_empty() {
        "receiver".to_owned()
    } else {
        safe
    }
}

/// A fresh first-frame request id: 16 base64url characters from a per-process random hasher.
/// It only has to be unique within a session, so it need not be cryptographic.
pub fn new_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let state = RandomState::new();
    let mut bytes = [0u8; 12];
    for (index, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = state.build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        hasher.write_usize(index);
        let value = hasher.finish().to_le_bytes();
        chunk.copy_from_slice(&value[..chunk.len()]);
    }
    base64url_no_pad(&bytes)
}

fn base64url_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let value = (b0 << 16) | (b1 << 8) | b2;
        for index in 0..=chunk.len() {
            let shift = 18 - 6 * index as u32;
            out.push(ALPHABET[((value >> shift) & 63) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
fn only_keys(object: &Map<String, Value>, keys: &[&str]) -> bool {
    object.keys().all(|key| keys.contains(&key.as_str()))
}

#[cfg(test)]
fn uint32_value(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|value| u32::try_from(value).ok())
}

// ---- Outbound messages (byte-for-byte shape of receiver.js) -------------------

pub fn hello_message(role: &str) -> String {
    json!({ "type": "hello", "role": role }).to_string()
}

pub fn start_message(epoch: &str, codec: Codec) -> String {
    json!({
        "type": "media-control",
        "action": "start",
        "protocol": V4_VERSION,
        "epoch": epoch,
        "codec": codec.wire_name(),
        "streamMode": START_STREAM_MODE,
        "reason": START_REASON,
        "quality": START_QUALITY,
    })
    .to_string()
}

pub fn first_frame_request(request_id: &str, epoch: &str) -> String {
    json!({
        "type": "first-frame",
        "protocol": 1,
        "phase": "request",
        "requestId": request_id,
        "transport": "wss",
        "epoch": epoch,
    })
    .to_string()
}

pub fn first_frame_presented(request_id: &str, generation: u32) -> String {
    json!({
        "type": "first-frame",
        "protocol": 1,
        "phase": "presented",
        "requestId": request_id,
        "generation": generation,
    })
    .to_string()
}

pub fn first_frame_keyframe(epoch: &str, codec: Codec) -> String {
    json!({
        "type": "media-control",
        "action": "keyframe",
        "protocol": V4_VERSION,
        "epoch": epoch,
        "reason": FIRST_FRAME_TIMEOUT_REASON,
        "codec": codec.wire_name(),
    })
    .to_string()
}

/// ACK for a received frame. `recv_ts_ms` is rounded to 0.1 ms like the browser.
pub fn ack_message(generation: u32, seq: u32, recv_ts_ms: f64, decoded_seq: Option<u32>) -> String {
    let recv = if recv_ts_ms.is_finite() {
        (recv_ts_ms.max(0.0) * 10.0).round() / 10.0
    } else {
        0.0
    };
    let mut message = Map::new();
    message.insert("type".into(), json!("ack"));
    message.insert("generation".into(), json!(generation));
    message.insert("seq".into(), json!(seq));
    message.insert("recvTsMs".into(), json!(recv));
    if let Some(decoded) = decoded_seq {
        message.insert("decodedSeq".into(), json!(decoded));
    }
    Value::Object(message).to_string()
}

pub fn keyframe_message(reason: &str) -> String {
    json!({ "type": "keyframe", "reason": sanitize_reason(reason) }).to_string()
}

pub fn ping_message() -> String {
    json!({ "type": "ping" }).to_string()
}

// ---- Fixture-shaped helpers for tests -------------------------------------------

#[cfg(test)]
pub(crate) fn encode_frame(header: &FrameHeader, payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(V4_HEADER_BYTES + payload.len());
    packet.extend_from_slice(&V4_MAGIC.to_be_bytes());
    packet.push(V4_VERSION);
    let mut flags = 0;
    if header.key {
        flags |= FLAG_KEY;
    }
    if header.refine {
        flags |= FLAG_REFINE;
    }
    packet.push(flags);
    packet.push(header.codec.packet_code());
    packet.push(0);
    packet.extend_from_slice(&header.generation.to_be_bytes());
    packet.extend_from_slice(&header.seq.to_be_bytes());
    packet.extend_from_slice(&header.capture_ts_us.to_be_bytes());
    packet.extend_from_slice(&header.width.to_be_bytes());
    packet.extend_from_slice(&header.height.to_be_bytes());
    packet.extend_from_slice(payload);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    const V4_FIXTURE: &str = include_str!("../../../fixtures/wss-v4-contract.json");
    const EPOCH_FIXTURE: &str = include_str!("../../../fixtures/wss-contract.json");

    fn hex(text: &str) -> Vec<u8> {
        assert!(text.len().is_multiple_of(2), "odd hex length");
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("valid hex"))
            .collect()
    }

    fn fixture() -> Value {
        serde_json::from_str(V4_FIXTURE).expect("fixture parses")
    }

    #[test]
    fn every_packet_fixture_is_accepted_or_refused_as_recorded() {
        let packets = fixture()["packets"].as_array().unwrap().clone();
        assert!(packets.len() >= 9);
        for entry in packets {
            let name = entry["name"].as_str().unwrap();
            let bytes = hex(entry["hex"].as_str().unwrap());
            let parsed = parse_frame(&bytes);
            let accepted = entry["accepted"].as_bool().unwrap();
            assert_eq!(parsed.is_ok(), accepted, "{name}: {parsed:?}");
            if let Ok((header, payload)) = parsed {
                assert_eq!(header.key, entry["key"].as_bool().unwrap(), "{name} key");
                assert_eq!(
                    header.refine,
                    entry["refine"].as_bool().unwrap(),
                    "{name} refine"
                );
                assert_eq!(
                    header.codec.packet_code(),
                    entry["codec"].as_u64().unwrap() as u8,
                    "{name} codec"
                );
                assert_eq!(
                    u64::from(header.generation),
                    entry["generation"].as_u64().unwrap(),
                    "{name} generation"
                );
                assert_eq!(
                    u64::from(header.seq),
                    entry["seq"].as_u64().unwrap(),
                    "{name} seq"
                );
                assert_eq!(
                    header.capture_ts_us.to_string(),
                    entry["captureTsUs"].as_str().unwrap(),
                    "{name} ts"
                );
                assert_eq!(
                    u64::from(header.width),
                    entry["width"].as_u64().unwrap(),
                    "{name} width"
                );
                assert_eq!(
                    u64::from(header.height),
                    entry["height"].as_u64().unwrap(),
                    "{name} height"
                );
                assert_eq!(
                    payload,
                    hex(entry["payloadHex"].as_str().unwrap()).as_slice(),
                    "{name} payload"
                );
            }
        }
    }

    #[test]
    fn every_client_message_fixture_is_accepted_or_refused_as_recorded() {
        let messages = fixture()["clientMessages"].as_array().unwrap().clone();
        assert_eq!(messages.len(), 12);
        for entry in messages {
            let text = entry["text"].as_str().unwrap();
            let accepted = entry["accepted"].as_bool().unwrap();
            let parsed = validate_client_control(text);
            assert_eq!(
                parsed.is_some(),
                accepted,
                "client message {text}: {parsed:?}"
            );
            if let (Some(kind), Some(message)) = (entry["kind"].as_str(), parsed) {
                let actual = match message {
                    ClientControl::Ack { .. } => "ack",
                    ClientControl::Keyframe { .. } => "keyframe",
                    ClientControl::Ping => "ping",
                };
                assert_eq!(actual, kind, "client message {text}");
            }
        }
    }

    #[test]
    fn fixture_ack_fields_are_read_exactly() {
        let with_decoded = validate_client_control(
            r#"{"type":"ack","generation":7,"seq":42,"recvTsMs":1234.5,"decodedSeq":41}"#,
        );
        assert_eq!(
            with_decoded,
            Some(ClientControl::Ack {
                generation: 7,
                seq: 42,
                recv_ts_ms: 1234.5,
                decoded_seq: Some(41)
            })
        );
        let without =
            validate_client_control(r#"{"type":"ack","generation":7,"seq":42,"recvTsMs":1234.5}"#);
        assert_eq!(
            without,
            Some(ClientControl::Ack {
                generation: 7,
                seq: 42,
                recv_ts_ms: 1234.5,
                decoded_seq: None
            })
        );
    }

    #[test]
    fn host_message_fixtures_match_the_host_side_validator() {
        let messages = fixture()["hostMessages"].as_array().unwrap().clone();
        assert_eq!(messages.len(), 3);
        for entry in messages {
            let text = entry["text"].as_str().unwrap();
            let accepted = entry["accepted"].as_bool().unwrap();
            assert_eq!(validate_host_control(text), accepted, "host message {text}");
        }
    }

    #[test]
    fn epoch_fixtures_are_accepted_or_refused_as_recorded() {
        let epochs = serde_json::from_str::<Value>(EPOCH_FIXTURE).unwrap()["epochs"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(epochs.len(), 5);
        for entry in epochs {
            let value = entry["value"].as_str().unwrap();
            assert_eq!(
                valid_epoch(value),
                entry["accepted"].as_bool().unwrap(),
                "epoch {value:?}"
            );
        }
    }

    #[test]
    fn header_constants_match_the_fixture() {
        let contract = fixture();
        assert_eq!(
            contract["headerBytes"].as_u64().unwrap() as usize,
            V4_HEADER_BYTES
        );
        assert_eq!(
            contract["maxPayloadBytes"].as_u64().unwrap() as usize,
            V4_MAX_PAYLOAD_BYTES
        );
        assert_eq!(V4_MAGIC.to_be_bytes(), *b"BPV4");
        assert!(is_audio_packet(b"BPA1"));
    }

    fn sample_header(key: bool) -> FrameHeader {
        FrameHeader {
            key,
            refine: false,
            codec: Codec::H264,
            generation: 3,
            seq: 100,
            capture_ts_us: 5,
            width: 1280,
            height: 720,
        }
    }

    #[test]
    fn encoded_frame_round_trips_through_the_parser() {
        let header = sample_header(true);
        let packet = encode_frame(&header, &[0xAB, 0xCD]);
        let (parsed, payload) = parse_frame(&packet).unwrap();
        assert_eq!(parsed, header);
        assert_eq!(payload, &[0xAB, 0xCD]);
    }

    #[test]
    fn exactly_header_length_is_refused_and_one_payload_byte_is_accepted() {
        let packet = encode_frame(&sample_header(false), &[]);
        assert_eq!(packet.len(), V4_HEADER_BYTES);
        assert_eq!(parse_frame(&packet), Err("truncated-or-empty"));
        let packet = encode_frame(&sample_header(false), &[1]);
        assert!(parse_frame(&packet).is_ok());
    }

    #[test]
    fn payload_at_the_limit_is_accepted_and_one_more_byte_is_refused() {
        let at_limit = encode_frame(&sample_header(false), &vec![0u8; V4_MAX_PAYLOAD_BYTES]);
        assert!(parse_frame(&at_limit).is_ok());
        let over = encode_frame(&sample_header(false), &vec![0u8; V4_MAX_PAYLOAD_BYTES + 1]);
        assert_eq!(parse_frame(&over), Err("oversized"));
    }

    #[test]
    fn each_header_rule_is_checked_independently() {
        let good = encode_frame(&sample_header(true), &[9]);
        let mutate = |index: usize, value: u8| {
            let mut packet = good.clone();
            packet[index] = value;
            parse_frame(&packet).map(|_| ())
        };
        assert_eq!(mutate(0, b'X'), Err("magic"));
        assert_eq!(mutate(4, 3), Err("version"));
        assert_eq!(mutate(4, 5), Err("version"));
        assert_eq!(mutate(5, 0x04), Err("unknown-flags"));
        assert_eq!(mutate(5, 0x80), Err("unknown-flags"));
        assert_eq!(mutate(6, 0), Err("codec"));
        assert_eq!(mutate(6, 3), Err("codec"));
        assert_eq!(mutate(7, 1), Err("reserved"));
        assert!(
            mutate(5, 0x03).is_ok(),
            "key and refine together are known flags"
        );
    }

    #[test]
    fn codec_one_is_hevc_and_two_is_h264_for_the_payload_header() {
        let mut header = sample_header(true);
        header.codec = Codec::Hevc;
        let (parsed, _) = parse_frame(&encode_frame(&header, &[1])).unwrap();
        assert_eq!(parsed.codec, Codec::Hevc);
    }

    #[test]
    fn bpa1_packets_are_recognised_and_short_input_is_not_audio() {
        assert!(is_audio_packet(b"BPA1\x00\x00"));
        assert!(!is_audio_packet(b"BPA"));
        assert!(!is_audio_packet(b"BPV4xxxx"));
        assert!(!is_audio_packet(&[]));
    }

    #[test]
    fn control_messages_refuse_unknown_keys_bad_types_and_oversize() {
        let oversized = format!(
            r#"{{"type":"ping","pad":"{}"}}"#,
            "x".repeat(V4_MAX_CONTROL_BYTES)
        );
        for text in [
            oversized.as_str(),
            r#"{"type":"ping","extra":1}"#,
            r#"{"type":"keyframe","reason":"a","extra":1}"#,
            r#"{"type":"keyframe"}"#,
            r#"{"type":"keyframe","reason":5}"#,
            r#"{"type":"ack","generation":7,"seq":1,"recvTsMs":null}"#,
            r#"{"type":"ack","generation":7,"seq":1,"recvTsMs":-1}"#,
            r#"{"type":"ack","generation":7,"seq":1,"recvTsMs":1,"decodedSeq":null}"#,
            r#"{"type":"ack","generation":7,"seq":1,"recvTsMs":1,"decodedSeq":-2}"#,
            r#"{"type":"ack","generation":7,"seq":1}"#,
            r#"{"type":"ack","generation":"7","seq":1,"recvTsMs":1}"#,
            r#"{"type":"ack","generation":7,"seq":1,"recvTsMs":1,"lanes":[1]}"#,
            r#"[]"#,
            r#"null"#,
            r#"42"#,
            r#"{"noType":true}"#,
            r#"{"type":7}"#,
            r#"{"type":"hello","role":"client"}"#,
            "",
        ] {
            assert!(
                validate_client_control(text).is_none(),
                "must refuse {text:?}"
            );
        }
    }

    #[test]
    fn host_heartbeats_are_limited_to_ping_and_media_host_hello() {
        assert!(validate_host_control(r#"{"type":"ping"}"#));
        assert!(validate_host_control(
            r#"{"type":"hello","role":"media-host"}"#
        ));
        for text in [
            r#"{"type":"hello","role":"client"}"#,
            r#"{"type":"hello"}"#,
            r#"{"type":"pong"}"#,
            r#"{"type":"ack","generation":1,"seq":1,"recvTsMs":1}"#,
            "not json",
        ] {
            assert!(!validate_host_control(text), "must refuse {text}");
        }
    }

    #[test]
    fn relay_controls_parse_hello_ok_media_peer_and_pong_only() {
        assert_eq!(
            parse_relay_control(
                r#"{"type":"hello-ok","mediaEpoch":"0123456789abcdef0123456789abcdef"}"#
            ),
            Some(RelayControl::HelloOk {
                media_epoch: Some("0123456789abcdef0123456789abcdef".into())
            })
        );
        assert_eq!(
            parse_relay_control(r#"{"type":"hello-ok","registeredAtMs":5}"#),
            Some(RelayControl::HelloOk { media_epoch: None })
        );
        assert_eq!(
            parse_relay_control(r#"{"type":"media-peer","present":true}"#),
            Some(RelayControl::MediaPeer { present: true })
        );
        assert_eq!(
            parse_relay_control(r#"{"type":"media-peer","present":"yes"}"#),
            None
        );
        assert_eq!(parse_relay_control(r#"{"type":"media-peer"}"#), None);
        assert_eq!(
            parse_relay_control(r#"{"type":"pong"}"#),
            Some(RelayControl::Pong)
        );
        assert_eq!(
            parse_relay_control(r#"{"type":"ack","generation":1,"seq":1,"recvTsMs":1}"#),
            None
        );
        assert_eq!(parse_relay_control("not json"), None);
    }

    #[test]
    fn request_ids_and_reasons_follow_the_relay_regexes() {
        assert!(valid_request_id("abcdefgh"));
        assert!(valid_request_id(&"a".repeat(64)));
        assert!(!valid_request_id("short"));
        assert!(!valid_request_id(&"a".repeat(65)));
        assert!(!valid_request_id("has space!"));
        assert!(valid_reason("sequence-gap"));
        assert!(valid_reason("a.b:c_d-e"));
        assert!(!valid_reason(""));
        assert!(!valid_reason(&"x".repeat(65)));
        assert!(!valid_reason("bad reason!"));
    }

    #[test]
    fn sanitize_reason_maps_unsafe_characters_and_caps_length() {
        assert_eq!(sanitize_reason("bad reason!"), "bad-reason-");
        assert_eq!(sanitize_reason(""), "receiver");
        assert_eq!(sanitize_reason(&"x".repeat(80)).len(), MAX_REASON_CHARS);
        assert!(valid_reason(&sanitize_reason("ünïcode ✓ reason")));
    }

    #[test]
    fn new_request_id_is_sixteen_valid_characters_and_varies() {
        let first = new_request_id();
        let second = new_request_id();
        assert_eq!(first.len(), 16);
        assert!(valid_request_id(&first));
        assert_ne!(first, second);
    }

    #[test]
    fn base64url_encodes_the_rfc4648_vectors_without_padding() {
        assert_eq!(base64url_no_pad(b"f"), "Zg");
        assert_eq!(base64url_no_pad(b"fo"), "Zm8");
        assert_eq!(base64url_no_pad(b"foo"), "Zm9v");
        assert_eq!(base64url_no_pad(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn outbound_messages_pass_the_client_validator() {
        assert!(validate_client_control(&ack_message(7, 42, 1234.56, Some(41))).is_some());
        assert!(validate_client_control(&ack_message(7, 42, 0.0, None)).is_some());
        assert!(validate_client_control(&keyframe_message("sequence-gap")).is_some());
        assert!(validate_client_control(&keyframe_message("bad reason!")).is_some());
        assert_eq!(
            validate_client_control(&ping_message()),
            Some(ClientControl::Ping)
        );
    }

    #[test]
    fn ack_rounds_receive_time_and_omits_decoded_seq_when_unknown() {
        let with = serde_json::from_str::<Value>(&ack_message(7, 42, 1234.56, Some(41))).unwrap();
        assert_eq!(with["recvTsMs"].as_f64(), Some(1234.6));
        assert_eq!(with["decodedSeq"].as_u64(), Some(41));
        let without = serde_json::from_str::<Value>(&ack_message(7, 42, -3.0, None)).unwrap();
        assert_eq!(without["recvTsMs"].as_f64(), Some(0.0));
        assert!(without.get("decodedSeq").is_none());
        assert_eq!(without["type"], "ack");
    }

    #[test]
    fn start_message_uses_browser_values_and_never_carries_audio() {
        let start: Value = serde_json::from_str(&start_message(
            "0123456789abcdef0123456789abcdef",
            Codec::Hevc,
        ))
        .unwrap();
        assert_eq!(start["type"], "media-control");
        assert_eq!(start["action"], "start");
        assert_eq!(start["protocol"], 4);
        assert_eq!(start["codec"], "hevc");
        assert_eq!(start["streamMode"], "work");
        assert_eq!(start["reason"], "client-start");
        assert_eq!(start["quality"], "high");
        assert!(
            start.get("audio").is_none(),
            "step 1 never opts into audio: {start}"
        );
    }

    #[test]
    fn first_frame_and_keyframe_messages_match_the_browser_shape() {
        let request: Value = serde_json::from_str(&first_frame_request(
            "ABCDEFGHIJKLMNOP",
            "0123456789abcdef0123456789abcdef",
        ))
        .unwrap();
        assert_eq!(request["phase"], "request");
        assert_eq!(request["transport"], "wss");
        assert_eq!(request["protocol"], 1);
        let presented: Value =
            serde_json::from_str(&first_frame_presented("ABCDEFGHIJKLMNOP", 9)).unwrap();
        assert_eq!(presented["phase"], "presented");
        assert_eq!(presented["generation"], 9);
        let keyframe: Value = serde_json::from_str(&first_frame_keyframe(
            "0123456789abcdef0123456789abcdef",
            Codec::H264,
        ))
        .unwrap();
        assert_eq!(keyframe["action"], "keyframe");
        assert_eq!(keyframe["reason"], "first-frame-timeout");
        assert_eq!(keyframe["codec"], "h264");
        let hello: Value = serde_json::from_str(&hello_message("media-client")).unwrap();
        assert_eq!(hello, json!({ "type": "hello", "role": "media-client" }));
    }
}
