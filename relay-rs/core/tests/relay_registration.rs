//! Host registration, the proof challenge, admin listing and deletion, the
//! upgrade gate, and the hello, challenge and idle timeouts. Ports the PORT
//! tests of routing_test.go, routing_boundaries_test.go and devicekeys_runtime_test.go.

mod relay_harness;

use bp_device_key::{DeviceId, Role as KeyRole, verify_relay_proof};
use bp_relay_core::{Action, AdminError, Route};
use relay_harness::*;
use serde_json::json;

fn refused() -> Option<(u16, String)> {
    Some((1008, "invalid hello".to_owned()))
}

#[test]
fn registration_needs_token_only_for_new_ids() {
    let mut h = Harness::new();
    let a = identity(1);
    let (id, actions) = h.host_hello(&a, None);
    assert_eq!(closed(&actions, id), refused());
    assert!(challenge_nonce(&actions, id).is_none());

    let (_host, _) = h.register_host(&a, Some(TOKEN));
    // A known PC re-registers without the token.
    let (id, actions) = h.host_hello(&a, None);
    assert!(challenge_nonce(&actions, id).is_some());
}

#[test]
fn known_id_reregisters_without_token() {
    let mut h = Harness::new();
    let a = identity(1);
    let (first, _) = h.register_host(&a, Some(TOKEN));
    h.close(first.id);
    let (second, actions) = h.register_host(&a, None);
    assert_eq!(second.pc, first.pc);
    assert!(sent_types(&actions, second.id).contains(&"hello-ok".to_owned()));
}

#[test]
fn persist_is_emitted_before_hello_ok() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, actions) = h.register_host(&a, Some(TOKEN));
    let persist = persisted_at(&actions).expect("PersistPcs emitted");
    let hello = actions
        .iter()
        .position(|action| {
            matches!(action, Action::SendText(to, text) if *to == host.id && text.contains("hello-ok"))
        })
        .expect("hello-ok sent");
    assert!(persist < hello, "persist must come before hello-ok");
    assert_eq!(persisted_pcs(&actions), Some(vec![host.pc.clone()]));
}

#[test]
fn proof_binds_origin_nonce_and_key() {
    let mut h = Harness::new();
    let a = identity(1);
    let other = identity(2);

    let (id, actions) = h.host_hello(&a, Some(TOKEN));
    let nonce = challenge_nonce(&actions, id).expect("challenge");
    let out = h.text(id, &proof_msg(&a, "https://evil.example.test", &nonce));
    assert_eq!(closed(&out, id), refused(), "wrong origin");

    let (id, _) = h.host_hello(&a, Some(TOKEN));
    let out = h.text(id, &proof_msg(&a, ORIGIN, &[0u8; 32]));
    assert_eq!(closed(&out, id), refused(), "wrong nonce");

    let (id, actions) = h.host_hello(&a, Some(TOKEN));
    let nonce = challenge_nonce(&actions, id).expect("challenge");
    let out = h.text(id, &proof_msg(&other, ORIGIN, &nonce));
    assert_eq!(closed(&out, id), refused(), "wrong key");

    let (id, actions) = h.host_hello(&a, Some(TOKEN));
    let nonce = challenge_nonce(&actions, id).expect("challenge");
    let out = h.text(id, &proof_msg(&a, ORIGIN, &nonce));
    assert!(sent_types(&out, id).contains(&"hello-ok".to_owned()));
}

#[test]
fn proof_is_single_use() {
    let mut h = Harness::new();
    let a = identity(1);
    let (id, actions) = h.host_hello(&a, Some(TOKEN));
    let nonce = challenge_nonce(&actions, id).expect("challenge");
    let out = h.text(id, &proof_msg(&a, "https://evil.example.test", &nonce));
    assert_eq!(closed(&out, id), refused());
    // The socket is gone: a correct answer now does nothing.
    let out = h.text(id, &proof_msg(&a, ORIGIN, &nonce));
    assert!(out.is_empty());
    assert!(h.relay.list_pcs().expect("listing").is_empty());
}

