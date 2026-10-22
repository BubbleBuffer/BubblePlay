//! The approval flow: wait page, ntfy effect, decision, poll, cookie, forget,
//! and the single-use and state rules around them.

mod gate_support;

use bp_relay_core::gate::{
    ConfigError, Gate, GateConfig, GateEffect, GateSnapshot, NtfyConfig, Request, Verdict,
};
use gate_support::*;

const DECIDER: &str = "198.51.100.200";
const UNKNOWN_TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// Open a pending approval for CLIENT at T0 and return the poll token and
/// the (approve, deny) bearers.
fn open(gate: &mut Gate, rng: &mut TestRng) -> (String, String, String) {
    let (page, effects) = wait_parts(get(gate, rng, CLIENT, T0));
    let (approve, deny) = bearer_tokens(&effects[0]);
    (poll_token(&page), approve, deny)
}

#[test]
fn gate_approve_flow_end_to_end() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(100);
    let (page, effects) = wait_parts(get(&mut gate, &mut rng, CLIENT, T0));
    assert_eq!(page.status, 200);
    assert_eq!(effects.len(), 1);
    let token = poll_token(&page);
    let (approve, _) = bearer_tokens(&effects[0]);

    let (pending, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 1_000));
    assert_eq!(state_of(&pending), "pending");

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

    let (approved, approved_effects) =
        handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 3_000));
    assert_eq!(state_of(&approved), "approved");
    let cookie = set_cookie_value(&approved).expect("cookie on approval");
    assert_eq!(snapshot_of(&approved_effects).records.len(), 1);

    let verdict = get_with_cookie(&mut gate, &mut rng, "/", &cookie, T0 + 4_000);
    assert_eq!(verdict, Verdict::Allow);
}

#[test]
fn gate_ntfy_request_matches_the_go_form() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(101);
    let (_, effects) = wait_parts(get(&mut gate, &mut rng, CLIENT, T0));
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
    let (_, effects) = wait_parts(get(&mut gate, &mut rng, CLIENT, T0));
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
    let (_, effects) = wait_parts(get(&mut gate, &mut rng, "2001:db8:1:2:3:4:5:6", T0));
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
    let (_, effects) = wait_parts(get(&mut gate, &mut rng, CLIENT, T0));
    let GateEffect::NtfyPublish { headers, .. } = &effects[0] else {
        panic!("expected an ntfy publish");
    };
    assert!(headers.iter().all(|(key, _)| key != "Authorization"));
}

#[test]
fn gate_deny_flow_refuses_and_offers_no_cookie() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(105);
    let (token, _, deny) = open(&mut gate, &mut rng);
    let (decided, _) = handled_parts(decide(
        &mut gate,
        &mut rng,
        &deny,
        "deny",
        DECIDER,
        T0 + 2_000,
    ));
    assert_eq!(decided.status, 204);

    let (denied, effects) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 3_000));
    assert_eq!(state_of(&denied), "denied");
    assert!(set_cookie_value(&denied).is_none());
    assert!(effects.is_empty());

    let again = poll(&mut gate, &mut rng, &token, CLIENT, T0 + 3_500);
    let (consumed, _) = handled_parts(again);
    assert_eq!(state_of(&consumed), "expired");
}

#[test]
fn gate_expiry_flow_times_out_and_starts_cooldown() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(106);
    let (token, _, _) = open(&mut gate, &mut rng);
    let (expired, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + TTL_MS));
    assert_eq!(state_of(&expired), "expired");
    assert!(set_cookie_value(&expired).is_none());
}

#[test]
fn gate_expiry_follows_the_configured_ttl() {
    let config = GateConfig {
        approval_ttl_secs: 30,
        ..config(true)
    };
    let mut gate = Gate::new(config, GateSnapshot::default(), T0).expect("valid config");
    let mut rng = TestRng::new(107);
    let (token, _, _) = open(&mut gate, &mut rng);
    let (still, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 29_999));
    assert_eq!(state_of(&still), "pending");
    let (gone, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 30_000));
    assert_eq!(state_of(&gone), "expired");
}

#[test]
fn gate_approve_token_cannot_be_used_twice_to_mint_cookies() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(108);
    let (token, approve, _) = open(&mut gate, &mut rng);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 2_000,
    );
    let (first, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 3_000));
    assert_eq!(state_of(&first), "approved");

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
    let (second, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 5_000));
    assert_eq!(state_of(&second), "expired");
}

#[test]
fn gate_deny_token_consumes_the_approve_token() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(109);
    let (token, approve, deny) = open(&mut gate, &mut rng);
    decide(&mut gate, &mut rng, &deny, "deny", DECIDER, T0 + 2_000);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 2_100,
    );
    let (state, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 3_000));
    assert_eq!(state_of(&state), "denied");
}

