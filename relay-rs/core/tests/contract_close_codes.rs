//! fixtures/close-codes-contract.json: the close reasons and codes the relay core defines.

mod common;

use bp_relay_core::relay::{EVICTED_CLOSE_CODE, reasons};
use common::*;

#[test]
fn contract_close_reason_constants() {
    let contract = fixture("close-codes-contract.json");
    let expected = &contract["relayReasons"];
    let constants = [
        ("INVALID_HELLO", reasons::INVALID_HELLO),
        ("INVALID_PACKET", reasons::INVALID_PACKET),
        ("INVALID_PROOF", reasons::INVALID_PROOF),
        ("NOT_PAIRED", reasons::NOT_PAIRED),
        ("PAIRING_CODE", reasons::PAIRING_CODE),
        ("RATE_LIMITED", reasons::RATE_LIMITED),
        ("NOT_AUTHORIZED", reasons::NOT_AUTHORIZED),
        ("TIMED_OUT", reasons::TIMED_OUT),
        ("ACCESS_REMOVED", reasons::ACCESS_REMOVED),
        ("NOT_APPROVED", reasons::NOT_APPROVED),
        ("SESSION_NOT_RESTORED", reasons::SESSION_NOT_RESTORED),
        ("PEER_REMOVED", reasons::PEER_REMOVED),
        ("TOO_MANY_PENDING", reasons::TOO_MANY_PENDING),
    ];
    assert_eq!(
        expected.as_object().map(|map| map.len()),
        Some(constants.len())
    );
    for (name, value) in constants {
        assert_eq!(as_str(expected, name), value, "{name}");
    }
}

#[test]
fn contract_pre_auth_eviction_close() {
    let contract = fixture("close-codes-contract.json");
    let row = as_array(&contract, "closes")
        .iter()
        .find(|row| as_str(row, "id") == "pre-auth-evicted")
        .expect("pre-auth-evicted row");
    assert_eq!(as_u64(row, "code"), u64::from(EVICTED_CLOSE_CODE));
    assert_eq!(as_str(row, "reason"), reasons::TOO_MANY_PENDING);
}
