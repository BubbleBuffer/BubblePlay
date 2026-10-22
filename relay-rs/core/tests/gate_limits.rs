//! Limits: pending slots, one per prefix, cooldowns, decision rate, and the
//! IPv4 /24 and IPv6 /56 prefixes.

mod gate_support;

use bp_relay_core::gate::{Gate, GateEffect, Verdict};
use gate_support::*;

const DECIDER: &str = "198.51.100.200";
const UNKNOWN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// Open an approval for `client` at `now` and return its (poll, approve, deny)
/// tokens.
fn open_for(
    gate: &mut Gate,
    rng: &mut TestRng,
    client: &str,
    now: u64,
) -> (String, String, String) {
    let (page, effects) = wait_parts(get(gate, rng, client, now));
    let (approve, deny) = bearer_tokens(&effects[0]);
    (poll_token(&page), approve, deny)
}

fn assert_try_later(verdict: Verdict) {
    let (page, effects) = wait_parts(verdict);
    assert_eq!(page.status, 429);
    assert!(effects.is_empty(), "a limited request must not publish");
    let body = String::from_utf8(page.body).expect("utf-8");
    assert!(body.contains("Try again later."));
}

fn assert_waits(verdict: Verdict) {
    let (page, effects) = wait_parts(verdict);
    assert_eq!(page.status, 200);
    assert_eq!(effects.len(), 1);
    assert!(matches!(effects[0], GateEffect::NtfyPublish { .. }));
}

#[test]
fn gate_ten_pending_limit_then_try_later_without_publish() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(200);
    for index in 0..10 {
        let client = format!("10.0.{index}.1");
        assert_waits(gate.handle(&mut rng, &req("GET", "/", &client, T0)));
    }
    assert_try_later(gate.handle(&mut rng, &req("GET", "/", "10.0.10.1", T0)));
}

#[test]
fn gate_one_pending_per_prefix() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(201);
    assert_waits(gate.handle(&mut rng, &req("GET", "/", CLIENT, T0)));
    assert_try_later(gate.handle(&mut rng, &req("GET", "/", "203.0.113.200", T0)));
    assert_waits(gate.handle(&mut rng, &req("GET", "/", "203.0.114.1", T0)));
}

#[test]
fn gate_ipv4_prefix_is_slash_24() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(202);
    assert_waits(gate.handle(&mut rng, &req("GET", "/", "203.0.113.1", T0)));
    assert_try_later(gate.handle(&mut rng, &req("GET", "/", "203.0.113.254", T0)));
    assert_waits(gate.handle(&mut rng, &req("GET", "/", "203.0.112.254", T0)));
}

#[test]
fn gate_ipv6_prefix_is_slash_56() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(203);
    assert_waits(gate.handle(&mut rng, &req("GET", "/", "2001:db8:0:100::1", T0)));
    // Same first 56 bits: the high byte of group 4 is 0x01 in both.
    assert_try_later(gate.handle(&mut rng, &req("GET", "/", "2001:db8:0:1ff::1", T0)));
    // The high byte of group 4 is 0x02 here, so this is a different /56.
    assert_waits(gate.handle(&mut rng, &req("GET", "/", "2001:db8:0:200::1", T0)));
}

#[test]
fn gate_ipv4_mapped_address_counts_as_its_ipv4_prefix() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(204);
    assert_waits(gate.handle(&mut rng, &req("GET", "/", CLIENT, T0)));
    assert_try_later(gate.handle(&mut rng, &req("GET", "/", "::ffff:203.0.113.9", T0)));
}

#[test]
fn gate_cooldown_after_deny_blocks_the_prefix_for_15_seconds() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(205);
    let (_, _, deny) = open_for(&mut gate, &mut rng, CLIENT, T0);
    decide(&mut gate, &mut rng, &deny, "deny", DECIDER, T0 + 2_000);
    assert_try_later(gate.handle(&mut rng, &req("GET", "/", CLIENT, T0 + 2_000 + 14_999)));
    assert_waits(gate.handle(&mut rng, &req("GET", "/", CLIENT, T0 + 2_000 + 15_000)));
}

#[test]
fn gate_cooldown_after_expiry_blocks_the_prefix_for_15_seconds() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(206);
    let _ = open_for(&mut gate, &mut rng, CLIENT, T0);
    assert_try_later(gate.handle(&mut rng, &req("GET", "/", CLIENT, T0 + TTL_MS + 5_000)));
    assert_waits(gate.handle(&mut rng, &req("GET", "/", CLIENT, T0 + TTL_MS + 15_000)));
}

#[test]
fn gate_cooldown_also_follows_an_approval() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(207);
    let (_, approve, _) = open_for(&mut gate, &mut rng, CLIENT, T0);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 2_000,
    );
    assert_try_later(gate.handle(&mut rng, &req("GET", "/", CLIENT, T0 + 3_000)));
}

#[test]
fn gate_expired_approvals_free_their_pending_slots() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(208);
    for index in 0..10 {
        let client = format!("10.0.{index}.1");
        assert_waits(gate.handle(&mut rng, &req("GET", "/", &client, T0)));
    }
    assert_waits(gate.handle(&mut rng, &req("GET", "/", "10.0.20.1", T0 + TTL_MS)));
}

#[test]
fn gate_decision_rate_limit_is_60_per_60_seconds() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(209);
    let (token, approve, _) = open_for(&mut gate, &mut rng, CLIENT, T0);
    for _ in 0..60 {
        decide(&mut gate, &mut rng, UNKNOWN, "approve", DECIDER, T0 + 1_000);
    }
    // The 61st request from the same prefix is answered 204 but not processed,
    // so a valid approve token does nothing.
    let (response, effects) = handled_parts(decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 1_000,
    ));
    assert_eq!(response.status, 204);
    assert!(effects.is_empty());
    let (state, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 2_000));
    assert_eq!(state_of(&state), "pending");
}

#[test]
fn gate_decision_rate_window_resets_after_60_seconds() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(210);
    let (token, approve, _) = open_for(&mut gate, &mut rng, CLIENT, T0);
    for _ in 0..60 {
        decide(&mut gate, &mut rng, UNKNOWN, "approve", DECIDER, T0 + 1_000);
    }
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 1_000 + 59_999,
    );
    let (still, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 60_000));
    assert_eq!(state_of(&still), "pending");
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 61_000,
    );
    let (done, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 62_000));
    assert_eq!(state_of(&done), "approved");
}

#[test]
fn gate_decision_rate_is_counted_per_prefix() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(211);
    let (token, approve, _) = open_for(&mut gate, &mut rng, CLIENT, T0);
    for _ in 0..60 {
        decide(&mut gate, &mut rng, UNKNOWN, "approve", DECIDER, T0 + 1_000);
    }
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        "192.0.2.1",
        T0 + 1_000,
    );
    let (state, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 2_000));
    assert_eq!(state_of(&state), "approved");
}
