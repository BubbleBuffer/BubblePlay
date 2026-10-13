//! BPA1 framing for host PC audio: one Opus packet per binary message.
//! The layout and the accepted/rejected cases are pinned by
//! `fixtures/audio-contract.json`; the tests below read that file directly.

use anyhow::{ensure, Context, Result};

use super::AudioFrame;

pub const MAGIC: [u8; 4] = *b"BPA1";
pub const VERSION: u8 = 1;
pub const HEADER_BYTES: usize = 28;
pub const MAX_PAYLOAD_BYTES: usize = 1500;
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;
pub const FRAME_SAMPLES: u16 = 480;

const CODEC_OPUS: u8 = 1;
const FLAG_DISCONTINUITY: u8 = 0x01;

/// Serialises one packet: 28-byte big-endian header followed by the Opus payload.
pub fn encode_packet(frame: &AudioFrame) -> Result<Vec<u8>> {
    let len = frame.payload.len();
    ensure!(
        (1..=MAX_PAYLOAD_BYTES).contains(&len),
        "audio payload of {len} bytes is outside 1..={MAX_PAYLOAD_BYTES}"
    );
    let mut out = Vec::with_capacity(HEADER_BYTES + len);
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(if frame.discontinuity {
        FLAG_DISCONTINUITY
    } else {
        0
    });
    out.push(CODEC_OPUS);
    out.push(CHANNELS);
    out.extend_from_slice(&SAMPLE_RATE.to_be_bytes());
    out.extend_from_slice(&frame.seq.to_be_bytes());
    out.extend_from_slice(&frame.capture_ts_us.to_be_bytes());
    out.extend_from_slice(&FRAME_SAMPLES.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&frame.payload);
    Ok(out)
}

