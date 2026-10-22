//! fixtures/secure-desktop-contract.json: every case that describes a message
//! the relay validates (port of relay/secure_desktop_protocol_test.go).
//!
//! policyCases and streamingTransition describe host-side policy and capture
//! behaviour, not wire validation, so they are skipped here.

mod common;

use bp_relay_core::validate_signal_text;
use common::*;
use serde_json::Value;

fn contract() -> Value {
    fixture("secure-desktop-contract.json")
}

/// The Go test's checkValid: the message is accepted for its role, rejected
/// for the other signaling role, accepted when padded to the 512-byte ceiling
/// and rejected one byte past it.
fn check_valid(message: &Value, role: &str) {
    let max = as_u64(&contract()["limits"], "messageMaxBytes") as usize;
    let text = compact(message);
    assert!(
        accepted(role, &text, ""),
        "valid {role} message rejected: {text}"
    );
    let wrong = if role == "host" { "client" } else { "host" };
    assert!(!accepted(wrong, &text, ""), "wrong role accepted: {text}");
    let padded = format!("{text}{}", " ".repeat(max - text.len()));
    assert!(
        accepted(role, &padded, ""),
        "wire byte boundary not met: {text}"
    );
    assert!(
        !accepted(role, &format!("{padded} "), ""),
        "wire byte boundary not enforced: {text}"
    );
}

#[test]
fn contract_secure_desktop_states_are_host_messages() {
    let contract = contract();
    let states = as_array(&contract, "desktopStates");
    assert!(!states.is_empty());
    for state in states {
        check_valid(state, "host");
    }
}

#[test]
fn contract_secure_desktop_recovered_state() {
    check_valid(&contract()["recoveredState"], "host");
}

#[test]
fn contract_secure_actions_are_viewer_messages() {
    for action in as_array(&contract(), "secureActions") {
        check_valid(action, "client");
    }
}

#[test]
fn contract_secure_action_results_are_host_messages() {
    for result in as_array(&contract(), "secureActionResults") {
        check_valid(result, "host");
    }
}

#[test]
fn contract_secure_desktop_invalid_messages_are_rejected() {
    let contract = contract();
    for case in as_array(&contract, "invalidMessages") {
        let name = as_str(case, "name");
        let role = as_str(case, "role");
        let text = compact(&case["message"]);
        assert!(
            !accepted(role, &text, ""),
            "invalid case accepted: {name}: {text}"
        );
    }
}

#[test]
fn contract_secure_desktop_role_cases() {
    let contract = contract();
    for case in as_array(&contract, "roleCases") {
        let role = as_str(case, "role");
        let expect = as_bool(case, "accepted");
        let text = compact(&case["message"]);
        assert_eq!(accepted(role, &text, ""), expect, "role {role}: {text}");
    }
}

#[test]
fn contract_secure_desktop_reason_byte_limit() {
    let contract = contract();
    let max = as_u64(&contract["limits"], "reasonMaxUtf8Bytes") as usize;
    for key in ["desktopStates", "secureActionResults"] {
        let mut message = contract[key][0].clone();
        for count in [48, 49] {
            message["reason"] = Value::String("\u{e9}".repeat(count));
            let text = compact(&message);
            assert_eq!(
                accepted("host", &text, ""),
                count * 2 <= max,
                "UTF-8 reason boundary: count={count}"
            );
        }
    }
}

#[test]
fn contract_secure_desktop_webrtc_in_place_transition() {
    let contract = contract();
    let transition = &contract["webrtcInPlaceTransition"];
    for key in ["admitted", "legacyAdmitted"] {
        let message = &transition[key];
        assert_eq!(as_str(message, "type"), "first-frame");
        assert!(accepted("host", &compact(message), ""), "{key} rejected");
        assert!(
            !accepted("client", &compact(message), ""),
            "{key} accepted from viewer"
        );
        // A first-frame message is only valid with a well-formed requestId.
        assert!(validate_signal_text(bp_relay_core::Side::Host, &compact(message), "").is_ok());
    }
}
