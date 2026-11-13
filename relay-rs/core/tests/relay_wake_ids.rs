//! After a hibernation wake the relay never hands out a connection id that an earlier wake issued.
//! A late close or error event for a socket that is gone names its old id, and must not reach a
//! new connection that took it.

mod relay_harness;

use bp_relay_core::{CoreConfig, PcTable, Relay, Route};
use relay_harness::*;

const FIRST_WAKE_MS: u64 = 1_800_000_000_000;
const SECOND_WAKE_MS: u64 = FIRST_WAKE_MS + 5_000;

fn first_id_after_wake(now_ms: u64) -> u64 {
    let (relay, closes) = Relay::rehydrate(
        CoreConfig::new(ORIGIN, TOKEN),
        PcTable::new(),
        Vec::new(),
        now_ms,
    );
    assert!(closes.is_empty());
    let mut h = Harness::new();
    h.relay = relay;
    h.now = now_ms;
    let pc = identity(1).id().to_string();
    h.upgrade(req(Route::HostWs, Some(&pc), None, false)).0
}

#[test]
fn a_later_wake_starts_its_ids_above_an_earlier_one() {
    let first = first_id_after_wake(FIRST_WAKE_MS);
    let second = first_id_after_wake(SECOND_WAKE_MS);
    assert!(second > first, "{second} must be above {first}");
}
