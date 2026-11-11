//! Hardening: navigation-only approvals, HEAD, the per-prefix caps, redacted
//! Debug output, saturating time arithmetic and canonical public origins.

mod gate_support;

use bp_relay_core::gate::{
    ConfigError, DECISION_WINDOW_MS, Gate, GateConfig, GateSnapshot, MAX_PENDING,
    MAX_TRACKED_PREFIXES, Verdict,
};
use gate_support::*;

const DECIDER: &str = "198.51.100.200";
const UNKNOWN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// A distinct client prefix per index, all under 10.0.0.0/12.
fn client(index: usize) -> String {
    format!("10.{}.{}.1", index / 256, index % 256)
}

#[test]
fn gate_document_navigation_opens_an_approval() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(500);
    let mut request = req("GET", "/", CLIENT, T0);
    request.sec_fetch_dest = Some("document");
    let (hold, effects) = hold_parts(navigate_request(&mut gate, &mut rng, &request));
    assert_eq!(hold.until_ms(), T0 + HOLD_MS);
    assert_eq!(effects.len(), 1);
    assert_eq!(gate.pending_ids(T0).len(), 1);
}

#[test]
fn gate_approval_requires_both_navigation_fetch_metadata_headers() {
    for (mode, dest) in [
        (None, None),
        (None, Some("document")),
        (Some("navigate"), None),
        (Some("cors"), Some("document")),
        (Some("no-cors"), Some("document")),
        (Some("navigate"), Some("iframe")),
        (Some("Navigate"), Some("document")),
    ] {
        let mut gate = new_gate();
        let mut rng = TestRng::new(501);
        let mut request = req("GET", "/", CLIENT, T0);
        request.sec_fetch_mode = mode;
        request.sec_fetch_dest = dest;
        let (response, effects) = handled_parts(gate.handle(&mut rng, &request));
        assert_eq!(response.status, 403, "mode={mode:?}, dest={dest:?}");
        assert!(effects.is_empty());
        assert!(gate.pending_ids(T0).is_empty());
    }
}

#[test]
fn gate_non_navigation_gets_403_and_opens_nothing() {
    for dest in [
        "image",
        "script",
        "style",
        "empty",
        "iframe",
        "",
        "DOCUMENTX",
    ] {
        let mut gate = new_gate();
        let mut rng = TestRng::new(502);
        let mut request = req("GET", "/app.js", CLIENT, T0);
        request.sec_fetch_dest = Some(dest);
        let (page, effects) = handled_parts(navigate_request(&mut gate, &mut rng, &request));
        assert_eq!(page.status, 403, "dest {dest:?}");
        assert!(effects.is_empty(), "dest {dest:?}");
        assert!(gate.pending_ids(T0).is_empty(), "dest {dest:?}");
    }
}

#[test]
fn gate_post_with_document_dest_gets_403_and_opens_nothing() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(503);
    let mut request = req("POST", "/pair", CLIENT, T0);
    request.sec_fetch_dest = Some("document");
    let (page, effects) = handled_parts(navigate_request(&mut gate, &mut rng, &request));
    assert_eq!(page.status, 403);
    assert!(effects.is_empty());
    assert!(gate.pending_ids(T0).is_empty());
}

#[test]
fn gate_non_navigation_with_valid_cookie_is_allowed() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(505);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let mut request = req("GET", "/app.js", CLIENT, T0 + 10_000);
    request.sec_fetch_dest = Some("script");
    request.cookie = Some(&cookie);
    assert_eq!(
        navigate_request(&mut gate, &mut rng, &request),
        Verdict::Allow
    );
}