#[test]
fn hello_public_must_derive_the_id() {
    let mut h = Harness::new();
    let a = identity(1);
    let b = identity(2);
    let id = h.upgrade(req(Route::HostWs, Some(&a.id().to_string()), None, false));
    let out = h.text(
        id,
        &json!({
            "type": "hello",
            "role": "host",
            "id": a.id().to_string(),
            "public": b64(&b.public_key()),
            "token": TOKEN,
        }),
    );
    assert_eq!(closed(&out, id), refused());
}

#[test]
fn wrong_token_is_refused_even_for_a_known_id() {
    let mut h = Harness::new();
    let a = identity(1);
    h.register_host(&a, Some(TOKEN));
    let (id, actions) = h.host_hello(&a, Some("not-the-token"));
    assert_eq!(closed(&actions, id), refused());
}

#[test]
fn known_id_rejects_a_foreign_key() {
    let mut h = Harness::new();
    let a = identity(1);
    let b = identity(2);
    h.register_host(&a, Some(TOKEN));
    // The socket is on A's PC, but the hello proves key B.
    let id = h.upgrade(req(Route::HostWs, Some(&a.id().to_string()), None, false));
    let out = h.text(
        id,
        &json!({
            "type": "hello",
            "role": "host",
            "id": b.id().to_string(),
            "public": b64(&b.public_key()),
        }),
    );
    assert_eq!(closed(&out, id), refused());
}

#[test]
fn unhealthy_store_refuses_registration_and_listing() {
    let mut h = Harness::new();
    let a = identity(1);
    h.relay.set_store_healthy(false);
    let (id, actions) = h.host_hello(&a, Some(TOKEN));
    assert_eq!(closed(&actions, id), refused());
    assert!(matches!(h.relay.list_pcs(), Err(AdminError::Unavailable)));
    assert!(matches!(
        h.relay.delete_pc(&a.id().to_string()),
        Err(AdminError::Unavailable)
    ));
}

#[test]
fn hello_times_out_after_ten_seconds() {
    let mut h = Harness::new();
    let a = identity(1);
    let id = h.upgrade(req(Route::HostWs, Some(&a.id().to_string()), None, false));
    assert!(h.advance(9_999).is_empty());
    assert_eq!(
        closed(&h.advance(1), id),
        Some((1008, "timed out".to_owned()))
    );
}

#[test]
fn challenge_must_be_answered_within_ten_seconds() {
    let mut h = Harness::new();
    let a = identity(1);
    let (id, actions) = h.host_hello(&a, Some(TOKEN));
    assert!(challenge_nonce(&actions, id).is_some());
    assert!(h.advance(9_999).is_empty());
    assert_eq!(closed(&h.advance(1), id), refused());
}

#[test]
fn idle_signalling_socket_closes_after_ninety_seconds() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    assert!(h.advance(60_000).is_empty());
    let out = h.text(host.id, &json!({"type": "ping"}));
    assert_eq!(sent_types(&out, host.id), vec!["pong".to_owned()]);
    // The ping moved the deadline to 150 s after registration.
    assert!(h.advance(89_999).is_empty());
    assert_eq!(
        closed(&h.advance(1), host.id),
        Some((1001, "peer disconnected".to_owned()))
    );
}

#[test]
fn delete_persists_first_and_closes_only_that_pcs_hosts() {
    let mut h = Harness::new();
    let a = identity(1);
    let b = identity(2);
    let (host_a, _) = h.register_host(&a, Some(TOKEN));
    let (host_b, _) = h.register_host(&b, Some(TOKEN));

    let out = h.relay.delete_pc(&host_a.pc).expect("deleted");
    assert_eq!(persisted_at(&out), Some(0), "persist is the first action");
    assert_eq!(persisted_pcs(&out), Some(vec![host_b.pc.clone()]));
    assert_eq!(
        closed(&out, host_a.id),
        Some((1008, "peer removed".to_owned()))
    );
    assert_eq!(closed(&out, host_b.id), None);
}

