//! Limits: waiting approvals (one per prefix, capped), cooldowns, decision rate,
//! and the IPv4 /24 and IPv6 /56 prefixes.

mod gate_support;

use bp_relay_core::gate::{GateEffect, HoldCheck, MAX_PENDING, Verdict};
use gate_support::*;

const DECIDER: &str = "198.51.100.200";
const UNKNOWN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// A navigation held while it opens a new approval, which publishes once.
fn assert_holds(verdict: Verdict) {
    let (_, effects) = hold_parts(verdict);
    assert_eq!(effects.len(), 1);
    assert!(matches!(effects[0], GateEffect::NtfyPublish { .. }));
}

/// A navigation held while it joins its prefix's waiting approval. Nothing is
/// published.
fn assert_joins(verdict: Verdict) {
    let (_, effects) = hold_parts(verdict);
    assert!(effects.is_empty(), "a joining navigation must not publish");
}

/// A navigation dropped with no content (capacity or cooldown).
fn assert_drops(verdict: Verdict) {
    assert!(is_drop(&verdict), "got {verdict:?}");
}

#[test]
fn gate_ten_pending_limit_then_drop_without_publish() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(200);
    for index in 0..MAX_PENDING {
        let client = format!("10.0.{index}.1");
        assert_holds(navigate_request(
            &mut gate,
            &mut rng,
            &req("GET", "/", &client, T0),
        ));
    }
    assert_drops(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", "10.0.10.1", T0),
    ));
    assert_eq!(gate.pending_ids(T0).len(), MAX_PENDING);
}

#[test]
fn gate_one_pending_per_prefix() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(201);
    assert_holds(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", CLIENT, T0),
    ));
    rng.copy_wait_cookie(CLIENT, "203.0.113.200");
    assert_joins(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", "203.0.113.200", T0),
    ));
    assert_eq!(gate.pending_ids(T0).len(), 1);
    assert_holds(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", "203.0.114.1", T0),
    ));
}

#[test]
fn gate_ipv4_prefix_is_slash_24() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(202);
    assert_holds(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", "203.0.113.1", T0),
    ));
    rng.copy_wait_cookie("203.0.113.1", "203.0.113.254");
    assert_joins(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", "203.0.113.254", T0),
    ));
    assert_holds(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", "203.0.112.254", T0),
    ));
}

#[test]
fn gate_ipv6_prefix_is_slash_56() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(203);
    assert_holds(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", "2001:db8:0:100::1", T0),
    ));
    rng.copy_wait_cookie("2001:db8:0:100::1", "2001:db8:0:1ff::1");
    // Same first 56 bits: the high byte of group 4 is 0x01 in both.
    assert_joins(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", "2001:db8:0:1ff::1", T0),
    ));
    // The high byte of group 4 is 0x02 here, so this is a different /56.
    assert_holds(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", "2001:db8:0:200::1", T0),
    ));
}

#[test]
fn gate_ipv4_mapped_address_counts_as_its_ipv4_prefix() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(204);
    assert_holds(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", CLIENT, T0),
    ));
    rng.copy_wait_cookie(CLIENT, "::ffff:203.0.113.9");
    assert_joins(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", "::ffff:203.0.113.9", T0),
    ));
}

#[test]
fn gate_cooldown_after_deny_blocks_the_prefix_for_15_seconds() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(205);
    let (_, _, deny) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    decide(&mut gate, &mut rng, &deny, "deny", DECIDER, T0 + 2_000);
    assert_drops(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", CLIENT, T0 + 2_000 + 14_999),
    ));
    assert_holds(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", CLIENT, T0 + 2_000 + 15_000),
    ));
}

#[test]
fn gate_cooldown_after_expiry_blocks_the_prefix_for_15_seconds() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(206);
    let _ = open_hold(&mut gate, &mut rng, CLIENT, T0);
    assert_drops(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", CLIENT, T0 + TTL_MS + 5_000),
    ));
    assert_holds(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", CLIENT, T0 + TTL_MS + 15_000),
    ));
}

#[test]
fn gate_cooldown_also_follows_an_approval() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(207);
    let (_, approve, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 2_000,
    );
    // The next navigation from the prefix claims the approval...
    let (response, _) = handled_parts(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", CLIENT, T0 + 3_000),
    ));
    assert_eq!(response.status, 302);
    assert!(set_cookie_value(&response).is_some());
    // ...and the prefix cools down after the approval, so a second navigation
    // from it is dropped until the cooldown ends.
    assert_drops(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", CLIENT, T0 + 3_000),
    ));
    assert_holds(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", CLIENT, T0 + 17_000),
    ));
}

#[test]
fn gate_expired_approvals_free_their_pending_slots() {
    let mut cfg = config(true);
    cfg.ntfy_rate.limit = 100;
    let mut gate = bp_relay_core::gate::Gate::new(cfg, Default::default(), T0).unwrap();
    let mut rng = TestRng::new(208);
    for index in 0..MAX_PENDING {
        let client = format!("10.0.{index}.1");
        assert_holds(navigate_request(
            &mut gate,
            &mut rng,
            &req("GET", "/", &client, T0),
        ));
    }
    assert_holds(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", "10.0.20.1", T0 + TTL_MS),
    ));
}

#[test]
fn gate_decision_rate_limit_is_60_per_60_seconds() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(209);
    let (hold, approve, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
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
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 2_000),
        HoldCheck::Waiting
    );
}

#[test]
fn gate_decision_rate_window_resets_after_60_seconds() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(210);
    let (hold, approve, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
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
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 20_000),
        HoldCheck::Waiting
    );
    // The window has passed, so this one is processed.
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 61_000,
    );
    // The approval landed after the hold ended, so the next navigation from the
    // prefix claims it.
    let (response, _) = handled_parts(navigate_request(
        &mut gate,
        &mut rng,
        &req("GET", "/", CLIENT, T0 + 62_000),
    ));
    assert_eq!(response.status, 302);
    assert!(set_cookie_value(&response).is_some());
}

#[test]
fn gate_decision_rate_is_counted_per_prefix() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(211);
    let (hold, approve, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
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
    let (response, _) = admit_parts(check(&mut gate, &mut rng, &hold, T0 + 2_000));
    assert_eq!(response.status, 302);
}
