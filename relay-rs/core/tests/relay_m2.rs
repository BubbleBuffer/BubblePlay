//! M2 core tests (relay-rs M2 sections 1 to 4): the signed viewer hello, host
//! admission, pairing windows and their limits, pairing-mode forwarding and the
//! signalling allowlist. Every test name starts with `m2_`.

mod relay_harness;

use std::net::{IpAddr, Ipv4Addr};

use bp_device_key::DeviceIdentity;
use bp_relay_core::limits::{ADMIT_VIEWERS_MAX, AUTH_MAX, CTL_DATA_MAX};
use bp_relay_core::{Action, ConnId, Invalid, Relay, Route, Side, validate_signal_text};
use relay_harness::*;
use serde_json::{Value, json};

const EPOCH: &str = "0123456789abcdef0123456789abcdef";
const PAIR_VALUE: [u8; 32] = [42; 32];
const WRONG_VALUE: [u8; 32] = [43; 32];
const TWO_TO_53_MINUS_1: u64 = 9_007_199_254_740_991;

// ---------------------------------------------------------------- helpers

/// The PC's host, registered with the test token. It admits the viewer identity.
fn host_up(h: &mut Harness) -> Host {
    h.register_host(&identity(1), Some(TOKEN)).0
}

/// A viewer identity that no test host admits.
fn stranger() -> DeviceIdentity {
    identity(201)
}

/// The admitted viewer's canonical ID, as the host lists it.
fn listed_viewer() -> String {
    viewer_identity().id().to_string()
}

/// A canonical device ID that differs for each `n`.
fn id_of(n: u16) -> String {
    let mut seed = [0u8; 32];
    seed[0] = n as u8;
    seed[1] = (n >> 8) as u8;
    DeviceIdentity::from_seed(&seed).id().to_string()
}

/// True when the socket was refused: close 1008 with an empty reason.
fn refused(actions: &[Action], id: ConnId) -> bool {
    closed(actions, id) == Some((1008, String::new()))
}

/// The socket an action is sent to, if it has one. `PersistPcs` has none.
fn target(action: &Action) -> Option<ConnId> {
    match action {
        Action::SendText(to, _) | Action::SendBinary(to, _) | Action::Close(to, _, _) => Some(*to),
        Action::PersistPcs(_) | Action::Snapshot(_, _) => None,
    }
}

/// One frame a refused socket can receive, in the form the contract checks.
#[derive(Debug, PartialEq)]
enum Frame {
    Challenge,
    Close(u16, String),
    Other(String),
}

fn is_type(text: &str, kind: &str) -> bool {
    serde_json::from_str::<Value>(text).is_ok_and(|v| v["type"] == kind)
}

/// Checks the refusal contract for `id`, a socket refused after `batches` of
/// relay actions (its hello, then its proof, and so on). Its own frames must
/// be exactly `[register-challenge, close 1008 ""]`, or only the close when the
/// refusal came before the challenge (`challenged` is false). No action may
/// target `host`, the PC's host socket.
fn assert_refusal_contract(batches: &[Vec<Action>], id: ConnId, host: ConnId, challenged: bool) {
    let frames: Vec<Frame> = batches
        .iter()
        .flatten()
        .filter(|action| target(action) == Some(id))
        .map(|action| match action {
            Action::SendText(_, text) if is_type(text, "register-challenge") => Frame::Challenge,
            Action::Close(_, code, reason) => Frame::Close(*code, reason.clone()),
            other => Frame::Other(format!("{other:?}")),
        })
        .collect();
    let mut expected = Vec::new();
    if challenged {
        expected.push(Frame::Challenge);
    }
    expected.push(Frame::Close(1008, String::new()));
    assert_eq!(frames, expected, "frames sent to the refused socket");
    let to_host = batches
        .iter()
        .flatten()
        .filter(|action| target(action) == Some(host))
        .count();
    assert_eq!(to_host, 0, "no action may target the host socket");
}

fn listed_admitted(h: &Harness, pc: &str) -> usize {
    h.relay
        .list_pcs()
        .expect("list_pcs")
        .iter()
        .find(|p| p.id == pc)
        .expect("pc listed")
        .admitted
}

fn send_pair_open(h: &mut Harness, host: ConnId, admit: &[u8; 32]) -> Vec<Action> {
    h.text(host, &json!({"type": "pair-open", "admit": b64(admit)}))
}

fn send_admit(h: &mut Harness, host: ConnId, viewers: &[String], remote: bool) -> Vec<Action> {
    h.text(
        host,
        &json!({"type": "admit", "viewers": viewers, "remotePairing": remote}),
    )
}

/// Advances the clock in steps under 90 s, pinging the host so it stays
/// connected.
fn pass_time(h: &mut Harness, host: ConnId, mut ms: u64) {
    while ms > 0 {
        let step = ms.min(60_000);
        h.advance(step);
        h.text(host, &json!({"type": "ping"}));
        ms -= step;
    }
}