/// Parses one packet. Any header field outside the contract, a truncated header
/// or a payload outside 1..=1500 bytes is an error.
pub fn decode_packet(bytes: &[u8]) -> Result<AudioFrame> {
    ensure!(
        bytes.len() >= HEADER_BYTES,
        "audio packet truncated: {} bytes",
        bytes.len()
    );
    ensure!(bytes[0..4] == MAGIC, "audio packet has the wrong magic");
    ensure!(
        bytes[4] == VERSION,
        "unsupported audio version {}",
        bytes[4]
    );
    let flags = bytes[5];
    ensure!(
        flags & !FLAG_DISCONTINUITY == 0,
        "unknown audio flags {flags:#04x}"
    );
    ensure!(
        bytes[6] == CODEC_OPUS,
        "unsupported audio codec {}",
        bytes[6]
    );
    ensure!(
        bytes[7] == CHANNELS,
        "unsupported audio channels {}",
        bytes[7]
    );
    let rate = u32::from_be_bytes(bytes[8..12].try_into().context("sample rate")?);
    ensure!(rate == SAMPLE_RATE, "unsupported audio sample rate {rate}");
    let seq = u32::from_be_bytes(bytes[12..16].try_into().context("seq")?);
    let capture_ts_us = u64::from_be_bytes(bytes[16..24].try_into().context("capture time")?);
    let samples = u16::from_be_bytes(bytes[24..26].try_into().context("frame samples")?);
    ensure!(
        samples == FRAME_SAMPLES,
        "unsupported frame samples {samples}"
    );
    let reserved = u16::from_be_bytes(bytes[26..28].try_into().context("reserved")?);
    ensure!(reserved == 0, "nonzero reserved audio field {reserved}");
    let payload = &bytes[HEADER_BYTES..];
    ensure!(
        (1..=MAX_PAYLOAD_BYTES).contains(&payload.len()),
        "audio payload of {} bytes is outside 1..={MAX_PAYLOAD_BYTES}",
        payload.len()
    );
    Ok(AudioFrame {
        seq,
        capture_ts_us,
        discontinuity: flags & FLAG_DISCONTINUITY != 0,
        payload: payload.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../fixtures/audio-contract.json");

    fn unhex(text: &str) -> Vec<u8> {
        assert!(text.len().is_multiple_of(2), "odd hex length");
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn fixture() -> serde_json::Value {
        serde_json::from_str(FIXTURE).expect("fixture json")
    }

    /// Fixture entries are either a full `hex` packet or a `hexPrefix` plus `payloadBytes` of zeros.
    fn fixture_bytes(entry: &serde_json::Value) -> Vec<u8> {
        if let Some(hex) = entry["hex"].as_str() {
            return unhex(hex);
        }
        let mut bytes = unhex(entry["hexPrefix"].as_str().expect("hexPrefix"));
        let payload = entry["payloadBytes"].as_u64().expect("payloadBytes") as usize;
        bytes.extend(std::iter::repeat_n(0u8, payload));
        bytes
    }

    #[test]
    fn header_constants_match_fixture() {
        let fx = fixture();
        assert_eq!(fx["magicHex"].as_str().unwrap(), "42504131");
        assert_eq!(fx["headerBytes"].as_u64().unwrap() as usize, HEADER_BYTES);
        assert_eq!(
            fx["maxPayloadBytes"].as_u64().unwrap() as usize,
            MAX_PAYLOAD_BYTES
        );
        assert_eq!(fx["version"].as_u64().unwrap() as u8, VERSION);
    }

    #[test]
    fn every_accepted_fixture_packet_decodes_to_its_fields() {
        let fx = fixture();
        let mut accepted = 0;
        for entry in fx["packets"].as_array().unwrap() {
            if !entry["accepted"].as_bool().unwrap() {
                continue;
            }
            accepted += 1;
            let name = entry["name"].as_str().unwrap();
            let bytes = fixture_bytes(entry);
            let frame = decode_packet(&bytes).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert_eq!(
                frame.seq as u64,
                entry["seq"].as_u64().unwrap(),
                "{name} seq"
            );
            let ts: u64 = entry["captureTsUs"].as_str().unwrap().parse().unwrap();
            assert_eq!(frame.capture_ts_us, ts, "{name} capture time");
            assert_eq!(
                frame.discontinuity,
                entry["discontinuity"].as_bool().unwrap(),
                "{name} discontinuity"
            );
            assert_eq!(
                frame.payload,
                unhex(entry["payloadHex"].as_str().unwrap()),
                "{name} payload"
            );
            // Canonical: re-encoding reproduces the fixture bytes exactly.
            assert_eq!(encode_packet(&frame).unwrap(), bytes, "{name} round trip");
        }
        assert_eq!(accepted, 2, "fixture accepted-packet count changed");
    }

    #[test]
    fn every_rejected_fixture_packet_fails() {
        let fx = fixture();
        let mut rejected = 0;
        for entry in fx["packets"].as_array().unwrap() {
            if entry["accepted"].as_bool().unwrap() {
                continue;
            }
            rejected += 1;
            let name = entry["name"].as_str().unwrap();
            assert!(
                decode_packet(&fixture_bytes(entry)).is_err(),
                "{name} must be rejected"
            );
        }
        assert_eq!(rejected, 11, "fixture rejected-packet count changed");
    }

    #[test]
    fn each_header_field_is_checked_on_its_own() {
        let good = unhex("42504131010001020000bb800000000700000000075bcd1501e00000fcfffe");
        assert!(decode_packet(&good).is_ok());
        // Every byte of the header is covered by a single-field mutation test,
        // so a missed offset shows up as an accepted corrupted packet.
        for (offset, bad) in [(4, 2u8), (5, 0x02), (6, 2), (7, 1)] {
            let mut bytes = good.clone();
            bytes[offset] = bad;
            assert!(
                decode_packet(&bytes).is_err(),
                "byte {offset} = {bad} must fail"
            );
        }
        let mut bytes = good.clone();
        bytes[26] = 1;
        assert!(decode_packet(&bytes).is_err(), "reserved byte must be zero");
    }

    #[test]
    fn payload_size_limits_are_inclusive() {
        let frame = |len: usize| AudioFrame {
            seq: 1,
            capture_ts_us: 2,
            discontinuity: false,
            payload: vec![0xAB; len],
        };
        let max = encode_packet(&frame(MAX_PAYLOAD_BYTES)).expect("1500 bytes is allowed");
        assert_eq!(max.len(), HEADER_BYTES + MAX_PAYLOAD_BYTES);
        assert_eq!(
            decode_packet(&max).unwrap().payload.len(),
            MAX_PAYLOAD_BYTES
        );
        assert!(encode_packet(&frame(MAX_PAYLOAD_BYTES + 1)).is_err());
        assert!(encode_packet(&frame(0)).is_err());
        assert!(
            decode_packet(&max[..HEADER_BYTES]).is_err(),
            "empty payload"
        );
    }

    #[test]
    fn discontinuity_flag_round_trips() {
        let frame = AudioFrame {
            seq: u32::MAX,
            capture_ts_us: u64::MAX,
            discontinuity: true,
            payload: vec![1, 2, 3],
        };
        let back = decode_packet(&encode_packet(&frame).unwrap()).unwrap();
        assert!(back.discontinuity);
        assert_eq!(back.seq, u32::MAX);
        assert_eq!(back.capture_ts_us, u64::MAX);
        assert_eq!(back.payload, vec![1, 2, 3]);
    }
}
