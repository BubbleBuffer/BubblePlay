use serde_json::Map;

use super::*;

fn object_of(raw: &str) -> Map<String, Value> {
    match serde_json::from_str::<Value>(raw).expect("json") {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn field<'a>(map: &'a Map<String, Value>, key: &str) -> &'a Value {
    map.get(key).expect("field")
}

// Port of the wrong-JSON-type loop in relay/server_test.go
// (TestBrowserGeneratedProtocolFixtures).
#[test]
fn wrong_json_types_fail_typed_helpers() {
    for (raw, expect_integer, expect_boolean, expect_string) in [
        ("null", false, false, false),
        ("\"0\"", false, false, true),
        ("false", false, true, false),
        ("[]", false, false, false),
        ("{}", false, false, false),
        ("0", true, false, false),
    ] {
        let value = serde_json::from_str::<Value>(raw).expect("json");
        let mut map = Map::new();
        map.insert("value".into(), value);
        assert_eq!(integer(&map, "value", 0.0, 100.0), expect_integer, "{raw}");
        assert_eq!(is_boolean(&map, "value"), expect_boolean, "{raw}");
        assert_eq!(bounded_string(&map, "value", 0, 10), expect_string, "{raw}");
    }
    let map = object_of("{\"value\": null}");
    assert!(!integer(&map, "value", 0.0, 100.0));
    assert!(!is_boolean(&map, "value"));
    assert!(!bounded_string(&map, "value", 0, 100));
}

// Go decodes JSON numbers into float64, so all of these behave like Go.
#[test]
fn numbers_follow_go_float64_semantics() {
    let map = object_of(
        r#"{"whole":1.0,"exponent":1e3,"negativeZero":-0,"fraction":1.5,
           "uint32":4294967295,"tooBig":4294967296,"negative":-1,
           "biggest":9007199254740991,"rounded":9007199254740993,
           "huge":123456789012345678901234567890}"#,
    );
    assert!(integer(&map, "whole", 1.0, 1.0));
    assert_eq!(number(&map, "exponent"), Some(1000.0));
    assert!(integer(&map, "negativeZero", 0.0, 4.0));
    assert!(!integer(&map, "fraction", 0.0, 10.0));
    assert!(uint32_value(&map, "uint32"));
    assert!(!uint32_value(&map, "tooBig"));
    assert!(!uint32_value(&map, "negative"));
    assert!(integer(&map, "biggest", 0.0, 9_007_199_254_740_991.0));
    // 2^53+1 rounds to 2^53, which is above the limit, as in Go.
    assert_eq!(number(&map, "rounded"), Some(9_007_199_254_740_992.0));
    assert!(!integer(&map, "rounded", 0.0, 9_007_199_254_740_991.0));
    assert!(integer(&map, "huge", 0.0, 1e30));
    // Go's ParseFloat fails on overflow (the field reads as absent) and
    // silently underflows to 0. The document itself stays valid, because
    // only a validator that reads the field decodes it.
    let extreme = object_of(r#"{"outOfRange":1e400,"underflow":1e-400,"negOverflow":-1e400}"#);
    assert_eq!(number(&extreme, "outOfRange"), None);
    assert_eq!(number(&extreme, "negOverflow"), None);
    assert!(!integer(&extreme, "outOfRange", 0.0, 1e308));
    assert_eq!(number(&extreme, "underflow"), Some(0.0));
    assert!(integer(&extreme, "underflow", 0.0, 0.0));
}

#[test]
fn exact_and_only_compare_the_key_set() {
    let map = object_of(r#"{"type":"bye","action":"deny"}"#);
    assert!(only(&map, &["type", "action"]));
    assert!(!only(&map, &["type"]));
    assert!(exact(&map, &["type", "action"]));
    assert!(!exact(&map, &["type"]));
    assert!(!exact(&map, &["type", "action", "extra"]));
}

#[test]
fn bounded_string_counts_bytes() {
    let mut map = Map::new();
    map.insert("value".into(), Value::String("e".repeat(96)));
    assert!(bounded_string(&map, "value", 0, 96));
    map.insert("value".into(), Value::String("\u{e9}".repeat(48)));
    assert_eq!(text(&map, "value").len(), 96);
    assert!(bounded_string(&map, "value", 0, 96));
    map.insert("value".into(), Value::String("\u{e9}".repeat(49)));
    assert!(!bounded_string(&map, "value", 0, 96));
    assert!(!bounded_string(&map, "missing", 0, 100));
}

#[test]
fn keyframe_reason_matches_go_pattern() {
    assert!(is_keyframe_reason("decoder:lost"));
    assert!(is_keyframe_reason(&"a".repeat(64)));
    assert!(!is_keyframe_reason(&"a".repeat(65)));
    assert!(!is_keyframe_reason(""));
    assert!(!is_keyframe_reason("bad reason!"));
    assert!(!is_keyframe_reason("\u{e9}"));
}

#[test]
fn epoch_matches_go_pattern() {
    assert!(validate_epoch("0123456789abcdef0123456789abcdef"));
    assert!(!validate_epoch("0123456789ABCDEF0123456789ABCDEF"));
    assert!(!validate_epoch("0123456789abcdef0123456789abcdeg"));
    assert!(!validate_epoch(""));
    assert!(!validate_epoch("0123456789abcdef0123456789abcdef0"));
}

#[test]
fn text_helper_returns_empty_for_non_strings() {
    let map = object_of(r#"{"s":"x","n":1,"b":true,"z":null}"#);
    assert_eq!(text(&map, "s"), "x");
    assert_eq!(text(&map, "n"), "");
    assert_eq!(text(&map, "b"), "");
    assert_eq!(text(&map, "z"), "");
    assert_eq!(text(&map, "missing"), "");
    assert_eq!(field(&map, "n"), &Value::from(1));
}