#[test]
fn delete_rejects_a_non_device_id() {
    let mut h = Harness::new();
    assert!(matches!(
        h.relay.delete_pc("not-a-device-id"),
        Err(AdminError::NotFound)
    ));
}

#[test]
fn delete_of_an_unregistered_or_deleted_pc_is_not_found() {
    let mut h = Harness::new();
    let a = identity(1);
    let never = identity(2);
    assert!(matches!(
        h.relay.delete_pc(&never.id().to_string()),
        Err(AdminError::NotFound)
    ));
    h.register_host(&a, Some(TOKEN));
    h.relay.delete_pc(&a.id().to_string()).expect("deleted");
    assert!(matches!(
        h.relay.delete_pc(&a.id().to_string()),
        Err(AdminError::NotFound)
    ));
    assert_eq!(h.relay.list_pcs().expect("listing").len(), 0);
}

#[test]
fn deleted_pc_needs_the_token_again() {
    let mut h = Harness::new();
    let a = identity(1);
    h.register_host(&a, Some(TOKEN));
    h.relay.delete_pc(&a.id().to_string()).expect("deleted");
    let (id, actions) = h.host_hello(&a, None);
    assert_eq!(closed(&actions, id), refused());
}

#[test]
fn admin_listing_reports_online_state() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let listing = h.relay.list_pcs().expect("listing");
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].id, host.pc);
    assert_eq!(listing[0].public, b64(&a.public_key()));
    assert_eq!(listing[0].first_seen, START_MS);
    assert!(listing[0].online);

    h.close(host.id);
    let listing = h.relay.list_pcs().expect("listing");
    assert!(!listing[0].online);
}

#[test]
fn upgrade_refuses_bad_pc_epoch_and_unauthorised_viewers() {
    let mut h = Harness::new();
    let a = identity(1);
    h.register_host(&a, Some(TOKEN));
    let viewer = h.open_viewer(&a.id().to_string());

    assert!(
        h.try_upgrade(req(Route::HostWs, Some("not-a-pc"), None, false))
            .is_err()
    );
    assert!(
        h.try_upgrade(req(Route::HostWs, None, None, false))
            .is_err()
    );
    assert!(
        h.try_upgrade(req(Route::Ws, Some(&viewer.pc), None, false))
            .is_err()
    );
    assert!(
        h.try_upgrade(req(Route::MediaWs, Some(&viewer.pc), None, true))
            .is_err()
    );
    assert!(
        h.try_upgrade(req(Route::MediaWs, Some(&viewer.pc), Some("not-hex"), true))
            .is_err()
    );
    let stale = "00".repeat(16);
    assert!(
        h.try_upgrade(req(Route::MediaWs, Some(&viewer.pc), Some(&stale), true))
            .is_err(),
        "an epoch that is not the room's current epoch"
    );
    // A version probe may come without a PC.
    assert!(
        h.try_upgrade(req(Route::HostVersionWs, None, None, false))
            .is_ok()
    );
}

#[test]
fn unauthenticated_caps_preserve_live_hosts_and_pending_valid_proofs() {
    for distributed in [false, true] {
        let mut h = Harness::with(|c| c.max_connections = 1);
        let a = identity(1);
        let (host, _) = h.register_host(&a, Some(TOKEN));
        let (replacement, challenge) = h.host_hello(&a, None);
        let nonce = challenge_nonce(&challenge, replacement).expect("challenge");
        let attacks = if distributed { 15 } else { 3 };
        for n in 0..attacks {
            if distributed {
                h.client = format!("203.0.{n}.1").parse().unwrap();
            }
            h.upgrade(req(Route::HostVersionWs, None, None, false));
        }
        if distributed {
            h.client = "192.0.2.1".parse().unwrap();
        }
        assert!(
            h.try_upgrade(req(Route::HostVersionWs, None, None, false))
                .is_err()
        );
        assert_eq!(
            sent_types(&h.text(host.id, &json!({"type":"ping"})), host.id),
            vec!["pong"]
        );
        let out = h.text(replacement, &proof_msg(&a, ORIGIN, &nonce));
        assert!(closed(&out, replacement).is_none());
        hello_ok(&out, replacement);
        assert_eq!(
            sent_types(&h.text(replacement, &json!({"type":"ping"})), replacement),
            vec!["pong"]
        );
    }
}