/// Opens a viewer socket on `pc`, sends `hello`, and returns the socket with
/// the register-challenge nonce.
fn viewer_start(h: &mut Harness, pc: &str, hello: &Value) -> (ConnId, [u8; 32]) {
    let id = h.upgrade(req(Route::Ws, Some(pc), None, true));
    let actions = h.text(id, hello);
    let nonce = challenge_nonce(&actions, id).expect("register-challenge sent");
    (id, nonce)
}

/// Reconnects `who` as the host of its PC, without a token (the PC is known).
fn reregister(h: &mut Harness, who: &DeviceIdentity) -> (ConnId, Vec<Action>) {
    let (id, actions) = h.host_hello(who, None);
    let nonce = challenge_nonce(&actions, id).expect("register-challenge sent");
    let actions = h.text(id, &proof_msg(who, ORIGIN, &nonce));
    (id, actions)
}

/// Runs the viewer handshake for `who` on `pc`: the signed hello (with `pair`
/// when given), then the proof for the challenge. Returns the socket and the
/// batch of actions from each step.
fn viewer_handshake(
    h: &mut Harness,
    who: &DeviceIdentity,
    pc: &str,
    pair: Option<&[u8; 32]>,
) -> (ConnId, Vec<Vec<Action>>) {
    let id = h.upgrade(req(Route::Ws, Some(pc), None, true));
    let mut hello = viewer_hello_msg(who, false);
    if let Some(pair) = pair {
        hello["pair"] = json!(b64(pair));
    }
    let hello_actions = h.text(id, &hello);
    let nonce = challenge_nonce(&hello_actions, id).expect("register-challenge sent");
    let proof_actions = h.text(id, &viewer_proof_msg(who, ORIGIN, &nonce));
    (id, vec![hello_actions, proof_actions])
}

/// A valid viewer hello is sent, then `proof` is answered. The socket must be
/// refused under the refusal contract, after the challenge.
fn proof_is_refused(proof: impl FnOnce(&[u8; 32]) -> Value) {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let id = h.upgrade(req(Route::Ws, Some(&host.pc), None, true));
    let hello_actions = h.text(id, &viewer_hello_msg(&viewer_identity(), false));
    let nonce = challenge_nonce(&hello_actions, id).expect("register-challenge sent");
    let proof_actions = h.text(id, &proof(&nonce));
    assert_refusal_contract(&[hello_actions, proof_actions], id, host.id, true);
}

/// A viewer hello changed by `tweak` must be refused before any challenge,
/// under the refusal contract.
fn hello_is_refused(tweak: impl FnOnce(&mut Value)) {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let mut hello = viewer_hello_msg(&viewer_identity(), false);
    tweak(&mut hello);
    let id = h.upgrade(req(Route::Ws, Some(&host.pc), None, true));
    let actions = h.text(id, &hello);
    assert!(
        challenge_nonce(&actions, id).is_none(),
        "a bad hello gets no challenge"
    );
    assert_refusal_contract(&[actions], id, host.id, false);
}

fn auth_hello_msg(seed: u8) -> Value {
    json!({
        "type": "auth-hello",
        "id": identity(seed).id().to_string(),
        "public": b64(&[seed; 32]),
        "nonce": b64(&[9u8; 32]),
        "ephemeral": b64(&[8u8; 32]),
        "caps": ["input"],
    })
}

fn auth_host_msg() -> Value {
    json!({
        "type": "auth-host",
        "id": identity(1).id().to_string(),
        "public": b64(&[1u8; 32]),
        "nonce": b64(&[9u8; 32]),
        "ephemeral": b64(&[8u8; 32]),
        "signature": b64(&[5u8; 64]),
    })
}

fn ctl_msg(seq: u64, data: &str) -> Value {
    json!({"type": "ctl", "kind": "input", "seq": seq, "data": data})
}

/// Accepted by the signal allowlist for `side`.
fn accepted(side: Side, value: &Value) {
    let text = value.to_string();
    assert!(
        validate_signal_text(side, &text, EPOCH).is_ok(),
        "should be accepted: {text}"
    );
}

/// Dropped by the signal allowlist for `side` (valid JSON, not allowed there).
fn dropped(side: Side, value: &Value) {
    let text = value.to_string();
    assert!(
        matches!(validate_signal_text(side, &text, EPOCH), Err(Invalid::Drop)),
        "should be dropped: {text}"
    );
}

// ----------------------------------------------- viewer proof and admission

