//! The silent hold: an unapproved navigation gets no page. It is held while
//! the approval is pending, admitted with a 302 and the cookie on approval,
//! and dropped on deny, timeout, cooldown, capacity or a missing approval path.

mod gate_support;

use bp_relay_core::gate::{
    COOKIE_NAME, Gate, GateEffect, GateSnapshot, HoldCheck, MAX_PENDING, Verdict,
};
use gate_support::*;

#[test]
fn gate_hold_then_approve_admits_the_same_request_with_a_302_and_the_cookie() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(901);
    let mut request = req("GET", "/pair", CLIENT, T0);
    request.query = Some("a=1&b=2");
    let (hold, effects) = hold_parts(gate.handle(&mut rng, &request));
    assert_eq!(hold.until_ms(), T0 + HOLD_MS);
    assert_eq!(effects.len(), 1);
    let (approve, _) = bearer_tokens(&effects[0]);

    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 1_000),
        HoldCheck::Waiting
    );
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 2_000,
    );
    let (response, effects) = admit_parts(check(&mut gate, &mut rng, &hold, T0 + 2_250));
    assert_eq!(response.status, 302);
    assert!(response.body.is_empty());
    assert_eq!(
        header(&response, "Location"),
        Some("https://relay.example/pair?a=1&b=2")
    );
    assert_eq!(header(&response, "Cache-Control"), Some("no-store"));
    let set_cookie = header(&response, "Set-Cookie").expect("cookie");
    assert!(set_cookie.starts_with(&format!("{COOKIE_NAME}=")));
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("Secure"));
    let snapshot = snapshot_of(&effects);
    assert_eq!(snapshot.records.len(), 1);

    let cookie = set_cookie_value(&response).expect("cookie pair");
    let verdict = get_with_cookie(&mut gate, &mut rng, "/", &cookie, T0 + 3_000);
    assert_eq!(verdict, Verdict::Allow);
}

#[test]
fn gate_hold_location_stays_on_the_public_origin() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(902);
    let request = req("GET", "//elsewhere.example/x", CLIENT, T0);
    let (hold, effects) = hold_parts(gate.handle(&mut rng, &request));
    let (approve, _) = bearer_tokens(&effects[0]);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 1_000,
    );
    let (response, _) = admit_parts(check(&mut gate, &mut rng, &hold, T0 + 1_000));
    assert_eq!(
        header(&response, "Location"),
        Some("https://relay.example//elsewhere.example/x")
    );
}

#[test]
fn gate_second_navigation_from_the_prefix_joins_the_waiting_approval() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(903);
    let (first, approve, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    // Same /24, a few seconds later: held too, and nothing is published.
    let (second, effects) = hold_parts(get(&mut gate, &mut rng, "203.0.113.99", T0 + 5_000));
    assert!(effects.is_empty(), "no second ntfy publish");
    assert_eq!(second.until_ms(), T0 + 5_000 + HOLD_MS);
    assert_eq!(gate.pending_ids(T0 + 5_000).len(), 1);

    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 6_000,
    );
    // The approval is claimed once: the first held request to look gets the
    // cookie, the other is dropped.
    let (response, _) = admit_parts(check(&mut gate, &mut rng, &second, T0 + 6_100));
    assert!(set_cookie_value(&response).is_some());
    assert_eq!(
        check(&mut gate, &mut rng, &first, T0 + 6_200),
        HoldCheck::Drop
    );
}

#[test]
fn gate_hold_times_out_into_a_drop() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(904);
    let (hold, _, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + HOLD_MS - 1),
        HoldCheck::Waiting
    );
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + HOLD_MS),
        HoldCheck::Drop
    );
    // The approval itself is still pending until its TTL.
    assert_eq!(gate.pending_ids(T0 + HOLD_MS).len(), 1);
}

#[test]
fn gate_approval_after_the_hold_is_claimed_once_by_the_next_navigation() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(905);
    let (hold, approve, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + HOLD_MS),
        HoldCheck::Drop
    );
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 60_000,
    );

    // The reload, from another address in the prefix, claims it at once.
    let mut reload = req("GET", "/", "203.0.113.8", T0 + 70_000);
    reload.query = Some("r=1");
    let (response, effects) = handled_parts(gate.handle(&mut rng, &reload));
    assert_eq!(response.status, 302);
    assert_eq!(
        header(&response, "Location"),
        Some("https://relay.example/?r=1")
    );
    assert!(set_cookie_value(&response).is_some());
    assert_eq!(snapshot_of(&effects).records.len(), 1);

    // Claimed: the next navigation is dropped (the prefix is cooling down),
    // and after the cooldown it opens a new approval instead of a cookie.
    assert!(is_drop(&get(&mut gate, &mut rng, CLIENT, T0 + 71_000)));
    let (_, effects) = hold_parts(get(&mut gate, &mut rng, CLIENT, T0 + 80_000));
    assert_eq!(effects.len(), 1, "a fresh approval is published");
}

