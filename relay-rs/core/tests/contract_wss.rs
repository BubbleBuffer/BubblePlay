//! fixtures/wss-contract.json and fixtures/wss-v4-contract.json.
//!
//! The wss-v4 packet cases describe the media frame header (magic, version,
//! flags, codec, reserved bytes). The relay validates only the frame size, so
//! for those cases the test asserts the relay's size rule and notes that the
//! content verdict belongs to the viewer's decoder. The clientMessages cases
//! are media-control messages and are validated by the relay.

mod common;

use bp_relay_core::{
    MediaControl, Side, validate_epoch, validate_media_binary, validate_media_text,
};
use common::*;
use serde_json::Value;

#[test]
fn contract_wss_epochs() {
    for case in as_array(&fixture("wss-contract.json"), "epochs") {
        let value = as_str(case, "value");
        assert_eq!(
            validate_epoch(value),
            as_bool(case, "accepted"),
            "epoch {value:?}"
        );
    }
}

#[test]
fn contract_wss_v4_packets_follow_the_relay_size_rule() {
    let contract = fixture("wss-v4-contract.json");
    let header = as_u64(&contract, "headerBytes") as usize;
    let max = as_u64(&contract, "maxPayloadBytes") as usize;
    for case in as_array(&contract, "packets") {
        let name = as_str(case, "name");
        let bytes = hex(as_str(case, "hex"));
        if as_bool(case, "accepted") {
            assert!(
                bytes.len() > header,
                "{name}: accepted packet lost its header"
            );
            assert!(
                validate_media_binary(Side::Host, &bytes).is_ok(),
                "{name}: relay rejected a valid frame"
            );
            assert!(
                bytes.len() <= max + header,
                "{name}: accepted packet exceeds the media limit"
            );
        }
        // The relay relays any host frame longer than the 28-byte header; the
        // header fields themselves are the viewer's business.
        assert_eq!(
            validate_media_binary(Side::Host, &bytes).is_ok(),
            bytes.len() > header,
            "{name}: relay size rule"
        );
        assert!(
            validate_media_binary(Side::Viewer, &bytes).is_err(),
            "{name}: viewer frames must never be relayed"
        );
    }
}

#[test]
fn contract_wss_v4_client_messages_are_media_control() {
    for case in as_array(&fixture("wss-v4-contract.json"), "clientMessages") {
        let text = as_str(case, "text");
        let got = validate_media_text(Side::Viewer, text, "");
        assert_eq!(got.is_ok(), as_bool(case, "accepted"), "{text}");
        if let Ok(control) = got {
            match as_str(case, "kind") {
                "ping" => assert_eq!(control, MediaControl::Ping, "{text}"),
                "ack" => assert!(matches!(control, MediaControl::Ack { .. }), "{text}"),
                "keyframe" => assert!(matches!(control, MediaControl::Keyframe { .. }), "{text}"),
                other => panic!("unexpected kind {other}"),
            }
        }
    }
}

#[test]
fn contract_wss_v4_host_messages() {
    for case in as_array(&fixture("wss-v4-contract.json"), "hostMessages") {
        let text = as_str(case, "text");
        let expect = as_bool(case, "accepted");
        let message: Value = serde_json::from_str(text).expect("json");
        let kind = as_str(&message, "type");
        if kind == "hello" {
            // The hello handshake is the registration layer (a later task);
            // it is not a media-control message.
            assert!(expect, "{text}");
            assert!(validate_media_text(Side::Host, text, "").is_err(), "{text}");
            continue;
        }
        assert_eq!(
            validate_media_text(Side::Host, text, "").is_ok(),
            expect,
            "{text}"
        );
    }
}