#[test]
fn m2_listed_viewer_is_admitted_after_the_signed_proof() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let id = h.upgrade(req(Route::Ws, Some(&host.pc), None, true));
    let actions = h.text(id, &viewer_hello_msg(&viewer_identity(), false));
    // Nothing but the challenge is sent before the proof.
    assert_eq!(sent_types(&actions, id), vec!["register-challenge"]);
    let nonce = challenge_nonce(&actions, id).expect("challenge");
    let actions = h.text(id, &viewer_proof_msg(&viewer_identity(), ORIGIN, &nonce));
    let ok = hello_ok(&actions, id);
    assert!(ok.get("ticket").is_some());
    assert!(closed(&actions, id).is_none());
}

#[test]
fn m2_wrong_signature_closes_1008_with_empty_reason() {
    proof_is_refused(|nonce| viewer_proof_msg(&stranger(), ORIGIN, nonce));
}

#[test]
fn m2_host_role_signature_is_refused() {
    proof_is_refused(|nonce| proof_msg(&viewer_identity(), ORIGIN, nonce));
}

#[test]
fn m2_proof_for_another_origin_is_refused() {
    proof_is_refused(|nonce| {
        viewer_proof_msg(&viewer_identity(), "https://other.example.test", nonce)
    });
}

#[test]
fn m2_proof_with_an_extra_key_is_refused() {
    proof_is_refused(|nonce| {
        let mut proof = viewer_proof_msg(&viewer_identity(), ORIGIN, nonce);
        proof["extra"] = json!(1);
        proof
    });
}

#[test]
fn m2_proof_without_a_signature_is_refused() {
    proof_is_refused(|_| json!({"type": "register-proof"}));
}

#[test]
fn m2_hello_with_a_public_key_for_another_id_is_refused() {
    hello_is_refused(|hello| hello["public"] = json!(b64(&stranger().public_key())));
}

#[test]
fn m2_hello_with_a_non_canonical_id_is_refused() {
    hello_is_refused(|hello| {
        hello["id"] = json!(viewer_identity().id().to_string().to_lowercase());
    });
}

#[test]
fn m2_hello_without_a_public_key_is_refused() {
    hello_is_refused(|hello| {
        hello.as_object_mut().expect("object").remove("public");
    });
}

#[test]
fn m2_hello_with_an_unknown_key_is_refused() {
    hello_is_refused(|hello| hello["extra"] = json!(true));
}

#[test]
fn m2_hello_with_a_non_boolean_turn_is_refused() {
    hello_is_refused(|hello| hello["turn"] = json!("yes"));
}

#[test]
fn m2_hello_with_a_pair_that_is_not_32_bytes_is_refused() {
    hello_is_refused(|hello| hello["pair"] = json!(b64(&[1u8; 31])));
}

#[test]
fn m2_hello_with_the_host_role_is_refused() {
    hello_is_refused(|hello| hello["role"] = json!("host"));
}

#[test]
fn m2_unlisted_viewer_is_refused_with_empty_reason() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let (id, batches) = viewer_handshake(&mut h, &stranger(), &host.pc, None);
    assert_refusal_contract(&batches, id, host.id, true);
}

#[test]
fn m2_challenge_expires_after_30_seconds() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let (id, _) = viewer_start(
        &mut h,
        &host.pc,
        &viewer_hello_msg(&viewer_identity(), false),
    );
    let actions = h.advance(30_001);
    assert!(refused(&actions, id));
}

#[test]
fn m2_proof_at_29_seconds_is_accepted() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let (id, nonce) = viewer_start(
        &mut h,
        &host.pc,
        &viewer_hello_msg(&viewer_identity(), false),
    );
    h.advance(29_000);
    let actions = h.text(id, &viewer_proof_msg(&viewer_identity(), ORIGIN, &nonce));
    assert!(hello_ok(&actions, id).get("ticket").is_some());
}

// ------------------------------------------- pre-admission and idle timeouts

#[test]
fn m2_socket_that_never_sends_hello_is_closed_1008_with_empty_reason() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let id = h.upgrade(req(Route::Ws, Some(&host.pc), None, true));
    let out = h.advance(10_000);
    assert_eq!(out, vec![Action::Close(id, 1008, String::new())]);
}

#[test]
fn m2_hello_without_a_challenge_answer_is_closed_1008_with_empty_reason() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let id = h.upgrade(req(Route::Ws, Some(&host.pc), None, true));
    let hello_actions = h.text(id, &viewer_hello_msg(&viewer_identity(), false));
    assert!(challenge_nonce(&hello_actions, id).is_some());
    let timeout_actions = h.advance(30_000);
    assert_eq!(
        timeout_actions,
        vec![Action::Close(id, 1008, String::new())]
    );
    assert_refusal_contract(&[hello_actions, timeout_actions], id, host.id, true);
}

#[test]
fn m2_host_challenge_timeout_keeps_the_invalid_hello_reason() {
    let mut h = Harness::new();
    let (id, actions) = h.host_hello(&identity(1), Some(TOKEN));
    assert!(challenge_nonce(&actions, id).is_some());
    let out = h.advance(30_000);
    assert_eq!(
        out,
        vec![Action::Close(id, 1008, "invalid hello".to_owned())]
    );
}