#[test]
fn gate_unclaimed_approval_expires_after_the_approval_ttl() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(906);
    let (_, approve, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 40_000,
    );
    assert_eq!(gate.next_deadline(), Some(T0 + 40_000 + 15_000));
    // Past the claim window the entry is gone: a navigation opens a new
    // approval and gets no cookie.
    let (hold, effects) = hold_parts(get(&mut gate, &mut rng, CLIENT, T0 + 40_000 + TTL_MS));
    assert_eq!(effects.len(), 1);
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 40_000 + TTL_MS + 1),
        HoldCheck::Waiting
    );
}

#[test]
fn gate_approval_from_another_prefix_cannot_be_claimed() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(907);
    let (_, approve, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 1_000,
    );
    let (_, effects) = hold_parts(get(&mut gate, &mut rng, OTHER_PREFIX, T0 + 2_000));
    assert_eq!(effects.len(), 1, "another prefix opens its own approval");
}

#[test]
fn gate_deny_drops_every_held_request_at_once() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(908);
    let (first, _, deny) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    let (second, _) = hold_parts(get(&mut gate, &mut rng, CLIENT, T0 + 1_000));
    decide(&mut gate, &mut rng, &deny, "deny", DECIDER, T0 + 2_000);
    assert_eq!(
        check(&mut gate, &mut rng, &first, T0 + 2_100),
        HoldCheck::Drop
    );
    assert_eq!(
        check(&mut gate, &mut rng, &second, T0 + 2_100),
        HoldCheck::Drop
    );
    assert!(gate.pending_ids(T0 + 2_100).is_empty());
    // Cooling down: dropped without a publish.
    assert!(is_drop(&get(&mut gate, &mut rng, CLIENT, T0 + 3_000)));
}

#[test]
fn gate_capacity_full_drops_without_a_publish() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(909);
    for index in 0..MAX_PENDING {
        let client = format!("10.{index}.0.1");
        let (_, effects) = hold_parts(get(&mut gate, &mut rng, &client, T0));
        assert_eq!(effects.len(), 1);
    }
    assert_eq!(get(&mut gate, &mut rng, "10.200.0.1", T0), Verdict::Drop);
}

#[test]
fn gate_without_ntfy_drops_navigations() {
    let mut gate = Gate::new(config(false), GateSnapshot::default(), T0).expect("valid");
    let mut rng = TestRng::new(910);
    assert_eq!(get(&mut gate, &mut rng, CLIENT, T0), Verdict::Drop);
    assert!(gate.pending_ids(T0).is_empty());
}

#[test]
fn gate_head_without_cookie_is_dropped_and_opens_nothing() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(911);
    let mut request = req("HEAD", "/", CLIENT, T0);
    request.sec_fetch_dest = Some("document");
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Drop);
    assert!(gate.pending_ids(T0).is_empty());
}

#[test]
fn gate_hold_is_clamped_to_at_most_the_default() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(912);
    gate.set_hold_ms(500);
    let (short, _) = hold_parts(get(&mut gate, &mut rng, CLIENT, T0));
    assert_eq!(short.until_ms(), T0 + 500);
    gate.set_hold_ms(10 * HOLD_MS);
    let (long, _) = hold_parts(get(&mut gate, &mut rng, OTHER_PREFIX, T0));
    assert_eq!(long.until_ms(), T0 + HOLD_MS);
}

#[test]
fn gate_the_poll_endpoint_is_gone() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(913);
    let (_, _, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    let mut request = req("POST", "/__gate/poll", CLIENT, T0 + 1_000);
    request.origin = Some(ORIGIN);
    request.body = &br#"{"pending":"x"}"#[..];
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Refuse);
}

#[test]
fn gate_decision_answers_204_with_no_store_and_no_body() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(914);
    let _ = open_hold(&mut gate, &mut rng, CLIENT, T0);
    let (response, _) = handled_parts(decide(
        &mut gate,
        &mut rng,
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "approve",
        DECIDER,
        T0 + 1_000,
    ));
    assert_eq!(response.status, 204);
    assert!(response.body.is_empty());
    assert_eq!(header(&response, "Cache-Control"), Some("no-store"));
    assert_eq!(header(&response, "X-Content-Type-Options"), Some("nosniff"));
}

#[test]
fn gate_forget_response_has_the_security_headers() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(915);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let mut request = req("POST", "/__gate/forget", CLIENT, T0 + 10_000);
    request.origin = Some(ORIGIN);
    request.cookie = Some(&cookie);
    let (response, effects) = handled_parts(gate.handle(&mut rng, &request));
    assert_eq!(header(&response, "Cache-Control"), Some("no-store"));
    assert_eq!(header(&response, "Referrer-Policy"), Some("no-referrer"));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, GateEffect::PersistGate(_)))
    );
}
