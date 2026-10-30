//! The approval flow: the held navigation, ntfy effect, decision, claim, cookie,
//! forget, and the single-use and state rules around them.

mod gate_support;

use bp_relay_core::gate::{
    ConfigError, Gate, GateConfig, GateEffect, GateSnapshot, HoldCheck, NtfyConfig, Request,
    Verdict,
};
use gate_support::*;

const DECIDER: &str = "198.51.100.200";
const UNKNOWN_TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

#[test]
fn gate_approve_flow_end_to_end() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(100);
    let (hold, effects) = hold_parts(get(&mut gate, &mut rng, CLIENT, T0));
    assert_eq!(effects.len(), 1);
    let (approve, _) = bearer_tokens(&effects[0]);

    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 1_000),
        HoldCheck::Waiting
    );

    let (decided, decision_effects) = handled_parts(decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 2_000,
    ));
    assert_eq!(decided.status, 204);
    assert!(decision_effects.is_empty());

    let (response, admitted_effects) = admit_parts(check(&mut gate, &mut rng, &hold, T0 + 3_000));
    assert_eq!(response.status, 302);
    let cookie = set_cookie_value(&response).expect("cookie on approval");
    assert_eq!(snapshot_of(&admitted_effects).records.len(), 1);

    let verdict = get_with_cookie(&mut gate, &mut rng, "/", &cookie, T0 + 4_000);
    assert_eq!(verdict, Verdict::Allow);
}

#[test]
fn gate_ntfy_request_matches_the_go_form() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(101);
    let (_, effects) = hold_parts(get(&mut gate, &mut rng, CLIENT, T0));
    let GateEffect::NtfyPublish { url, headers, .. } = &effects[0] else {
        panic!("expected an ntfy publish");
    };
    assert_eq!(url, "https://ntfy.example");
    assert!(headers.contains(&("Content-Type".into(), "application/json".into())));
    assert!(headers.contains(&("Authorization".into(), "Bearer ntfy-secret".into())));

    let json = ntfy_json(&effects[0]);
    assert_eq!(json["topic"], "bp-gate");
    assert_eq!(json["title"], "BubblePlay access request");
    assert_eq!(json["tags"][0], "lock");
    assert_eq!(json["actions"].as_array().map(Vec::len), Some(2));

    let approve = &json["actions"][0];
    assert_eq!(approve["action"], "http");
    assert_eq!(approve["label"], "Approve");
    assert_eq!(approve["url"], "https://relay.example/__gate/decision");
    assert_eq!(approve["method"], "POST");
    assert_eq!(approve["body"], "approve");
    assert_eq!(approve["clear"], true);

    let deny = &json["actions"][1];
    assert_eq!(deny["label"], "Deny");
    assert_eq!(deny["url"], "https://relay.example/__gate/decision");
    assert_eq!(deny["body"], "deny");
    assert_eq!(deny["clear"], true);
}

#[test]
fn gate_ntfy_message_names_the_ipv4_prefix_not_the_full_ip() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(102);
    let (_, effects) = hold_parts(get(&mut gate, &mut rng, CLIENT, T0));
    let json = ntfy_json(&effects[0]);
    assert_eq!(
        json["message"],
        "Browser navigation awaiting approval from client prefix 203.0.113.0/24"
    );
}

#[test]
fn gate_ntfy_message_names_the_ipv6_prefix_not_the_full_ip() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(103);
    let (_, effects) = hold_parts(get(&mut gate, &mut rng, "2001:db8:1:2:3:4:5:6", T0));
    let json = ntfy_json(&effects[0]);
    assert_eq!(
        json["message"],
        "Browser navigation awaiting approval from client prefix 2001:db8:1::/56"
    );
}

#[test]
fn gate_ntfy_without_token_sends_no_authorization_header() {
    let config = GateConfig {
        ntfy: Some(NtfyConfig {
            url: "https://ntfy.example".into(),
            topic: "bp-gate".into(),
            token: None,
        }),
        ..config(true)
    };
    let mut gate = Gate::new(config, GateSnapshot::default(), T0).expect("valid config");
    let mut rng = TestRng::new(104);
    let (_, effects) = hold_parts(get(&mut gate, &mut rng, CLIENT, T0));
    let GateEffect::NtfyPublish { headers, .. } = &effects[0] else {
        panic!("expected an ntfy publish");
    };
    assert!(headers.iter().all(|(key, _)| key != "Authorization"));
}