#[test]
fn m2_idle_open_viewer_is_closed_1001_peer_disconnected() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let (viewer, _) = h.viewer_hello(&host.pc, false);
    // The host pings at 60 s so only the viewer goes idle at 90 s.
    pass_time(&mut h, host.id, 60_000);
    let out = h.advance(30_000);
    assert_eq!(
        closes(&out, viewer.id),
        vec![(1001, "peer disconnected".to_owned())]
    );
    assert!(closes(&out, host.id).is_empty());
}

#[test]
fn m2_refused_viewers_leave_the_admitted_viewer_and_the_room_epoch_alone() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    let (viewer, _) = h.viewer_hello(&pc, false);
    let mut seen = Vec::new();

    // An unlisted viewer, with no pairing window open.
    let (unlisted, batches) = viewer_handshake(&mut h, &stranger(), &pc, None);
    assert_refusal_contract(&batches, unlisted, host.id, true);
    seen.extend(batches.into_iter().flatten());
    h.close(unlisted);

    // A wrong-pair viewer, with a pairing window open.
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    let (wrong, batches) = viewer_handshake(&mut h, &identity(202), &pc, Some(&WRONG_VALUE));
    assert_refusal_contract(&batches, wrong, host.id, true);
    seen.extend(batches.into_iter().flatten());
    h.close(wrong);

    // The admitted viewer received nothing, is still connected, and the room
    // epoch is the one it was given: a media upgrade with it is still accepted.
    assert!(seen.iter().all(|action| target(action) != Some(viewer.id)));
    let pong = h.text(viewer.id, &json!({"type": "ping"}));
    assert_eq!(sent_types(&pong, viewer.id), vec!["pong".to_owned()]);
    assert!(
        h.try_upgrade(req(Route::MediaWs, Some(&pc), Some(&viewer.epoch), true))
            .is_ok()
    );
}

// ------------------------------------------------------------- host admit

#[test]
fn m2_admit_replaces_the_list_and_list_pcs_counts_it() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    assert_eq!(listed_admitted(&h, &pc), 1);
    send_admit(&mut h, host.id, &[listed_viewer(), id_of(1)], false);
    assert_eq!(listed_admitted(&h, &pc), 2);
}

#[test]
fn m2_admit_with_the_same_list_is_not_persisted() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let actions = send_admit(&mut h, host.id, &[listed_viewer()], false);
    assert!(persisted_at(&actions).is_none());
}

#[test]
fn m2_admit_that_changes_the_list_is_persisted() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let actions = send_admit(&mut h, host.id, &[listed_viewer(), id_of(1)], false);
    assert!(persisted_at(&actions).is_some());
}

#[test]
fn m2_remote_pairing_change_is_persisted() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    let actions = send_admit(&mut h, host.id, &[listed_viewer()], true);
    assert!(persisted_at(&actions).is_some());
    assert!(h.relay.pcs().records[&pc].remote_pairing);
}

#[test]
fn m2_admit_is_consumed_and_never_forwarded_to_the_viewer() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let (viewer, _) = h.viewer_hello(&host.pc, false);
    let actions = send_admit(&mut h, host.id, &[listed_viewer()], false);
    assert!(sent_text(&actions, viewer.id).is_empty());
}

#[test]
fn m2_admit_with_257_viewers_is_dropped() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    let all: Vec<String> = (0..=ADMIT_VIEWERS_MAX as u16).map(id_of).collect();
    let actions = send_admit(&mut h, host.id, &all, false);
    assert!(actions.is_empty());
    assert_eq!(listed_admitted(&h, &pc), 1);
}

#[test]
fn m2_admit_with_256_viewers_is_accepted() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    let all: Vec<String> = (0..ADMIT_VIEWERS_MAX as u16).map(id_of).collect();
    send_admit(&mut h, host.id, &all, false);
    assert_eq!(listed_admitted(&h, &pc), 256);
}

#[test]
fn m2_admit_with_a_duplicate_id_is_dropped() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    let actions = send_admit(&mut h, host.id, &[listed_viewer(), listed_viewer()], false);
    assert!(actions.is_empty());
    assert_eq!(listed_admitted(&h, &pc), 1);
}

#[test]
fn m2_admit_with_a_non_canonical_id_is_dropped() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    let actions = send_admit(&mut h, host.id, &[id_of(1).to_lowercase()], false);
    assert!(actions.is_empty());
    assert_eq!(listed_admitted(&h, &pc), 1);
}

#[test]
fn m2_delisted_viewer_is_closed_with_empty_reason() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let (viewer, _) = h.viewer_hello(&host.pc, false);
    let actions = send_admit(&mut h, host.id, &[id_of(1)], false);
    assert!(refused(&actions, viewer.id));
}

