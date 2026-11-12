//! fixtures/session-timing-contract.json, "clientWin" section: the viewer's session timers and
//! reconnect schedule.

use std::time::Duration;

use serde_json::Value;

use super::gate::next_backoff;
use super::phase2::AUTH_HOST_TIMEOUT;
use super::session::Timing;

const FIXTURE: &str = include_str!("../../../fixtures/session-timing-contract.json");

fn contract() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture parses")
}

fn millis(value: &Value, key: &str) -> Duration {
    Duration::from_millis(
        value[key]
            .as_u64()
            .unwrap_or_else(|| panic!("{key} missing")),
    )
}

#[test]
fn session_timing_defaults_match_the_contract() {
    let contract = contract();
    let values = &contract["clientWin"]["values"];
    let timing = Timing::default();
    assert_eq!(timing.handshake, millis(values, "handshakeStepMs"));
    assert_eq!(timing.heartbeat, millis(values, "heartbeatMs"));
    assert_eq!(timing.dead, millis(values, "deadSilenceMs"));
    assert_eq!(
        timing.keyframe_interval,
        millis(values, "keyframeThrottleMs")
    );
    assert_eq!(timing.first_frame, millis(values, "firstFrameWatchdogMs"));
    assert_eq!(timing.auth_host, millis(values, "authHostTimeoutMs"));
    assert_eq!(AUTH_HOST_TIMEOUT, millis(values, "authHostTimeoutMs"));
    assert_eq!(timing.recycled_budget, millis(values, "recoveryBudgetMs"));
    let recycled = &values["recycledAuthHost"];
    assert_eq!(timing.recycled_auth, millis(recycled, "firstMs"));
    assert_eq!(timing.recycled_auth_max, millis(recycled, "maxMs"));
}

#[test]
fn reconnect_backoff_follows_the_contract_schedule() {
    let contract = contract();
    let backoff = &contract["clientWin"]["values"]["backoff"];
    let timing = Timing::default();
    assert_eq!(timing.backoff_min, millis(backoff, "minMs"));
    assert_eq!(timing.backoff_max, millis(backoff, "maxMs"));
    let schedule = backoff["schedule"].as_str().expect("schedule name");
    let expected: Vec<Duration> = contract["schedules"][schedule]
        .as_array()
        .expect("schedule array")
        .iter()
        .map(|ms| Duration::from_millis(ms.as_u64().expect("schedule millis")))
        .collect();
    let mut previous = Duration::ZERO;
    let seen: Vec<Duration> = expected
        .iter()
        .map(|_| {
            previous = next_backoff(previous, timing.backoff_min, timing.backoff_max);
            previous
        })
        .collect();
    assert_eq!(seen, expected);
}