#[test]
fn gate_decision_map_is_capped_and_pruned_after_its_window() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(506);
    for index in 0..MAX_TRACKED_PREFIXES {
        let (response, _) = handled_parts(decide(
            &mut gate,
            &mut rng,
            UNKNOWN,
            "approve",
            &client(index),
            T0,
        ));
        assert_eq!(response.status, 204);
    }
    let (full, _) = handled_parts(decide(
        &mut gate,
        &mut rng,
        UNKNOWN,
        "approve",
        "10.16.0.1",
        T0,
    ));
    assert_eq!(full.status, 429);
    // A prefix already counted is not a new arrival, so it is still answered.
    let (known, _) = handled_parts(decide(
        &mut gate,
        &mut rng,
        UNKNOWN,
        "approve",
        &client(0),
        T0,
    ));
    assert_eq!(known.status, 204);
    // Once the window has passed, the expired entries are pruned and the new
    // prefix fits.
    let (after, _) = handled_parts(decide(
        &mut gate,
        &mut rng,
        UNKNOWN,
        "approve",
        "10.16.0.1",
        T0 + DECISION_WINDOW_MS,
    ));
    assert_eq!(after.status, 204);
}

#[test]
fn gate_pending_list_is_capped_and_recovers_when_an_approval_is_denied() {
    let mut cfg = config(true);
    cfg.ntfy_rate.limit = 100;
    let mut gate = Gate::new(cfg, GateSnapshot::default(), T0).unwrap();
    let mut rng = TestRng::new(507);
    let mut denies = Vec::new();
    for index in 0..MAX_PENDING {
        let (_, _, deny) = open_hold(&mut gate, &mut rng, &client(index), T0);
        denies.push(deny);
    }
    // Every slot holds a waiting approval: the next prefix is dropped with no publish.
    assert!(is_drop(&get(&mut gate, &mut rng, "10.16.0.1", T0 + 2_000)));
    assert_eq!(gate.pending_ids(T0 + 2_000).len(), MAX_PENDING);
    // Denying one frees its slot at once: the removed entry is not kept.
    decide(&mut gate, &mut rng, &denies[0], "deny", DECIDER, T0 + 3_000);
    let (_, effects) = hold_parts(get(&mut gate, &mut rng, "10.16.0.1", T0 + 4_000));
    assert_eq!(effects.len(), 1);
}

#[test]
fn gate_debug_output_never_shows_the_ntfy_token_or_the_bearers() {
    let cfg = config(true);
    let mut gate = Gate::new(cfg.clone(), GateSnapshot::default(), T0).expect("valid config");
    let mut rng = TestRng::new(508);
    let (hold, effects) = hold_parts(get(&mut gate, &mut rng, CLIENT, T0));
    let (approve, deny) = bearer_tokens(&effects[0]);
    let mut dump = format!(
        "{cfg:?} {effects:?} {:?}",
        Verdict::Hold(hold, effects.clone())
    );
    let authorization = format!("Bearer {approve}");
    let mut request = req("POST", "/__gate/decision", DECIDER, T0);
    request.authorization = Some(&authorization);
    dump.push_str(&format!("{request:?}"));
    // The URL and topic stay visible, so the dump is still useful.
    assert!(dump.contains("https://ntfy.example"));
    assert!(dump.contains("<redacted>"));
    assert!(!dump.contains("ntfy-secret"));
    assert!(!dump.contains(&approve));
    assert!(!dump.contains(&deny));
}

#[test]
fn gate_time_near_u64_max_does_not_overflow() {
    let now = u64::MAX - 10;
    let mut gate = Gate::new(config(true), GateSnapshot::default(), now).expect("valid config");
    let mut rng = TestRng::new(509);
    let (hold, approve, _) = open_hold(&mut gate, &mut rng, CLIENT, now);
    assert_eq!(hold.until_ms(), u64::MAX);
    decide(&mut gate, &mut rng, &approve, "approve", DECIDER, now);
    let (response, _) = admit_parts(check(&mut gate, &mut rng, &hold, now));
    assert!(set_cookie_value(&response).is_some());
    let _ = gate.tick(u64::MAX);
    assert!(gate.pending_ids(u64::MAX).is_empty());
}

fn origin_error(origin: &str) -> Option<ConfigError> {
    let cfg = GateConfig {
        public_origin: origin.to_string(),
        ..config(true)
    };
    Gate::new(cfg, GateSnapshot::default(), T0).err()
}

#[test]
fn gate_accepts_canonical_bare_origins() {
    for origin in [
        "https://relay.example",
        "http://localhost",
        "https://relay.example:8443",
        "http://127.0.0.1:8080",
        "https://[2001:db8::1]",
        "https://[2001:db8::1]:8443",
        "https://a-b.example.com",
        // One trailing slash is canonicalised away.
        "https://relay.example/",
        "https://relay.example:8443/",
    ] {
        assert!(
            origin_error(origin).is_none(),
            "{origin} should be accepted"
        );
    }
}