#[test]
fn authenticated_capacity_is_checked_after_proof_without_displacing_a_host() {
    let mut h = Harness::with(|c| c.max_connections = 1);
    let (host, _) = h.register_host(&identity(1), Some(TOKEN));
    let b = identity(2);
    let (id, challenge) = h.host_hello(&b, Some(TOKEN));
    let nonce = challenge_nonce(&challenge, id).expect("pre-auth capacity remains available");
    let out = h.text(id, &proof_msg(&b, ORIGIN, &nonce));
    assert_eq!(closed(&out, id), refused());
    assert!(!h.relay.pcs().records.contains_key(&b.id().to_string()));
    assert_eq!(
        sent_types(&h.text(host.id, &json!({"type":"ping"})), host.id),
        vec!["pong"]
    );
}

#[test]
fn unauthenticated_prefix_cap_uses_ipv4_24_and_ipv6_56() {
    for clients in [
        [
            "198.51.100.1",
            "198.51.100.2",
            "198.51.100.3",
            "198.51.100.4",
            "198.51.100.255",
        ],
        [
            "2001:db8:1234:5600::1",
            "2001:db8:1234:5601::1",
            "2001:db8:1234:567f::1",
            "2001:db8:1234:56aa::1",
            "2001:db8:1234:56ff::1",
        ],
    ] {
        let mut h = Harness::new();
        for client in &clients[..4] {
            h.client = client.parse().unwrap();
            h.upgrade(req(Route::HostVersionWs, None, None, false));
        }
        h.client = clients[4].parse().unwrap();
        assert!(
            h.try_upgrade(req(Route::HostVersionWs, None, None, false))
                .is_err()
        );
        h.advance(10_000);
        assert!(
            h.try_upgrade(req(Route::HostVersionWs, None, None, false))
                .is_ok()
        );
    }
}

#[test]
fn upgrades_are_limited_to_thirty_per_minute_before_acceptance() {
    let mut h = Harness::new();
    for _ in 0..30 {
        let id = h.upgrade(req(Route::HostVersionWs, None, None, false));
        h.close(id);
    }
    assert!(
        h.try_upgrade(req(Route::HostVersionWs, None, None, false))
            .is_err()
    );
    h.advance(59_999);
    assert!(
        h.try_upgrade(req(Route::HostVersionWs, None, None, false))
            .is_err()
    );
    h.advance(1);
    assert!(
        h.try_upgrade(req(Route::HostVersionWs, None, None, false))
            .is_ok()
    );
}

#[test]
fn proof_deadline_is_enforced_even_before_tick() {
    let mut h = Harness::new();
    let a = identity(1);
    let (id, challenge) = h.host_hello(&a, Some(TOKEN));
    let nonce = challenge_nonce(&challenge, id).unwrap();
    h.now += 10_000;
    let out = h.text(id, &proof_msg(&a, ORIGIN, &nonce));
    assert_eq!(closed(&out, id), refused());
    assert!(h.relay.pcs().records.is_empty());
}

#[test]
fn standalone_token_failures_are_limited_without_blocking_registered_host_proofs() {
    let mut h = Harness::new();
    let a = identity(1);
    h.register_host(&a, Some(TOKEN));
    for _ in 0..5 {
        let (id, out) = h.host_hello(&a, Some("wrong"));
        assert_eq!(closed(&out, id), refused());
        h.close(id);
    }
    let (id, out) = h.host_hello(&a, Some(TOKEN));
    assert_eq!(closed(&out, id), refused());
    let (_, out) = h.register_host(&a, None);
    assert!(
        out.iter()
            .any(|a| matches!(a, Action::SendText(_, text) if text.contains("hello-ok")))
    );
    h.advance(600_000);
    let (id, out) = h.host_hello(&a, Some(TOKEN));
    assert!(challenge_nonce(&out, id).is_some());
}

