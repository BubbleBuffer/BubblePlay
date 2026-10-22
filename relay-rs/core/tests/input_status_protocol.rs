//! Mirrors relay/input_status_protocol_test.go: the input-status `reason` and
//! `route` fields, accepted and rejected, through the signal validator.

use bp_relay_core::{Side, SignalKind, validate_signal_text};
use serde_json::{Map, Value, json};

const EPOCH: &str = "0123456789abcdef0123456789abcdef";

/// The Go test's base input-status message.
fn base() -> Map<String, Value> {
    json!({
        "type": "input-status",
        "protocol": 1,
        "generation": 4,
        "receivedPackets": 12,
        "acceptedPackets": 11,
        "failedPackets": 1,
        "droppedPackets": 1,
        "releasedInputs": 2,
        "requestedEvents": 14,
        "appliedEvents": 13
    })
    .as_object()
    .expect("object")
    .clone()
}

/// Validates the base message with the fields in `extra` merged over it, sent from `side`.
fn validate(side: Side, extra: Value) -> bool {
    let mut message = base();
    for (key, value) in extra.as_object().expect("extra is an object") {
        message.insert(key.clone(), value.clone());
    }
    validate_signal_text(side, &Value::Object(message).to_string(), EPOCH).is_ok()
}

#[test]
fn input_status_old_receipt_is_accepted_on_host() {
    let raw = Value::Object(base()).to_string();
    assert_eq!(
        validate_signal_text(Side::Host, &raw, EPOCH)
            .ok()
            .map(|message| message.kind()),
        Some(SignalKind::InputStatus)
    );
    assert!(!validate(Side::Viewer, json!({})));
}

#[test]
fn input_status_warning_reason_and_route_accepted_on_host_only() {
    for reason in [
        json!(null),
        json!("admin-required"),
        json!("admin-unavailable"),
    ] {
        for route in ["local", "admin", "secure"] {
            let fields = json!({"reason": reason, "route": route});
            assert!(
                validate(Side::Host, fields.clone()),
                "valid warning rejected: {fields}"
            );
            assert!(
                !validate(Side::Viewer, fields.clone()),
                "client warning accepted: {fields}"
            );
        }
    }
}

#[test]
fn input_status_rejects_invalid_reason() {
    for reason in [
        json!(""),
        json!("policy-off"),
        json!(false),
        json!(1),
        json!({}),
        json!([]),
    ] {
        assert!(
            !validate(Side::Host, json!({"reason": reason})),
            "invalid reason accepted: {reason}"
        );
    }
}

#[test]
fn input_status_rejects_invalid_route() {
    for route in [
        json!(null),
        json!(""),
        json!("system"),
        json!(false),
        json!(1),
        json!({}),
        json!([]),
    ] {
        assert!(
            !validate(Side::Host, json!({"route": route})),
            "invalid route accepted: {route}"
        );
    }
}