#[test]
fn m2_listed_viewer_stays_connected_when_the_list_repeats() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let (viewer, _) = h.viewer_hello(&host.pc, false);
    let actions = send_admit(&mut h, host.id, &[listed_viewer()], false);
    assert!(closed(&actions, viewer.id).is_none());
}

#[test]
fn m2_pairing_viewer_is_not_closed_by_the_list() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    let (p, _) = h.viewer_join(&stranger(), &host.pc, false, Some(&PAIR_VALUE));
    let actions = send_admit(&mut h, host.id, &[], false);
    assert!(closed(&actions, p).is_none());
}

#[test]
fn m2_admitted_list_survives_a_restart() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    send_admit(&mut h, host.id, &[listed_viewer(), id_of(1)], false);
    // A restart rebuilds the relay from the saved table.
    let restarted = Relay::new(h.relay.config().clone(), h.relay.pcs().clone());
    let listing = restarted.list_pcs().expect("list_pcs");
    assert_eq!(
        listing
            .iter()
            .find(|p| p.id == pc)
            .expect("listed")
            .admitted,
        2
    );
}

#[test]
fn m2_reregistration_keeps_the_admitted_list() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    send_admit(&mut h, host.id, &[listed_viewer(), id_of(1)], false);
    h.close(host.id);
    let (id, actions) = reregister(&mut h, &identity(1));
    hello_ok(&actions, id);
    assert_eq!(h.relay.pcs().records[&pc].admitted.len(), 2);
}

// ---------------------------------------------------------------- pairing

#[test]
fn m2_pair_open_is_consumed_and_forwards_nothing() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let actions = send_pair_open(&mut h, host.id, &PAIR_VALUE);
    assert!(
        actions.iter().all(|a| matches!(a, Action::Snapshot(..))),
        "{actions:?}"
    );
}

#[test]
fn m2_matching_pair_admits_in_pairing_mode_without_a_ticket() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    let (id, actions) = h.viewer_join(&stranger(), &host.pc, false, Some(&PAIR_VALUE));
    let ok = hello_ok(&actions, id);
    assert_eq!(ok["pairing"], json!(true));
    assert!(ok.get("ticket").is_none());
}

/// The host gets one peer-joined for the admission in `actions`: the viewer's
/// epoch, the pairing flag, and queued before the viewer's hello-ok.
fn assert_peer_joined(actions: &[Action], host: ConnId, viewer: ConnId, pairing: bool) {
    let ok = hello_ok(actions, viewer);
    let to_host = sent_json(actions, host);
    assert_eq!(to_host.len(), 1, "{to_host:?}");
    assert_eq!(
        to_host[0],
        json!({"type": "peer-joined", "mediaEpoch": ok["mediaEpoch"], "pairing": pairing})
    );
    let host_at = actions
        .iter()
        .position(|a| matches!(a, Action::SendText(to, _) if *to == host));
    let viewer_at = actions
        .iter()
        .position(|a| matches!(a, Action::SendText(to, _) if *to == viewer));
    let (Some(host_at), Some(viewer_at)) = (host_at, viewer_at) else {
        panic!("{actions:?}");
    };
    assert!(host_at < viewer_at, "the host hears first: {actions:?}");
}

#[test]
fn m2_admitted_viewer_is_announced_to_the_host_before_its_hello_ok() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let (viewer, actions) = h.viewer_hello(&host.pc, false);
    assert_peer_joined(&actions, host.id, viewer.id, false);
}

#[test]
fn m2_pairing_viewer_is_announced_to_the_host_with_pairing_true() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    let (id, actions) = h.viewer_join(&stranger(), &host.pc, false, Some(&PAIR_VALUE));
    assert_peer_joined(&actions, host.id, id, true);
}

#[test]
fn m2_pairing_admission_does_not_add_the_viewer_to_the_list() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    h.viewer_join(&stranger(), &pc, false, Some(&PAIR_VALUE));
    assert_eq!(listed_admitted(&h, &pc), 1);
}

#[test]
fn m2_pair_window_admits_one_viewer_only() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    let (first, actions) = h.viewer_join(&stranger(), &host.pc, false, Some(&PAIR_VALUE));
    hello_ok(&actions, first);
    let (second, batches) = viewer_handshake(&mut h, &identity(202), &host.pc, Some(&PAIR_VALUE));
    assert_refusal_contract(&batches, second, host.id, true);
}

#[test]
fn m2_wrong_pair_value_is_refused() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    let (id, batches) = viewer_handshake(&mut h, &stranger(), &host.pc, Some(&WRONG_VALUE));
    assert_refusal_contract(&batches, id, host.id, true);
}

