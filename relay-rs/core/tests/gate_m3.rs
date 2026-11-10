//! The gate's next deadline (M3-B): it reports when its next timer is due, so
//! the adapter can wake it up. Reading it changes nothing about the state.

mod gate_support;

use bp_relay_core::gate::{COOKIE_MAX_AGE_SECS, Gate, GateEffect, Response};
use gate_support::*;

const COOKIE_MS: u64 = COOKIE_MAX_AGE_SECS * 1000;
const DECIDER: &str = "198.51.100.200";

/// Runs the approval for CLIENT and returns the admitted 302 and its effects.
/// The approval is decided at T0 + 1_000 and the held navigation is checked at
/// `check_at`, which mints the cookie.
fn approve_at(gate: &mut Gate, rng: &mut TestRng, check_at: u64) -> (Response, Vec<GateEffect>) {
    let (hold, approve, _) = open_hold(gate, rng, CLIENT, T0);
    decide(gate, rng, &approve, "approve", DECIDER, T0 + 1_000);
    admit_parts(check(gate, rng, &hold, check_at))
}

#[test]
fn m3_a_fresh_gate_has_no_deadline() {
    assert_eq!(new_gate().next_deadline(), None);
}

#[test]
fn m3_pending_approval_is_due_at_the_deadline_it_reports() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(400);
    let _ = open_hold(&mut gate, &mut rng, CLIENT, T0);
    let due = gate
        .next_deadline()
        .expect("a waiting approval has a deadline");
    assert!(due <= T0 + TTL_MS, "due {due}");

    assert!(gate.tick(due - 1).is_empty());
    assert_eq!(gate.next_deadline(), Some(due));

    let _ = gate.tick(due);
    assert!(
        gate.next_deadline().is_none_or(|next| next > due),
        "after the deadline: {:?}",
        gate.next_deadline()
    );
}

#[test]
fn m3_cookie_is_due_at_its_expiry_and_then_nothing_is_tracked() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(401);
    let mint = T0 + 2_000;
    let _ = approve_at(&mut gate, &mut rng, mint);

    // Past the approval's keep time, so only the cookie is left to wake for.
    let _ = gate.tick(T0 + 600_000);
    let expires = mint + COOKIE_MS;
    assert_eq!(gate.next_deadline(), Some(expires));

    assert!(gate.tick(expires - 1).is_empty());
    assert_eq!(gate.next_deadline(), Some(expires));

    let effects = gate.tick(expires);
    assert!(snapshot_of(&effects).records.is_empty());
    assert_eq!(gate.next_deadline(), None);
}