#[test]
fn gate_deny_flow_drops_the_hold_and_offers_no_cookie() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(105);
    let (hold, _, deny) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    let (decided, _) = handled_parts(decide(
        &mut gate,
        &mut rng,
        &deny,
        "deny",
        DECIDER,
        T0 + 2_000,
    ));
    assert_eq!(decided.status, 204);

    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 3_000),
        HoldCheck::Drop
    );
    assert!(gate.snapshot().records.is_empty());
    assert!(gate.pending_ids(T0 + 3_000).is_empty());
    // The same hold again: the approval is gone, so it stays dropped.
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 3_500),
        HoldCheck::Drop
    );
}

#[test]
fn gate_expiry_flow_times_out_and_starts_cooldown() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(106);
    let (hold, _, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + TTL_MS),
        HoldCheck::Drop
    );
    assert!(gate.pending_ids(T0 + TTL_MS).is_empty());
    // The timed-out approval starts the prefix's cooldown.
    assert!(is_drop(&get(&mut gate, &mut rng, CLIENT, T0 + TTL_MS)));
}

#[test]
fn gate_expiry_follows_the_configured_ttl() {
    let config = GateConfig {
        approval_ttl_secs: 30,
        ..config(true)
    };
    let mut gate = Gate::new(config, GateSnapshot::default(), T0).expect("valid config");
    let mut rng = TestRng::new(107);
    let (hold, _, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    assert_eq!(gate.pending_ids(T0 + 29_999).len(), 1);
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 29_999),
        HoldCheck::Waiting
    );
    assert!(gate.pending_ids(T0 + 30_000).is_empty());
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 30_000),
        HoldCheck::Drop
    );
}

#[test]
fn gate_approve_token_cannot_be_used_twice_to_mint_cookies() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(108);
    let (hold, approve, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 2_000,
    );
    let (first, _) = admit_parts(check(&mut gate, &mut rng, &hold, T0 + 3_000));
    assert!(set_cookie_value(&first).is_some());
    // The hold was claimed by its admit: checking it again is a drop.
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 3_500),
        HoldCheck::Drop
    );

    let (replay, effects) = handled_parts(decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 4_000,
    ));
    assert_eq!(replay.status, 204);
    assert!(effects.is_empty());
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 5_000),
        HoldCheck::Drop
    );
}

#[test]
fn gate_deny_token_consumes_the_approve_token() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(109);
    let (hold, approve, deny) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    decide(&mut gate, &mut rng, &deny, "deny", DECIDER, T0 + 2_000);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 2_100,
    );
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 3_000),
        HoldCheck::Drop
    );
}

#[test]
fn gate_approve_token_consumes_the_deny_token() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(110);
    let (hold, approve, deny) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 2_000,
    );
    decide(&mut gate, &mut rng, &deny, "deny", DECIDER, T0 + 2_100);
    let (response, _) = admit_parts(check(&mut gate, &mut rng, &hold, T0 + 3_000));
    assert_eq!(response.status, 302);
}

#[test]
fn gate_decision_with_the_wrong_action_is_ignored() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(111);
    let (hold, approve, deny) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    decide(&mut gate, &mut rng, &approve, "deny", DECIDER, T0 + 2_000);
    decide(&mut gate, &mut rng, &deny, "approve", DECIDER, T0 + 2_100);
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 3_000),
        HoldCheck::Waiting
    );
}

#[test]
fn gate_decision_action_is_trimmed_and_case_insensitive() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(112);
    let (hold, approve, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        " APPROVE\n",
        DECIDER,
        T0 + 2_000,
    );
    let (response, _) = admit_parts(check(&mut gate, &mut rng, &hold, T0 + 3_000));
    assert_eq!(response.status, 302);
}

#[test]
fn gate_decision_bearer_must_decode_to_32_bytes() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(113);
    let (hold, _, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    let (short, _) = handled_parts(decide(
        &mut gate,
        &mut rng,
        "short",
        "approve",
        DECIDER,
        T0 + 2_000,
    ));
    assert_eq!(short.status, 204);
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 3_000),
        HoldCheck::Waiting
    );
}

#[test]
fn gate_decision_with_an_unknown_bearer_is_ignored_and_answers_204() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(114);
    let (hold, _, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    let (response, effects) = handled_parts(decide(
        &mut gate,
        &mut rng,
        UNKNOWN_TOKEN,
        "approve",
        DECIDER,
        T0 + 2_000,
    ));
    assert_eq!(response.status, 204);
    assert!(response.body.is_empty());
    assert!(effects.is_empty());
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 3_000),
        HoldCheck::Waiting
    );
}