#[test]
fn gate_approve_token_consumes_the_deny_token() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(110);
    let (token, approve, deny) = open(&mut gate, &mut rng);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 2_000,
    );
    decide(&mut gate, &mut rng, &deny, "deny", DECIDER, T0 + 2_100);
    let (state, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 3_000));
    assert_eq!(state_of(&state), "approved");
}

#[test]
fn gate_decision_with_the_wrong_action_is_ignored() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(111);
    let (token, approve, deny) = open(&mut gate, &mut rng);
    decide(&mut gate, &mut rng, &approve, "deny", DECIDER, T0 + 2_000);
    decide(&mut gate, &mut rng, &deny, "approve", DECIDER, T0 + 2_100);
    let (state, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 3_000));
    assert_eq!(state_of(&state), "pending");
}

#[test]
fn gate_decision_action_is_trimmed_and_case_insensitive() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(112);
    let (token, approve, _) = open(&mut gate, &mut rng);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        " APPROVE\n",
        DECIDER,
        T0 + 2_000,
    );
    let (state, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 3_000));
    assert_eq!(state_of(&state), "approved");
}

#[test]
fn gate_decision_bearer_must_decode_to_32_bytes() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(113);
    let (token, _, _) = open(&mut gate, &mut rng);
    let (short, _) = handled_parts(decide(
        &mut gate,
        &mut rng,
        "short",
        "approve",
        DECIDER,
        T0 + 2_000,
    ));
    assert_eq!(short.status, 204);
    let (state, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 3_000));
    assert_eq!(state_of(&state), "pending");
}

#[test]
fn gate_decision_with_an_unknown_bearer_is_ignored_and_answers_204() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(114);
    let (token, _, _) = open(&mut gate, &mut rng);
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
    let (state, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 3_000));
    assert_eq!(state_of(&state), "pending");
}

#[test]
fn gate_decision_without_authorization_answers_204() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(115);
    let _ = open(&mut gate, &mut rng);
    let request = Request {
        method: "POST",
        path: "/__gate/decision",
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
fn gate_poll_with_an_unknown_token_is_expired() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(116);
    let _ = open(&mut gate, &mut rng);
    let (response, _) = handled_parts(poll(&mut gate, &mut rng, UNKNOWN_TOKEN, CLIENT, T0 + 1_000));
    assert_eq!(state_of(&response), "expired");
}

#[test]
fn gate_poll_with_a_bad_body_is_expired() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(117);
    let _ = open(&mut gate, &mut rng);
    let request = Request {
        body: b"not json",
        ..req("POST", "/__gate/poll", CLIENT, T0 + 1_000)
    };
    let (response, _) = handled_parts(gate.handle(&mut rng, &request));
    assert_eq!(response.status, 200);
    assert_eq!(state_of(&response), "expired");
}

#[test]
fn gate_poll_after_the_cookie_is_minted_is_expired() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(118);
    let (token, approve, _) = open(&mut gate, &mut rng);
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 2_000,
    );
    let (first, _) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 3_000));
    assert_eq!(state_of(&first), "approved");
    let (second, effects) = handled_parts(poll(&mut gate, &mut rng, &token, CLIENT, T0 + 3_500));
    assert_eq!(state_of(&second), "expired");
    assert!(set_cookie_value(&second).is_none());
    assert!(effects.is_empty());
}

#[test]
fn gate_approval_without_ntfy_shows_the_not_configured_page() {
    let mut gate = Gate::new(config(false), GateSnapshot::default(), T0).expect("valid config");
    let mut rng = TestRng::new(119);
    let (page, effects) = wait_parts(get(&mut gate, &mut rng, CLIENT, T0));
    assert_eq!(page.status, 503);
    assert!(effects.is_empty());
    let body = String::from_utf8(page.body).expect("utf-8");
    assert!(body.contains("Approval not configured."));
    assert!(!body.contains("/__gate/poll"));
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

    let verdict = get_with_cookie(&mut gate, &mut rng, "/", &cookie, T0 + 11_000);
    assert!(matches!(verdict, Verdict::Wait(_, _)), "got {verdict:?}");
}

#[test]
fn gate_public_origin_with_trailing_slash_is_rejected() {
    let config = GateConfig {
        public_origin: format!("{ORIGIN}/"),
        ..config(true)
    };
    assert_eq!(
        Gate::new(config, GateSnapshot::default(), T0).err(),
        Some(ConfigError::NonCanonicalPublicOrigin)
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
