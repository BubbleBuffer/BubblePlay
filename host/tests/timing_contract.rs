//! fixtures/session-timing-contract.json, "host" section: the PC's link timers and retry schedules.

use std::time::Duration;

use bubbleplay_host::dk_host::EARLY_HELLO_WINDOW;
use bubbleplay_host::signaling::{KEEPALIVE_TIMEOUT, SIGNALING_HANDSHAKE_TIMEOUT};
use bubbleplay_host::transport::RTC_DISCONNECT_GRACE;
use bubbleplay_host::wss_v4::{
    Backoff, ACK_STALL_LIMIT, AUDIO_STALL_LIMIT, HEARTBEAT_INTERVAL, INBOUND_SILENCE_LIMIT,
    MEDIA_REFUSAL_LIMIT, RECONNECT_BACKOFF_MAX, RECONNECT_BACKOFF_MIN, WRITE_STALL_LIMIT,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("../../fixtures/session-timing-contract.json");

fn millis(value: &Value, key: &str) -> Duration {
    Duration::from_millis(
        value[key]
            .as_u64()
            .unwrap_or_else(|| panic!("{key} missing")),
    )
}

#[test]
fn host_timers_match_the_timing_contract() {
    let contract: Value = serde_json::from_str(FIXTURE).expect("fixture parses");
    let values = &contract["host"]["values"];
    assert_eq!(HEARTBEAT_INTERVAL, millis(values, "mediaHeartbeatMs"));
    assert_eq!(
        INBOUND_SILENCE_LIMIT,
        millis(values, "mediaInboundSilenceMs")
    );
    assert_eq!(
        KEEPALIVE_TIMEOUT,
        millis(values, "signalingKeepaliveAckLimitMs")
    );
    assert_eq!(
        SIGNALING_HANDSHAKE_TIMEOUT,
        millis(values, "signalingHandshakeMs")
    );
    assert_eq!(EARLY_HELLO_WINDOW, millis(values, "earlyHelloWindowMs"));
    assert_eq!(ACK_STALL_LIMIT, millis(values, "ackStallMs"));
    assert_eq!(AUDIO_STALL_LIMIT, millis(values, "audioStallMs"));
    assert_eq!(WRITE_STALL_LIMIT, millis(values, "writeStallMs"));
    assert_eq!(RTC_DISCONNECT_GRACE, millis(values, "rtcDisconnectGraceMs"));
    assert_eq!(u64::from(MEDIA_REFUSAL_LIMIT), values["mediaRefusalLimit"]);
}

#[test]
fn host_media_backoff_follows_the_contract_schedule() {
    let contract: Value = serde_json::from_str(FIXTURE).expect("fixture parses");
    let backoff = &contract["host"]["values"]["mediaBackoff"];
    assert_eq!(RECONNECT_BACKOFF_MIN, millis(backoff, "minMs"));
    assert_eq!(RECONNECT_BACKOFF_MAX, millis(backoff, "maxMs"));
    let schedule = backoff["schedule"].as_str().expect("schedule name");
    let expected: Vec<Duration> = contract["schedules"][schedule]
        .as_array()
        .expect("schedule array")
        .iter()
        .map(|ms| Duration::from_millis(ms.as_u64().expect("schedule millis")))
        .collect();
    let mut link = Backoff::default();
    let seen: Vec<Duration> = expected.iter().map(|_| link.next_delay()).collect();
    assert_eq!(seen, expected);
}