#[test]
fn gate_decision_without_authorization_answers_204() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(115);
    let _ = open_hold(&mut gate, &mut rng, CLIENT, T0);
    let request = Request {
        method: "POST",
        path: "/__gate/decision",
        query: None,
        websocket: false,
        origin: None,
        cookie: None,
        authorization: None,
        sec_fetch_dest: None,
        body: b"approve",
        client_ip: ip(DECIDER),
        now_ms: T0 + 2_000,
    };
    let (response, _) = handled_parts(gate.handle(&mut rng, &request));
    assert_eq!(response.status, 204);
}

#[test]
fn gate_hold_of_an_unknown_approval_is_dropped() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(116);
    let (hold, _, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    // A gate that never issued this approval knows nothing of the hold.
    let mut other = new_gate();
    assert_eq!(
        check(&mut other, &mut rng, &hold, T0 + 1_000),
        HoldCheck::Drop
    );
}

#[test]
fn gate_held_request_checked_after_a_claim_is_dropped() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(118);
    let (hold, approve, _) = open_hold(&mut gate, &mut rng, CLIENT, T0);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 2_000,
    );
    // A navigation from another address in the prefix claims the approval
    // before the held request looks at it.
    let (response, _) = handled_parts(get(&mut gate, &mut rng, "203.0.113.8", T0 + 2_500));
    assert_eq!(response.status, 302);
    assert!(set_cookie_value(&response).is_some());
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + 3_000),
        HoldCheck::Drop
    );
}

#[test]
fn gate_forget_clears_the_cookie_and_its_record() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(120);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let request = Request {
        method: "POST",
        path: "/__gate/forget",
        origin: Some(ORIGIN),
        cookie: Some(&cookie),
        ..req("POST", "/__gate/forget", CLIENT, T0 + 10_000)
    };
    let (response, effects) = handled_parts(gate.handle(&mut rng, &request));
    assert_eq!(response.status, 204);
    let set = header(&response, "Set-Cookie").expect("clearing cookie");
    assert!(set.starts_with("__Host-gate=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Lax"));
    assert!(snapshot_of(&effects).records.is_empty());

    // The cookie is gone. The prefix is still cooling down from its approval.
    let verdict = get_with_cookie(&mut gate, &mut rng, "/", &cookie, T0 + 11_000);
    assert!(is_drop(&verdict), "got {verdict:?}");
    let verdict = get_with_cookie(&mut gate, &mut rng, "/", &cookie, T0 + 16_000);
    assert!(matches!(verdict, Verdict::Hold(_, _)), "got {verdict:?}");
}

#[test]
fn gate_public_origin_trailing_slash_is_stripped() {
    let config = GateConfig {
        public_origin: format!("{ORIGIN}/"),
        ..config(true)
    };
    let mut gate = Gate::new(config, GateSnapshot::default(), T0).expect("valid config");
    let mut rng = TestRng::new(121);
    let mut request = req("GET", "/", CLIENT, T0);
    request.origin = Some(ORIGIN);
    let (_, effects) = hold_parts(gate.handle(&mut rng, &request));
    let json = ntfy_json(&effects[0]);
    assert_eq!(
        json["actions"][0]["url"],
        "https://relay.example/__gate/decision"
    );
}

#[test]
fn gate_approval_ttl_bounds_are_enforced() {
    let with_ttl = |secs: u64| GateConfig {
        approval_ttl_secs: secs,
        ..config(true)
    };
    assert_eq!(
        Gate::new(with_ttl(29), GateSnapshot::default(), T0).err(),
        Some(ConfigError::ApprovalTtlOutOfRange(29))
    );
    assert_eq!(
        Gate::new(with_ttl(121), GateSnapshot::default(), T0).err(),
        Some(ConfigError::ApprovalTtlOutOfRange(121))
    );
    assert!(Gate::new(with_ttl(30), GateSnapshot::default(), T0).is_ok());
    assert!(Gate::new(with_ttl(120), GateSnapshot::default(), T0).is_ok());
}

#[test]
fn gate_empty_public_origin_is_refused() {
    let config = GateConfig {
        public_origin: "/".into(),
        ..config(true)
    };
    assert_eq!(
        Gate::new(config, GateSnapshot::default(), T0).err(),
        Some(ConfigError::EmptyPublicOrigin)
    );
}