#[test]
fn m2_pair_window_is_open_just_before_300_seconds() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    pass_time(&mut h, host.id, 299_000);
    let (id, actions) = h.viewer_join(&stranger(), &host.pc, false, Some(&PAIR_VALUE));
    hello_ok(&actions, id);
}

#[test]
fn m2_pair_window_expires_after_300_seconds() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    pass_time(&mut h, host.id, 300_000);
    let (id, batches) = viewer_handshake(&mut h, &stranger(), &host.pc, Some(&PAIR_VALUE));
    assert_refusal_contract(&batches, id, host.id, true);
}

#[test]
fn m2_pair_close_removes_the_window() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    let actions = h.text(host.id, &json!({"type": "pair-close"}));
    assert!(
        actions.iter().all(|a| matches!(a, Action::Snapshot(..))),
        "{actions:?}"
    );
    let (id, batches) = viewer_handshake(&mut h, &stranger(), &host.pc, Some(&PAIR_VALUE));
    assert_refusal_contract(&batches, id, host.id, true);
}

#[test]
fn m2_host_disconnect_drops_the_window() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    h.close(host.id);
    let (new_host, _) = reregister(&mut h, &identity(1));
    let (id, batches) = viewer_handshake(&mut h, &stranger(), &pc, Some(&PAIR_VALUE));
    assert_refusal_contract(&batches, id, new_host, true);
}

#[test]
fn m2_five_wrong_tries_block_the_prefix_even_with_the_right_value() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    for _ in 0..5 {
        let (id, batches) = viewer_handshake(&mut h, &stranger(), &pc, Some(&WRONG_VALUE));
        assert_refusal_contract(&batches, id, host.id, true);
        h.close(id);
    }
    let (id, batches) = viewer_handshake(&mut h, &stranger(), &pc, Some(&PAIR_VALUE));
    assert_refusal_contract(&batches, id, host.id, true);
    h.close(id);
    // The window itself is still open for another prefix.
    h.client = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
    let (id, actions) = h.viewer_join(&stranger(), &pc, false, Some(&PAIR_VALUE));
    hello_ok(&actions, id);
}

#[test]
fn m2_prefix_failures_reset_after_the_window() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    for _ in 0..5 {
        let (id, _) = h.viewer_join(&stranger(), &pc, false, Some(&WRONG_VALUE));
        h.close(id);
    }
    // The failure count began at t0. Reopen the window at t0 + 200 s and
    // check at t0 + 300 s, when the count has run out.
    pass_time(&mut h, host.id, 200_000);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    pass_time(&mut h, host.id, 100_000);
    let (id, actions) = h.viewer_join(&stranger(), &pc, false, Some(&PAIR_VALUE));
    hello_ok(&actions, id);
}

#[test]
fn m2_twenty_failures_for_one_window_send_pair_closed_to_the_host() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    // Five failures per prefix keeps each prefix under its own limit.
    for i in 0..20u8 {
        h.client = IpAddr::V4(Ipv4Addr::new(10, 0, i / 5, 1));
        let (id, actions) = h.viewer_join(&stranger(), &pc, false, Some(&WRONG_VALUE));
        assert!(refused(&actions, id));
        let told_host = sent_types(&actions, host.id)
            .iter()
            .any(|t| t.as_str() == "pair-closed");
        assert_eq!(
            told_host,
            i == 19,
            "pair-closed only on the twentieth failure"
        );
        h.close(id);
    }
    h.client = IpAddr::V4(Ipv4Addr::new(10, 0, 9, 1));
    let (id, actions) = h.viewer_join(&stranger(), &pc, false, Some(&PAIR_VALUE));
    assert!(refused(&actions, id));
}

#[test]
fn m2_full_prefix_map_refuses_new_prefixes_and_keeps_tracked_ones() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let pc = host.pc.clone();
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    // 4096 distinct prefixes, one failure each. The window is reopened every
    // 19 attempts so the twentieth failure never drops it.
    for i in 0..4096u16 {
        if i.is_multiple_of(19) {
            send_pair_open(&mut h, host.id, &PAIR_VALUE);
        }
        h.client = IpAddr::V4(Ipv4Addr::new(10, (i >> 8) as u8, (i & 0xff) as u8, 1));
        let (id, batches) = viewer_handshake(&mut h, &stranger(), &pc, Some(&WRONG_VALUE));
        assert_refusal_contract(&batches, id, host.id, true);
        h.close(id);
    }
    h.client = IpAddr::V4(Ipv4Addr::new(10, 200, 0, 1));
    let (id, batches) = viewer_handshake(&mut h, &stranger(), &pc, Some(&PAIR_VALUE));
    assert_refusal_contract(&batches, id, host.id, true);
    h.close(id);
    // 10.0.0.x is already tracked (one failure), so it still reaches the window.
    h.client = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
    let (id, actions) = h.viewer_join(&stranger(), &pc, false, Some(&PAIR_VALUE));
    hello_ok(&actions, id);
}

