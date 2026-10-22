//! relay/testdata/worker-contract.json: the shapes the browser and the host
//! really produce (port of TestBrowserGeneratedProtocolFixtures in
//! relay/server_test.go).

mod common;

use bp_relay_core::{FileKind, Side, validate_file_binary, validate_media_binary};
use common::*;

#[test]
fn contract_worker_signals_are_accepted() {
    let fixture = test_data("worker-contract.json");
    let epoch = as_str(&fixture, "epoch");
    for signal in as_array(&fixture, "signals") {
        let role = as_str(signal, "role");
        let text = compact(&signal["message"]);
        assert!(
            accepted(role, &text, epoch),
            "{role} producer shape rejected: {text}"
        );
    }
}

#[test]
fn contract_worker_file_packets_are_accepted() {
    let fixture = test_data("worker-contract.json");
    for (index, value) in as_array(&fixture, "files").iter().enumerate() {
        let bytes = hex(text_of(value));
        let packet = validate_file_binary(Side::Viewer, &bytes)
            .unwrap_or_else(|_| panic!("packet {index} rejected"));
        // Packet 3 is the client's REQUEST and must not come from the host.
        if index == 3 {
            assert_eq!(packet.kind, FileKind::Request);
            assert!(validate_file_binary(Side::Host, &bytes).is_err());
        }
    }
}

#[test]
fn contract_worker_media_frame_is_accepted() {
    let fixture = test_data("worker-contract.json");
    let frame = hex(as_str(&fixture, "mediaHex"));
    assert!(validate_media_binary(Side::Host, &frame).is_ok());
}