#[test]
fn gate_rejects_non_canonical_public_origins() {
    for origin in [
        "https://Relay.example",
        "https://relay.example//",
        "https://relay.example/path",
        "https://relay.example/path/",
        "https://relay.example?q=1",
        "https://relay.example#frag",
        "https://relay.example:443",
        "http://relay.example:80",
        "https://relay.example:0443",
        "https://relay.example:0",
        "https://relay.example:",
        "https://relay.example:99999",
        "https://user@relay.example",
        "https://relay example",
        "relay.example",
        "ftp://relay.example",
        "https://",
        "https://[2001:db8::1",
        "https://[]:8443",
    ] {
        assert_eq!(
            origin_error(origin),
            Some(ConfigError::NonCanonicalPublicOrigin),
            "{origin}"
        );
    }
    assert_eq!(origin_error(""), Some(ConfigError::EmptyPublicOrigin));
}

#[test]
fn gate_wait_cookie_binds_both_held_and_late_approval_claims() {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let mut gate = new_gate();
    let mut rng = TestRng::new(510);
    let initial = req("GET", "/pair", CLIENT, T0);
    let (response, effects) = handled_parts(gate.handle(&mut rng, &initial));
    assert_eq!(response.status, 302);
    assert_eq!(
        header(&response, "Location"),
        Some("https://relay.example/pair")
    );
    let set = header(&response, "Set-Cookie").expect("waiting cookie");
    assert!(set.starts_with("__Host-bp_wait="));
    for attribute in ["Path=/", "Secure", "HttpOnly", "SameSite=Strict"] {
        assert!(set.contains(attribute), "{attribute}");
    }
    let cookie = set.split(';').next().unwrap().to_string();
    let value = cookie.split_once('=').unwrap().1;
    assert_eq!(URL_SAFE_NO_PAD.decode(value).unwrap().len(), 32);
    assert!(gate.snapshot().records.is_empty());
    let (approve, _) = bearer_tokens(&effects[0]);

    // A second browser on the same network cannot join the original hold.
    assert_eq!(
        gate.handle(&mut rng, &req("GET", "/pair", "203.0.113.8", T0 + 1)),
        Verdict::Drop
    );
    let wrong = format!("__Host-bp_wait={UNKNOWN}");
    let mut follow = req("GET", "/pair", CLIENT, T0 + 1);
    follow.cookie = Some(&wrong);
    assert_eq!(gate.handle(&mut rng, &follow), Verdict::Drop);
    follow.cookie = Some(&cookie);
    let (hold, more) = hold_parts(gate.handle(&mut rng, &follow));
    assert!(more.is_empty());
    assert_eq!(
        check(&mut gate, &mut rng, &hold, T0 + HOLD_MS + 1),
        bp_relay_core::gate::HoldCheck::Drop
    );
    decide(
        &mut gate,
        &mut rng,
        &approve,
        "approve",
        DECIDER,
        T0 + 60_000,
    );

    // Late approval still needs the secret, not merely a shared /24.
    for supplied in [None, Some(wrong.as_str())] {
        let mut thief = req("GET", "/pair", "203.0.113.8", T0 + 61_000);
        thief.cookie = supplied;
        assert_eq!(gate.handle(&mut rng, &thief), Verdict::Drop);
    }
    // Even the right secret cannot claim an approval from another prefix.
    let mut moved = req("GET", "/pair", OTHER_PREFIX, T0 + 61_000);
    moved.cookie = Some(&cookie);
    let (separate, _) = handled_parts(gate.handle(&mut rng, &moved));
    assert!(set_cookie_value(&separate).is_none());
    // Original browser can still claim exactly once; failed claims did not consume it.
    follow.now_ms = T0 + 62_000;
    let (admitted, _) = handled_parts(gate.handle(&mut rng, &follow));
    assert!(set_cookie_value(&admitted).is_some());
    assert_eq!(gate.handle(&mut rng, &follow), Verdict::Drop);
}