#[test]
fn m2_listed_viewer_takes_the_slot_from_a_pairing_viewer() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    let (p, actions) = h.viewer_join(&stranger(), &host.pc, false, Some(&PAIR_VALUE));
    hello_ok(&actions, p);
    let (listed, actions) = h.viewer_hello(&host.pc, false);
    assert_eq!(
        closed(&actions, p),
        Some((1008, "peer replaced".to_owned()))
    );
    assert!(!listed.epoch.is_empty());
}

#[test]
fn m2_pairing_forwarding_allows_only_the_auth_and_pair_proof_messages() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    let (p, _) = h.viewer_join(&stranger(), &host.pc, false, Some(&PAIR_VALUE));

    let actions = h.text(p, &auth_hello_msg(201));
    assert_eq!(sent_types(&actions, host.id), vec!["auth-hello"]);
    let actions = h.text(p, &json!({"type": "offer", "sdp": "v=0"}));
    assert!(sent_text(&actions, host.id).is_empty());
    let actions = h.text(host.id, &ctl_msg(0, ""));
    assert!(sent_text(&actions, p).is_empty());
    let actions = h.text(host.id, &auth_host_msg());
    assert_eq!(sent_types(&actions, p), vec!["auth-host"]);
}

#[test]
fn m2_pairing_viewer_bye_is_dropped() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    let (p, _) = h.viewer_join(&stranger(), &host.pc, false, Some(&PAIR_VALUE));
    let actions = h.text(p, &json!({"type": "bye"}));
    assert!(actions.is_empty());
    assert!(closed(&actions, p).is_none());
}

#[test]
fn m2_pair_done_and_pair_denied_reach_the_pairing_viewer() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    let (p, _) = h.viewer_join(&stranger(), &host.pc, false, Some(&PAIR_VALUE));
    let actions = h.text(host.id, &json!({"type": "pair-done"}));
    assert_eq!(sent_types(&actions, p), vec!["pair-done"]);
    let actions = h.text(host.id, &json!({"type": "pair-denied"}));
    assert_eq!(sent_types(&actions, p), vec!["pair-denied"]);
}

#[test]
fn m2_pair_proof_is_forwarded_both_ways_in_pairing_mode() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    let (p, _) = h.viewer_join(&stranger(), &host.pc, false, Some(&PAIR_VALUE));
    let proof = json!({"type": "pair-proof", "mac": b64(&[7u8; 32])});
    let actions = h.text(p, &proof);
    assert_eq!(sent_types(&actions, host.id), vec!["pair-proof"]);
    let actions = h.text(host.id, &proof);
    assert_eq!(sent_types(&actions, p), vec!["pair-proof"]);
}

#[test]
fn m2_pairing_viewer_offer_is_not_buffered_for_the_next_host() {
    let mut h = Harness::new();
    let host = host_up(&mut h);
    send_pair_open(&mut h, host.id, &PAIR_VALUE);
    let (p, _) = h.viewer_join(&stranger(), &host.pc, false, Some(&PAIR_VALUE));
    h.close(host.id);
    let actions = h.text(p, &json!({"type": "offer", "sdp": "v=0"}));
    assert!(actions.is_empty());
    let (new_host, actions) = reregister(&mut h, &identity(1));
    assert!(
        !sent_types(&actions, new_host)
            .iter()
            .any(|t| t.as_str() == "offer")
    );
}

#[test]
fn m2_listed_viewer_offer_is_buffered_while_the_host_is_away() {
    // The control for the test above: a listed viewer's offer is held and
    // flushed to the host that registers next.
    let mut h = Harness::new();
    let host = host_up(&mut h);
    let (v, _) = h.viewer_hello(&host.pc, false);
    h.close(host.id);
    h.text(v.id, &json!({"type": "offer", "sdp": "v=0"}));
    let (new_host, actions) = reregister(&mut h, &identity(1));
    assert!(
        sent_types(&actions, new_host)
            .iter()
            .any(|t| t.as_str() == "offer")
    );
}

// ------------------------------------------------------------- allowlist

#[test]
fn m2_auth_hello_is_accepted_from_viewers_only() {
    accepted(Side::Viewer, &auth_hello_msg(201));
    dropped(Side::Host, &auth_hello_msg(201));
}

#[test]
fn m2_auth_hello_rejects_bad_shapes_and_oversize() {
    let mut extra = auth_hello_msg(201);
    extra["extra"] = json!(1);
    dropped(Side::Viewer, &extra);

    let mut too_many = auth_hello_msg(201);
    too_many["caps"] = serde_json::Value::from(vec!["input"; 33]);
    dropped(Side::Viewer, &too_many);

    let mut long_name = auth_hello_msg(201);
    long_name["caps"] = json!(["a".repeat(65)]);
    dropped(Side::Viewer, &long_name);

    let mut big = auth_hello_msg(201);
    big["caps"] = json!(vec!["a".repeat(64); 32]);
    assert!(big.to_string().len() > AUTH_MAX);
    dropped(Side::Viewer, &big);
}

