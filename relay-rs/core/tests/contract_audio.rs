//! fixtures/audio-contract.json.
//!
//! The BPA1 packet cases describe the audio frame header, which the viewer's
//! decoder checks. The relay only relays host frames longer than the 28-byte
//! header, so for the packets the test asserts that size rule (and that a
//! viewer can never send frames). The signaling texts are validated here.

mod common;

use bp_relay_core::{Side, validate_media_binary, validate_signal_text};
use common::*;
use serde_json::Value;

const HEADER: usize = 28;
const EPOCH: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn contract_audio_packets_follow_the_relay_size_rule() {
    let contract = fixture("audio-contract.json");
    assert_eq!(as_u64(&contract, "headerBytes") as usize, HEADER);
    for case in as_array(&contract, "packets") {
        let name = as_str(case, "name");
        let bytes = match case.get("hex").and_then(Value::as_str) {
            Some(text) => hex(text),
            None => {
                let mut bytes = hex(as_str(case, "hexPrefix"));
                bytes.resize(HEADER + as_u64(case, "payloadBytes") as usize, 0xab);
                bytes
            }
        };
        if as_bool(case, "accepted") {
            assert!(validate_media_binary(Side::Host, &bytes).is_ok(), "{name}");
        }
        assert_eq!(
            validate_media_binary(Side::Host, &bytes).is_ok(),
            bytes.len() > HEADER,
            "{name}: relay size rule"
        );
        assert!(
            validate_media_binary(Side::Viewer, &bytes).is_err(),
            "{name}"
        );
    }
}

#[test]
fn contract_audio_host_capabilities() {
    let contract = fixture("audio-contract.json");
    let text = as_str(&contract["signaling"]["hostCapabilities"], "text");
    assert!(accepted("host", text, EPOCH), "{text}");
}

#[test]
fn contract_audio_wss_start_is_a_media_control_from_the_viewer() {
    let contract = fixture("audio-contract.json");
    let text = as_str(&contract["signaling"]["wssStart"], "text");
    assert!(
        validate_signal_text(Side::Viewer, text, EPOCH).is_ok(),
        "{text}"
    );
    assert!(
        validate_signal_text(Side::Viewer, text, "ffffffffffffffffffffffffffffffff").is_err(),
        "stale epoch accepted: {text}"
    );
    assert!(
        validate_signal_text(Side::Host, text, EPOCH).is_err(),
        "viewer message accepted from host: {text}"
    );
}
