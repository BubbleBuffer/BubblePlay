//! fixtures/file-transfer-contract.json: the file packet layout and the
//! signaling file-offer / host-capabilities cases the relay validates.
//!
//! receiverSessions describe the receiving browser's state machine, not the
//! relay, so they are skipped.

mod common;

use bp_relay_core::{FileKind, Side, validate_file_binary};
use common::*;
use serde_json::Value;

fn contract() -> Value {
    fixture("file-transfer-contract.json")
}

fn kind_for(name: &str) -> FileKind {
    match name {
        "OFFER" => FileKind::Offer,
        "ACCEPT" => FileKind::Accept,
        "DATA" => FileKind::Data,
        "ACK" => FileKind::Ack,
        "DONE" => FileKind::Done,
        "CANCEL" => FileKind::Cancel,
        "RESULT" => FileKind::Result,
        "REQUEST" => FileKind::Request,
        other => panic!("unknown packet type {other}"),
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

#[test]
fn contract_file_packets_are_accepted_and_parsed() {
    let contract = contract();
    let types = &contract["types"];
    for case in as_array(&contract, "packets") {
        let name = as_str(case, "name");
        let bytes = hex(as_str(case, "hex"));
        let packet = validate_file_binary(Side::Viewer, &bytes)
            .unwrap_or_else(|_| panic!("{name}: valid packet rejected"));
        let expected = kind_for(as_str(case, "type"));
        assert_eq!(packet.kind, expected, "{name}: type");
        assert_eq!(expected.code(), bytes[4], "{name}: header code");
        assert_eq!(
            u32_at(&bytes, 8),
            as_u64(case, "transferId") as u32,
            "{name}: transferId"
        );
        assert_eq!(
            u32_at(&bytes, 12),
            as_u64(case, "seq") as u32,
            "{name}: seq"
        );
        assert_eq!(
            packet.payload,
            hex(as_str(case, "payloadHex")).as_slice(),
            "{name}: payload"
        );
        // The fixture's type table and the packet bytes agree.
        assert_eq!(
            as_u64(types, as_str(case, "type")) as u8,
            bytes[4],
            "{name}: types table"
        );
        // Every one of these is a client packet.
        if expected != FileKind::Request {
            assert!(
                validate_file_binary(Side::Host, &bytes).is_ok(),
                "{name}: host packet"
            );
        } else {
            assert!(
                validate_file_binary(Side::Host, &bytes).is_err(),
                "{name}: REQUEST from host"
            );
        }
    }
}

#[test]
fn contract_file_packets_rejected() {
    let contract = contract();
    for case in as_array(&contract, "rejected") {
        let name = as_str(case, "name");
        let bytes = rejected_bytes(case);
        assert!(
            validate_file_binary(Side::Viewer, &bytes).is_err(),
            "{name}: relay accepted a rejected packet"
        );
        assert!(
            validate_file_binary(Side::Host, &bytes).is_err(),
            "{name}: relay accepted a rejected packet from the host"
        );
    }
}

/// The packet of a rejected case: its full hex, or (for an oversize packet)
/// the header hex padded with 0xab to totalBytes.
fn rejected_bytes(case: &Value) -> Vec<u8> {
    match case.get("hex").and_then(Value::as_str) {
        Some(text) => hex(text),
        None => {
            let mut bytes = hex(as_str(case, "headerHex"));
            let total = as_u64(case, "totalBytes") as usize;
            assert!(total >= bytes.len(), "totalBytes below the header");
            bytes.resize(total, 0xab);
            bytes
        }
    }
}

#[test]
fn contract_file_offer_messages() {
    let contract = contract();
    for case in as_array(&contract["signaling"], "fileOffer") {
        let text = as_str(case, "text");
        let expect = as_bool(case, "accepted");
        assert_eq!(accepted("host", text, ""), expect, "{text}");
        if expect {
            assert!(
                !accepted("client", text, ""),
                "file-offer accepted from viewer: {text}"
            );
        }
    }
}

#[test]
fn contract_file_host_capabilities() {
    let case = &contract()["signaling"]["hostCapabilities"];
    let text = as_str(case, "text");
    assert!(accepted("host", text, ""), "{text}");
    assert!(!accepted("client", text, ""), "{text}");
}