#[test]
fn m2_auth_finish_needs_exact_keys_and_a_64_byte_signature() {
    let finish = |bytes: usize| json!({"type": "auth-finish", "signature": b64(&vec![4u8; bytes])});
    accepted(Side::Viewer, &finish(64));
    dropped(Side::Viewer, &finish(63));
    dropped(Side::Host, &finish(64));
    let mut extra = finish(64);
    extra["extra"] = json!(1);
    dropped(Side::Viewer, &extra);
}

#[test]
fn m2_auth_bind_is_allowed_in_both_directions() {
    let bind = |bytes: usize| json!({"type": "auth-bind", "signature": b64(&vec![3u8; bytes])});
    accepted(Side::Viewer, &bind(64));
    accepted(Side::Host, &bind(64));
    dropped(Side::Viewer, &bind(63));
}

#[test]
fn m2_auth_host_is_host_only() {
    accepted(Side::Host, &auth_host_msg());
    dropped(Side::Viewer, &auth_host_msg());
}

#[test]
fn m2_pair_proof_needs_a_32_byte_mac() {
    let proof = |bytes: usize| json!({"type": "pair-proof", "mac": b64(&vec![7u8; bytes])});
    accepted(Side::Viewer, &proof(32));
    accepted(Side::Host, &proof(32));
    dropped(Side::Viewer, &proof(31));
    let mut extra = proof(32);
    extra["extra"] = json!(1);
    dropped(Side::Host, &extra);
}

#[test]
fn m2_pair_done_and_pair_denied_are_host_only_with_exact_keys() {
    accepted(Side::Host, &json!({"type": "pair-done"}));
    accepted(Side::Host, &json!({"type": "pair-denied"}));
    dropped(Side::Viewer, &json!({"type": "pair-done"}));
    dropped(Side::Viewer, &json!({"type": "pair-denied"}));
    dropped(Side::Host, &json!({"type": "pair-done", "extra": 1}));
}

#[test]
fn m2_pair_open_is_host_only_with_exact_keys() {
    let open = |bytes: usize| json!({"type": "pair-open", "admit": b64(&vec![6u8; bytes])});
    accepted(Side::Host, &open(32));
    dropped(Side::Viewer, &open(32));
    dropped(Side::Host, &open(31));
    let mut extra = open(32);
    extra["extra"] = json!(1);
    dropped(Side::Host, &extra);
}

#[test]
fn m2_admit_is_host_only_and_bounded() {
    let admit = |viewers: Vec<String>, remote: Value| json!({"type": "admit", "viewers": viewers, "remotePairing": remote});
    accepted(Side::Host, &admit(vec![listed_viewer()], json!(false)));
    dropped(Side::Viewer, &admit(vec![listed_viewer()], json!(false)));
    dropped(
        Side::Host,
        &admit(vec![listed_viewer(), listed_viewer()], json!(false)),
    );
    dropped(Side::Host, &admit(vec![listed_viewer()], json!("no")));
    let all: Vec<String> = (0..=ADMIT_VIEWERS_MAX as u16).map(id_of).collect();
    dropped(Side::Host, &admit(all, json!(false)));
}

#[test]
fn m2_pair_close_is_host_only() {
    accepted(Side::Host, &json!({"type": "pair-close"}));
    dropped(Side::Viewer, &json!({"type": "pair-close"}));
}

#[test]
fn m2_ctl_accepts_seq_up_to_2_to_the_53_minus_one_only() {
    accepted(Side::Viewer, &ctl_msg(TWO_TO_53_MINUS_1, ""));
    dropped(Side::Viewer, &ctl_msg(TWO_TO_53_MINUS_1 + 1, ""));
    dropped(Side::Host, &ctl_msg(TWO_TO_53_MINUS_1 + 1, ""));
}

#[test]
fn m2_ctl_data_is_capped_at_ctl_data_max() {
    accepted(Side::Host, &ctl_msg(0, &"A".repeat(CTL_DATA_MAX)));
    dropped(Side::Host, &ctl_msg(0, &"A".repeat(CTL_DATA_MAX + 1)));
    // Four more characters keep the base64 valid, so only the cap refuses it.
    dropped(Side::Host, &ctl_msg(0, &"A".repeat(CTL_DATA_MAX + 4)));
}

#[test]
fn m2_ctl_data_must_be_base64url() {
    dropped(Side::Viewer, &ctl_msg(0, "!!!"));
}

#[test]
fn m2_ctl_is_accepted_from_both_sides() {
    accepted(Side::Viewer, &ctl_msg(0, ""));
    accepted(Side::Host, &ctl_msg(0, ""));
}
