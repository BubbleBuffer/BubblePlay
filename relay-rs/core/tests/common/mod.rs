//! Shared helpers for the integration tests.
#![allow(dead_code)]

use bp_relay_core::{Side, validate_signal_text_for_role};
use serde_json::Value;

/// Read a fixture from the repository's fixtures/ directory.
pub fn fixture(name: &str) -> Value {
    load(&format!(
        "{}/../../fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
}

/// Read a fixture from the Go relay's testdata/ directory.
pub fn test_data(name: &str) -> Value {
    load(&format!(
        "{}/../../relay/testdata/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
}

fn load(path: &str) -> Value {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{path}: {error}"));
    serde_json::from_str(&raw).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// Compact JSON, the way the Go tests re-encode a fixture message (unmarshal
/// into any, then marshal again) before validating it.
pub fn compact(value: &Value) -> String {
    serde_json::to_string(value).expect("encode")
}

/// Go's socket role string to a signal side; None for a non-signaling role.
pub fn side_for(role: &str) -> Option<Side> {
    match role {
        "host" => Some(Side::Host),
        "client" | "viewer" => Some(Side::Viewer),
        _ => None,
    }
}

/// Validate a message with Go's role string and the room epoch.
pub fn accepted(role: &str, raw: &str, epoch: &str) -> bool {
    validate_signal_text_for_role(side_for(role), raw, epoch).is_ok()
}

/// Hex decoding for the binary fixture cases.
pub fn hex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd hex length");
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        other => panic!("bad hex digit {other}"),
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| (digit(pair[0]) << 4) | digit(pair[1]))
        .collect()
}

/// A required string field.
pub fn as_str<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} is not a string in {value}"))
}

/// A required unsigned field.
pub fn as_u64(value: &Value, key: &str) -> u64 {
    value[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{key} is not an integer in {value}"))
}

/// A required boolean field.
pub fn as_bool(value: &Value, key: &str) -> bool {
    value[key]
        .as_bool()
        .unwrap_or_else(|| panic!("{key} is not a boolean in {value}"))
}

/// A required array field.
pub fn as_array<'a>(value: &'a Value, key: &str) -> &'a Vec<Value> {
    value[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} is not an array in {value}"))
}

/// A required JSON string.
pub fn text_of(value: &Value) -> &str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("not a string: {value}"))
}