#[test]
fn gate_ntfy_budget_is_global_and_resets_after_ten_minutes() {
    use bp_relay_core::gate::DEFAULT_NTFY_RATE;
    let mut gate = new_gate();
    let mut rng = TestRng::new(511);
    // Free every pending slot immediately: this tests the publish budget,
    // rather than the simultaneous-pending cap or the per-prefix cooldown.
    for index in 0..10 {
        let now = T0 + index as u64;
        let (response, effects) =
            handled_parts(gate.handle(&mut rng, &req("GET", "/", &client(index), now)));
        assert_eq!(response.status, 302);
        let (_, deny) = bearer_tokens(&effects[0]);
        decide(&mut gate, &mut rng, &deny, "deny", DECIDER, now);
    }
    assert!(gate.pending_ids(T0 + 10).is_empty());
    assert_eq!(
        gate.handle(
            &mut rng,
            &req(
                "GET",
                "/",
                &client(11),
                T0 + DEFAULT_NTFY_RATE.window_ms - 1
            )
        ),
        Verdict::Drop
    );
    let (_, effects) = handled_parts(gate.handle(
        &mut rng,
        &req("GET", "/", &client(11), T0 + DEFAULT_NTFY_RATE.window_ms),
    ));
    assert_eq!(effects.len(), 1);
}

#[test]
fn gate_rate_budgets_accept_configuration_and_share_prefix_auth_failures() {
    use bp_relay_core::gate::{Prefix, RateLimit};
    let mut cfg = config(true);
    cfg.ntfy_rate = RateLimit {
        limit: 1,
        window_ms: 1_000,
    };
    cfg.auth_failure_rate = RateLimit {
        limit: 2,
        window_ms: 1_000,
    };
    let mut gate = Gate::new(cfg, GateSnapshot::default(), T0).unwrap();
    let mut rng = TestRng::new(512);
    let (_, effects) = handled_parts(gate.handle(&mut rng, &req("GET", "/", CLIENT, T0)));
    assert_eq!(effects.len(), 1);
    assert_eq!(
        gate.handle(&mut rng, &req("GET", "/", OTHER_PREFIX, T0 + 999)),
        Verdict::Drop
    );
    let (_, effects) =
        handled_parts(gate.handle(&mut rng, &req("GET", "/", OTHER_PREFIX, T0 + 1_000)));
    assert_eq!(effects.len(), 1);

    assert!(gate.auth_allowed(ip(CLIENT), T0 + 1_000));
    gate.record_auth_failure(ip(CLIENT), T0 + 1_000);
    assert!(gate.auth_allowed(ip("203.0.113.9"), T0 + 1_001));
    gate.record_auth_prefix_failure(Prefix::of(ip("::ffff:203.0.113.9")), T0 + 1_001);
    assert!(!gate.auth_allowed(ip(CLIENT), T0 + 1_999));
    assert!(gate.auth_allowed(ip(OTHER_PREFIX), T0 + 1_999));
    assert!(gate.auth_allowed(ip(CLIENT), T0 + 2_000));
}

#[test]
fn gate_auth_failure_default_is_five_per_prefix_for_ten_minutes_and_map_is_bounded() {
    use bp_relay_core::gate::DEFAULT_AUTH_FAILURE_RATE;
    let mut gate = new_gate();
    for _ in 0..5 {
        assert!(gate.auth_allowed(ip(CLIENT), T0));
        gate.record_auth_failure(ip(CLIENT), T0);
    }
    assert!(!gate.auth_allowed(ip("203.0.113.99"), T0 + 599_999));
    assert!(gate.auth_allowed(ip(CLIENT), T0 + DEFAULT_AUTH_FAILURE_RATE.window_ms));
    for index in 0..MAX_TRACKED_PREFIXES {
        gate.record_auth_failure(ip(&client(index)), T0 + 600_000);
    }
    assert!(!gate.auth_allowed(ip(OTHER_PREFIX), T0 + 600_001));
    assert!(gate.auth_allowed(ip(&client(0)), T0 + 600_001));
    assert!(gate.auth_allowed(ip(OTHER_PREFIX), T0 + 1_200_000));
}