#[test]
fn hello_bad_token_consumes_the_same_gate_budget_as_http_failures() {
    use bp_relay_core::gate::{
        DEFAULT_AUTH_FAILURE_RATE, DEFAULT_NTFY_RATE, Gate, GateConfig, GateSnapshot,
    };
    let mut h = Harness::new();
    let mut gate = Gate::new(
        GateConfig {
            public_origin: ORIGIN.to_owned(),
            approval_ttl_secs: 90,
            ntfy: None,
            ntfy_rate: DEFAULT_NTFY_RATE,
            auth_failure_rate: DEFAULT_AUTH_FAILURE_RATE,
        },
        GateSnapshot::default(),
        h.now,
    )
    .unwrap();
    // Four HTTP authentication failures leave exactly one WebSocket failure.
    for _ in 0..4 {
        gate.record_auth_failure(h.client, h.now);
    }
    let id = h.upgrade(req(Route::HostVersionWs, None, None, false));
    let out = h.relay.on_text_with_gate(
        id,
        r#"{"type":"hello","role":"version-probe","token":"wrong"}"#,
        h.now,
        &mut h.rng,
        &mut gate,
    );
    assert_eq!(closed(&out, id), refused());
    assert!(!gate.auth_allowed(h.client, h.now));
    let id = h.upgrade(req(Route::HostVersionWs, None, None, false));
    let out = h.relay.on_text_with_gate(
        id,
        &json!({"type":"hello","role":"version-probe","token":TOKEN}).to_string(),
        h.now,
        &mut h.rng,
        &mut gate,
    );
    assert_eq!(closed(&out, id), refused());
    h.client = "203.0.113.1".parse().unwrap();
    let id = h.upgrade(req(Route::HostVersionWs, None, None, false));
    let out = h.relay.on_text_with_gate(
        id,
        &json!({"type":"hello","role":"version-probe","token":TOKEN}).to_string(),
        h.now,
        &mut h.rng,
        &mut gate,
    );
    hello_ok(&out, id);
}

#[test]
fn shutdown_during_upgrade_is_quiet() {
    let mut h = Harness::new();
    let a = identity(1);
    let id = h.upgrade(req(Route::HostWs, Some(&a.id().to_string()), None, false));
    assert!(h.close(id).is_empty());
    assert!(h.advance(1_000_000).is_empty());
}

#[test]
fn device_key_runtime_contract() {
    let id = identity(9);
    let pc = id.id().to_string();
    let public = id.public_key();
    assert_eq!(DeviceId::parse(&pc).expect("parses").to_string(), pc);
    assert_eq!(DeviceId::from_public_key(&public).to_string(), pc);

    let nonce = [5u8; 32];
    let sig = id.sign_relay_proof(KeyRole::Host, ORIGIN, &nonce);
    let ok = verify_relay_proof(
        KeyRole::Host,
        ORIGIN,
        &nonce[..],
        &pc,
        &public[..],
        &sig[..],
    );
    assert!(ok.is_ok());
    let wrong_role = verify_relay_proof(
        KeyRole::Viewer,
        ORIGIN,
        &nonce[..],
        &pc,
        &public[..],
        &sig[..],
    );
    assert!(wrong_role.is_err());
    let wrong_origin = verify_relay_proof(
        KeyRole::Host,
        "https://other.example.test",
        &nonce[..],
        &pc,
        &public[..],
        &sig[..],
    );
    assert!(wrong_origin.is_err());
    let wrong_nonce = verify_relay_proof(
        KeyRole::Host,
        ORIGIN,
        &[6u8; 32][..],
        &pc,
        &public[..],
        &sig[..],
    );
    assert!(wrong_nonce.is_err());
}
