//! fixtures/session-timing-contract.json, "relay" section: the relay's connection deadlines.

mod common;

use bp_relay_core::Timings;
use common::*;

#[test]
fn contract_timing_relay_deadlines() {
    let contract = fixture("session-timing-contract.json");
    let values = &contract["relay"]["values"];
    let timings = Timings::default();
    assert_eq!(timings.hello_ms, as_u64(values, "helloMs"));
    assert_eq!(timings.challenge_ms, as_u64(values, "challengeMs"));
    assert_eq!(timings.idle_ms, as_u64(values, "idleMs"));
}
