//! Keepalive and pong wire tests (see `signaling.rs`).

use super::*;

#[test]
fn signaling_keepalive_expires_only_after_the_bounded_silence_window() {
    let acknowledged = Instant::now();
    assert!(!keepalive_expired(
        acknowledged,
        acknowledged + KEEPALIVE_TIMEOUT
    ));
    assert!(keepalive_expired(
        acknowledged,
        acknowledged + KEEPALIVE_TIMEOUT + Duration::from_millis(1)
    ));
}

#[test]
fn signaling_pong_has_a_distinct_wire_shape() {
    assert_eq!(
        serde_json::to_string(&SignalMsg::Pong).unwrap(),
        r#"{"type":"pong"}"#
    );
}
