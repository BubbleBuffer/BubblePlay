//! Persistence: what is stored, reloading, pruning, and the 30-day lifetime.

mod gate_support;

use bp_relay_core::gate::{COOKIE_MAX_AGE_SECS, Gate, GateSnapshot, Verdict};
use gate_support::*;
use sha2::{Digest, Sha256};

const COOKIE_MS: u64 = COOKIE_MAX_AGE_SECS * 1000;
const DECIDER: &str = "198.51.100.200";

/// Run the approval for CLIENT and return the poll response and its effects.
/// The cookie is minted at `poll_at`.
fn approve_at(
    gate: &mut Gate,
    rng: &mut TestRng,
    poll_at: u64,
) -> (
    bp_relay_core::gate::Response,
    Vec<bp_relay_core::gate::GateEffect>,
) {
    let (page, effects) = wait_parts(get(gate, rng, CLIENT, T0));
    let token = poll_token(&page);
    let (approve, _) = bearer_tokens(&effects[0]);
    decide(gate, rng, &approve, "approve", DECIDER, T0 + 1_000);
    handled_parts(poll(gate, rng, &token, CLIENT, poll_at))
}

#[test]
fn gate_cookie_is_stored_only_as_its_sha256() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(300);
    let cookie = approve_client(&mut gate, &mut rng, CLIENT, T0);
    let value = cookie.trim_start_matches("__Host-gate=");
    let snapshot = gate.snapshot();
    assert_eq!(snapshot.records.len(), 1);
    let expected: [u8; 32] = Sha256::digest(value.as_bytes()).into();
    assert_eq!(snapshot.records[0].hash, expected);
    assert_ne!(value.as_bytes(), &snapshot.records[0].hash[..]);
}

#[test]
fn gate_persisted_snapshot_reloads_and_keeps_the_cookie() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(301);
    let (response, effects) = approve_at(&mut gate, &mut rng, T0 + 2_000);
    let cookie = set_cookie_value(&response).expect("cookie");
    let snapshot = snapshot_of(&effects);
    let mut reloaded = Gate::new(config(true), snapshot, T0 + 5_000).expect("valid config");
    let verdict = get_with_cookie(&mut reloaded, &mut rng, "/", &cookie, T0 + 6_000);
    assert_eq!(verdict, Verdict::Allow);
}

#[test]
fn gate_reload_drops_records_that_expired_before_the_reload() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(302);
    let (response, effects) = approve_at(&mut gate, &mut rng, T0 + 2_000);
    let cookie = set_cookie_value(&response).expect("cookie");
    let mut snapshot = snapshot_of(&effects);
    snapshot.records[0].expires_ms = T0 + 10_000;
    let mut reloaded = Gate::new(config(true), snapshot, T0 + 10_000).expect("valid config");
    let verdict = get_with_cookie(&mut reloaded, &mut rng, "/", &cookie, T0 + 10_000);
    assert!(matches!(verdict, Verdict::Wait(_, _)), "got {verdict:?}");
    assert!(reloaded.snapshot().records.is_empty());
}

#[test]
fn gate_snapshot_survives_repeated_reloads_unchanged() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(303);
    let (_, effects) = approve_at(&mut gate, &mut rng, T0 + 2_000);
    let snapshot = snapshot_of(&effects);
    let first = Gate::new(config(true), snapshot.clone(), T0 + 5_000).expect("valid config");
    let second = Gate::new(config(true), first.snapshot(), T0 + 6_000).expect("valid config");
    assert_eq!(second.snapshot(), snapshot);
}

#[test]
fn gate_tick_prunes_expired_records_and_emits_persist() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(304);
    let _ = approve_at(&mut gate, &mut rng, T0 + 2_000);
    let expires = T0 + 2_000 + COOKIE_MS;
    assert!(gate.tick(expires - 1).is_empty());
    let effects = gate.tick(expires);
    let snapshot = snapshot_of(&effects);
    assert!(snapshot.records.is_empty());
    assert!(gate.tick(expires + 1).is_empty());
}

#[test]
fn gate_tick_without_expiry_emits_nothing() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(305);
    let _ = approve_at(&mut gate, &mut rng, T0 + 2_000);
    assert!(gate.tick(T0 + 3_000).is_empty());
}

#[test]
fn gate_cookie_lasts_30_days_from_approval_with_the_fixed_attributes() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(306);
    let mint = T0 + 2_000;
    let (response, _) = approve_at(&mut gate, &mut rng, mint);
    let set = header(&response, "Set-Cookie").expect("Set-Cookie");
    assert!(set.contains("; Max-Age=2592000; Path=/; Secure; HttpOnly; SameSite=Lax"));
    let cookie = set_cookie_value(&response).expect("cookie");
    let valid = get_with_cookie(&mut gate, &mut rng, "/", &cookie, mint + COOKIE_MS - 1);
    assert_eq!(valid, Verdict::Allow);
    let expired = get_with_cookie(&mut gate, &mut rng, "/", &cookie, mint + COOKIE_MS);
    assert!(matches!(expired, Verdict::Wait(_, _)), "got {expired:?}");
}

#[test]
fn gate_cookie_expiry_is_not_extended_by_use() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(307);
    let mint = T0 + 2_000;
    let (response, _) = approve_at(&mut gate, &mut rng, mint);
    let cookie = set_cookie_value(&response).expect("cookie");
    let used = get_with_cookie(
        &mut gate,
        &mut rng,
        "/",
        &cookie,
        mint + COOKIE_MS - 86_400_000,
    );
    assert_eq!(used, Verdict::Allow);
    let after = get_with_cookie(&mut gate, &mut rng, "/", &cookie, mint + COOKIE_MS);
    assert!(matches!(after, Verdict::Wait(_, _)), "got {after:?}");
}

#[test]
fn gate_pending_approvals_are_not_persisted() {
    let mut gate = new_gate();
    let mut rng = TestRng::new(308);
    let (_, effects) = wait_parts(get(&mut gate, &mut rng, CLIENT, T0));
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, bp_relay_core::gate::GateEffect::PersistGate(_)))
    );
    assert!(gate.snapshot().records.is_empty());
}

#[test]
fn gate_a_fresh_gate_has_an_empty_snapshot() {
    let gate = Gate::new(config(true), GateSnapshot::default(), T0).expect("valid config");
    assert!(gate.snapshot().records.is_empty());
}
