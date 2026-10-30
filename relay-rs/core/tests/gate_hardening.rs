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
    let (hold, effects) = hold_parts(gate.handle(&mut rng, &request));
    assert_eq!(hold.until_ms(), T0 + HOLD_MS);
    assert_eq!(effects.len(), 1);
    assert_eq!(gate.pending_ids(T0).len(), 1);
}

#[test]
fn gate_navigation_without_sec_fetch_dest_opens_an_approval() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(501);
    let (_, effects) = hold_parts(get(&mut gate, &mut rng, CLIENT, T0));
    assert_eq!(effects.len(), 1);
    assert_eq!(gate.pending_ids(T0).len(), 1);
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
        let (page, effects) = handled_parts(gate.handle(&mut rng, &request));
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
    let (page, effects) = handled_parts(gate.handle(&mut rng, &request));
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
    assert_eq!(gate.handle(&mut rng, &request), Verdict::Allow);
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
    let mut gate = new_gate();
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
