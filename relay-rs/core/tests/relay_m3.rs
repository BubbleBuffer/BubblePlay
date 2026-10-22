//! Hibernation (M3-B): what an adapter attaches to each socket, rehydration
//! after a hibernation, and the next deadline. The lockstep test runs one relay
//! through a script; at every cut point it rebuilds a second relay from the
//! attachments and checks that both answer the rest of the script alike.

mod relay_harness;

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};

use bp_relay_core::{
    ATTACHMENT_MAX, Action, CachedCapabilities, ConnId, ConnPhase, ConnSnapshot, CoreConfig, Relay,
    Route, SnapshotPairWindow, SnapshotTicket,
};
use relay_harness::*;
use serde_json::json;

const PAIR: [u8; 32] = [42; 32];
const WRONG: [u8; 32] = [43; 32];
const OFFER: &str = r#"{"type":"offer","sdp":"v=0"}"#;
const ANSWER: &str = r#"{"type":"answer","sdp":"v=0"}"#;
const HOST_CAPS: &str = r#"{"type":"host-capabilities","serviceMode":false,"coldBoot":false,"secureDesktop":false,"secureText":false}"#;
const HOST_CAPS_SERVICE: &str = r#"{"type":"host-capabilities","serviceMode":true,"coldBoot":false,"secureDesktop":false,"secureText":false}"#;
/// The idle step: a quarter of the 90 s idle timeout.
const STEP_MS: u64 = 22_500;
const SCRIPT_LEN: usize = 7;

/// What an adapter attaches to each open socket: the latest snapshot it was handed.
type Attached = BTreeMap<ConnId, ConnSnapshot>;

/// The sockets of the lockstep setup.
#[derive(Clone, Copy)]
struct Ids {
    host: ConnId,
    viewer: ConnId,
    media_host: ConnId,
    media_viewer: ConnId,
}

/// One step of the lockstep script.
#[derive(Clone, Copy, Debug)]
enum Op {
    Ping,
    ViewerOffer,
    HostAnswer,
    MediaFrame,
    HostCaps,
    Advance(u64),
    Close(ConnId),
}

/// Records what the relay attached: the latest snapshot of each socket, and no
/// entry for a socket that was closed.
fn track(attached: &mut Attached, actions: &[Action]) {
    for action in actions {
        match action {
            Action::Snapshot(id, snapshot) => {
                attached.insert(*id, snapshot.clone());
            }
            Action::Close(id, _, _) => {
                attached.remove(id);
            }
            _ => {}
        }
    }
}

/// Does what the adapter does with `actions`: records them, and for each close
/// calls `on_close` and handles what that returns. Returns every action seen.
fn settle(h: &mut Harness, attached: &mut Attached, actions: Vec<Action>) -> Vec<Action> {
    track(attached, &actions);
    let closing: Vec<ConnId> = actions
        .iter()
        .filter_map(|action| match action {
            Action::Close(id, _, _) => Some(*id),
            _ => None,
        })
        .collect();
    let mut seen = actions;
    for id in closing {
        let after = h.close(id);
        attached.remove(&id);
        seen.extend(settle(h, attached, after));
    }
    seen
}

/// Runs one script step on the relay and settles what it returns.
fn step(h: &mut Harness, attached: &mut Attached, ids: Ids, op: Op) -> Vec<Action> {
    let actions = match op {
        Op::Ping => ping(h, ids.host),
        Op::ViewerOffer => h.raw(ids.viewer, OFFER),
        Op::HostAnswer => h.raw(ids.host, ANSWER),
        Op::MediaFrame => h.binary(ids.media_host, &[7u8; 29]),
        Op::HostCaps => h.raw(ids.host, HOST_CAPS_SERVICE),
        Op::Advance(ms) => h.advance(ms),
        Op::Close(id) => {
            attached.remove(&id);
            h.close(id)
        }
    };
    settle(h, attached, actions)
}

fn ping(h: &mut Harness, host: ConnId) -> Vec<Action> {
    h.text(host, &json!({"type": "ping"}))
}

fn without_activity(snapshot: &ConnSnapshot) -> ConnSnapshot {
    ConnSnapshot {
        activity_ms: 0,
        ..snapshot.clone()
    }
}

fn without_snapshots(actions: &[Action]) -> Vec<Action> {
    actions
        .iter()
        .filter(|action| !matches!(action, Action::Snapshot(..)))
        .cloned()
        .collect()
}

/// A harness built with `tweak`, one registered host, and the default viewer
/// admitted by it. Every action is settled into `attached`. Returns the harness,
/// the host, its PC and its ticket.
fn start_with(
    tweak: impl FnOnce(&mut CoreConfig),
    attached: &mut Attached,
) -> (Harness, ConnId, String, String) {
    let mut h = Harness::with(tweak);
    let host_identity = identity(1);
    let pc = host_identity.id().to_string();
    let (host, actions) = h.host_hello(&host_identity, Some(TOKEN));
    let nonce = challenge_nonce(&actions, host).expect("register-challenge sent");
    settle(&mut h, attached, actions);
    let actions = h.text(host, &proof_msg(&host_identity, ORIGIN, &nonce));
    let ticket = hello_ok(&actions, host)["ticket"]
        .as_str()
        .expect("ticket")
        .to_owned();
    settle(&mut h, attached, actions);
    let admit = json!({
        "type": "admit",
        "viewers": [viewer_identity().id().to_string()],
        "remotePairing": false,
    });
    let actions = h.text(host, &admit);
    settle(&mut h, attached, actions);
    (h, host, pc, ticket)
}

/// The lockstep setup: a registered host with cached capabilities and an open
/// pairing window, two refused strangers, an admitted viewer and a media pair.
fn setup(attached: &mut Attached) -> (Harness, Ids) {
    let (mut h, host, pc, ticket) = start_with(|_| {}, attached);
    let actions = h.text(host, &json!({"type": "pair-open", "admit": b64(&PAIR)}));
    settle(&mut h, attached, actions);
    for _ in 0..2 {
        let (_, actions) = h.viewer_join(&identity(201), &pc, false, Some(&WRONG));
        settle(&mut h, attached, actions);
    }
    let actions = h.raw(host, HOST_CAPS);
    settle(&mut h, attached, actions);
    let (viewer, actions) = h.viewer_hello(&pc, false);
    settle(&mut h, attached, actions);
    let (media_host, actions) = h.data_hello(
        Route::HostMediaWs,
        &pc,
        &viewer.epoch,
        "media-host",
        &ticket,
    );
    settle(&mut h, attached, actions);
    let (media_viewer, actions) = h.data_hello(
        Route::MediaWs,
        &pc,
        &viewer.epoch,
        "media-client",
        &viewer.ticket,
    );
    settle(&mut h, attached, actions);
    let ids = Ids {
        host,
        viewer: viewer.id,
        media_host,
        media_viewer,
    };
    (h, ids)
}

#[test]
fn m3_rehydrated_relay_answers_like_the_one_it_came_from() {
    for cut in 0..=SCRIPT_LEN {
        let mut attached = Attached::new();
        let (mut a, ids) = setup(&mut attached);
        let script: [Op; SCRIPT_LEN] = [
            Op::Ping,
            Op::ViewerOffer,
            Op::HostAnswer,
            Op::MediaFrame,
            Op::Advance(1_000),
            Op::HostCaps,
            Op::Close(ids.viewer),
        ];
        for op in &script[..cut] {
            step(&mut a, &mut attached, ids, *op);
        }

        // Every open socket has an attachment, and it matches the live state
        // (the activity may lag by less than one step, so it is not compared).
        let live: Vec<ConnId> = [ids.host, ids.viewer, ids.media_host, ids.media_viewer]
            .into_iter()
            .filter(|id| a.relay.snapshot_of(*id).is_some())
            .collect();
        assert_eq!(attached.len(), live.len(), "cut {cut}: {attached:?}");
        let snapshots: Vec<ConnSnapshot> = live
            .iter()
            .map(|id| {
                let attachment = attached
                    .get(id)
                    .unwrap_or_else(|| panic!("cut {cut}: no attachment for {id:?}"));
                let live_state = a.relay.snapshot_of(*id).expect("live socket");
                assert_eq!(
                    without_activity(attachment),
                    without_activity(&live_state),
                    "cut {cut}: socket {id:?}"
                );
                attachment.clone()
            })
            .collect();

        let (relay, closes) = Relay::rehydrate(
            a.relay.config().clone(),
            a.relay.pcs().clone(),
            snapshots,
            a.now,
        );
        assert!(closes.is_empty(), "cut {cut}: {closes:?}");
        let mut b = Harness {
            relay,
            rng: a.rng.clone(),
            now: a.now,
            client: a.client,
        };
        let mut b_attached = attached.clone();
        for op in &script[cut..] {
            let from_a = step(&mut a, &mut attached, ids, *op);
            let from_b = step(&mut b, &mut b_attached, ids, *op);
            assert_eq!(
                without_snapshots(&from_a),
                without_snapshots(&from_b),
                "cut {cut}, {op:?}"
            );
        }
    }
}

#[test]
fn m3_pre_auth_sockets_come_back_closed() {
    let mut h = Harness::new();
    let host_identity = identity(1);
    let pc = host_identity.id().to_string();
    let (host, _) = h.host_hello(&host_identity, Some(TOKEN));
    let host_snapshot = h.relay.snapshot_of(host).expect("host is open");
    assert_eq!(host_snapshot.phase, ConnPhase::HostProof);

    let viewer = h.upgrade(req(Route::Ws, Some(pc.as_str()), None, true));
    h.text(viewer, &viewer_hello_msg(&viewer_identity(), false));
    let viewer_snapshot = h.relay.snapshot_of(viewer).expect("viewer is open");
    assert_eq!(viewer_snapshot.phase, ConnPhase::ViewerProof);

    let (_, out) = Relay::rehydrate(
        h.relay.config().clone(),
        h.relay.pcs().clone(),
        vec![host_snapshot, viewer_snapshot],
        h.now,
    );
    assert_eq!(closes(&out, host), vec![(1008, String::new())]);
    assert_eq!(closes(&out, viewer), vec![(1008, String::new())]);
}

#[test]
fn m3_pair_failure_count_survives_a_rehydration() {
    let mut attached = Attached::new();
    let (mut h, host, pc, _) = start_with(|_| {}, &mut attached);
    let actions = h.text(host, &json!({"type": "pair-open", "admit": b64(&PAIR)}));
    settle(&mut h, &mut attached, actions);

    // Nineteen refused attempts, five per client prefix so no prefix limit applies.
    for i in 0u8..19 {
        h.client = IpAddr::V4(Ipv4Addr::new(10, 0, i / 5, 1));
        let (_, actions) = h.viewer_join(&identity(201), &pc, false, Some(&WRONG));
        settle(&mut h, &mut attached, actions);
    }
    let window = attached[&host].pair_window.clone().expect("pair window");
    assert_eq!(window.failures, 19);

    let snapshots: Vec<ConnSnapshot> = attached.values().cloned().collect();
    assert_eq!(snapshots.len(), 1);
    let (relay, out) = Relay::rehydrate(
        h.relay.config().clone(),
        h.relay.pcs().clone(),
        snapshots,
        h.now,
    );
    assert!(out.is_empty(), "{out:?}");

    // The twentieth attempt, from a fresh prefix, drops the window in both relays.
    let twentieth = IpAddr::V4(Ipv4Addr::new(10, 0, 3, 1));
    let mut b = Harness {
        relay,
        rng: h.rng.clone(),
        now: h.now,
        client: twentieth,
    };
    let (stranger, actions) = b.viewer_join(&identity(201), &pc, false, Some(&WRONG));
    assert!(
        sent_types(&actions, host).contains(&"pair-closed".to_owned()),
        "{actions:?}"
    );
    assert_eq!(closed(&actions, stranger), Some((1008, String::new())));

    h.client = twentieth;
    let (stranger_a, actions) = h.viewer_join(&identity(201), &pc, false, Some(&WRONG));
    assert!(
        sent_types(&actions, host).contains(&"pair-closed".to_owned()),
        "{actions:?}"
    );
    assert_eq!(closed(&actions, stranger_a), Some((1008, String::new())));
}

#[test]
fn m3_contradictory_snapshots_close_and_nothing_panics() {
    let mut attached = Attached::new();
    let (mut h, host, pc, _) = start_with(|_| {}, &mut attached);
    let (viewer, actions) = h.viewer_hello(&pc, false);
    settle(&mut h, &mut attached, actions);
    let host_snapshot = attached[&host].clone();
    let viewer_snapshot = attached[&viewer.id].clone();
    let config = h.relay.config().clone();
    let pcs = h.relay.pcs().clone();
    let now = h.now;

    // A second host snapshot under a higher id: a duplicate of a live host.
    let mut duplicate = host_snapshot.clone();
    duplicate.id = ConnId(900);
    let (_, out) = Relay::rehydrate(
        config.clone(),
        pcs.clone(),
        vec![host_snapshot.clone(), duplicate],
        now,
    );
    assert_eq!(closes(&out, ConnId(900)), vec![(1008, String::new())]);
    assert!(closes(&out, host).is_empty(), "{out:?}");

    // An epoch that is not 32 lowercase hex digits.
    let mut bad_epoch = viewer_snapshot.clone();
    bad_epoch.epoch = "bad epoch".to_owned();
    let (_, out) = Relay::rehydrate(config.clone(), pcs.clone(), vec![bad_epoch], now);
    assert_eq!(closes(&out, viewer.id), vec![(1008, String::new())]);

    // A socket that is not live.
    let mut early = host_snapshot;
    early.phase = ConnPhase::Hello;
    let (_, out) = Relay::rehydrate(config, pcs, vec![early], now);
    assert_eq!(closes(&out, host), vec![(1008, String::new())]);
}

#[test]
fn m3_worst_case_attachment_fits_the_limit() {
    let snapshot = ConnSnapshot {
        id: ConnId(u64::MAX),
        route: Route::HostVersionWs,
        pc: identity(1).id().to_string(),
        epoch: "0".repeat(32),
        phase: ConnPhase::Live,
        device: String::new(),
        pairing: false,
        activity_ms: u64::MAX,
        ticket: Some(SnapshotTicket {
            hash: [0xff; 32],
            expires_ms: u64::MAX,
        }),
        cached: vec![
            CachedCapabilities {
                kind: "host-capabilities".to_owned(),
                raw: vec![0xff; 512],
            },
            CachedCapabilities {
                kind: "input-capabilities".to_owned(),
                raw: vec![0xff; 256],
            },
        ],
        pair_window: Some(SnapshotPairWindow {
            admit: [0xff; 32],
            expires_ms: u64::MAX,
            failures: 19,
        }),
    };
    let json = serde_json::to_string(&snapshot).expect("serializes");
    assert!(json.len() <= ATTACHMENT_MAX, "{} bytes", json.len());
    let back: ConnSnapshot = serde_json::from_str(&json).expect("parses");
    assert_eq!(back, snapshot);
}

#[test]
fn m3_next_deadline_moves_with_the_idle_deadline() {
    let (mut h, host, _, _) = start_with(|_| {}, &mut Attached::new());
    h.advance(60_000);
    ping(&mut h, host);
    assert_eq!(h.relay.next_deadline(), Some(START_MS + 150_000));

    assert!(h.advance(89_999).is_empty());
    assert_eq!(h.relay.next_deadline(), Some(START_MS + 150_000));
    let out = h.advance(1);
    assert_eq!(
        closed(&out, host),
        Some((1001, "peer disconnected".to_owned()))
    );
}

#[test]
fn m3_next_deadline_wakes_for_a_ticket_expiry() {
    let mut attached = Attached::new();
    let (mut h, host, _, _) = start_with(|c| c.timings.ticket_ttl_ms = 200_000, &mut attached);
    let expiry = attached[&host]
        .ticket
        .as_ref()
        .expect("the host holds a ticket")
        .expires_ms;
    h.advance(80_000);
    ping(&mut h, host);
    h.advance(80_000);
    ping(&mut h, host);
    assert_eq!(h.relay.next_deadline(), Some(expiry));

    assert!(h.advance(expiry - 1 - h.now).is_empty());
    let out = h.advance(1);
    assert!(
        out.iter().any(|action| matches!(
            action,
            Action::Snapshot(id, snapshot) if *id == host && snapshot.ticket.is_none()
        )),
        "{out:?}"
    );
}

#[test]
fn m3_note_activity_persists_once_a_step_has_passed() {
    let (mut h, host, _, _) = start_with(|_| {}, &mut Attached::new());
    assert!(h.relay.note_activity(host, START_MS + 10_000).is_empty());

    let due = h.relay.note_activity(host, START_MS + 80_000);
    assert!(
        matches!(due.as_slice(), [Action::Snapshot(id, _)] if *id == host),
        "{due:?}"
    );
    assert_eq!(h.relay.next_deadline(), Some(START_MS + 170_000));

    assert!(h.advance(START_MS + 169_999 - h.now).is_empty());
    let out = h.advance(1);
    assert_eq!(
        closed(&out, host),
        Some((1001, "peer disconnected".to_owned()))
    );
}

#[test]
fn m3_rehydrated_active_socket_is_not_closed_early() {
    let mut attached = Attached::new();
    let (mut a, host, _, _) = start_with(|_| {}, &mut attached);
    for at in [START_MS + 10_000, START_MS + 20_000] {
        let actions = a.advance(at - a.now);
        settle(&mut a, &mut attached, actions);
        let actions = ping(&mut a, host);
        settle(&mut a, &mut attached, actions);
    }
    let actions = a.advance(START_MS + 60_000 - a.now);
    settle(&mut a, &mut attached, actions);
    let a_deadline = a.relay.next_deadline().expect("host deadline");

    let snapshots: Vec<ConnSnapshot> = attached.values().cloned().collect();
    let (relay, out) = Relay::rehydrate(
        a.relay.config().clone(),
        a.relay.pcs().clone(),
        snapshots,
        a.now,
    );
    assert!(out.is_empty(), "{out:?}");
    let b_deadline = relay.next_deadline().expect("host deadline");
    assert!(
        b_deadline >= a_deadline && b_deadline <= a_deadline + STEP_MS,
        "live deadline {a_deadline}, rehydrated {b_deadline}"
    );

    let mut b = Harness {
        relay,
        rng: a.rng.clone(),
        now: a.now,
        client: a.client,
    };
    let out = b.advance(a_deadline - 1 - b.now);
    assert!(closes(&out, host).is_empty(), "{out:?}");
}

#[test]
fn m3_rehydrated_idle_socket_closes_within_one_step_of_its_deadline() {
    let mut attached = Attached::new();
    let (mut a, host, _, _) = start_with(|_| {}, &mut attached);
    let actions = a.advance(30_000);
    settle(&mut a, &mut attached, actions);

    let snapshots: Vec<ConnSnapshot> = attached.values().cloned().collect();
    let (relay, out) = Relay::rehydrate(
        a.relay.config().clone(),
        a.relay.pcs().clone(),
        snapshots,
        a.now,
    );
    assert!(out.is_empty(), "{out:?}");

    // The live socket would close at START + 90 s. The rehydrated one is one
    // step later at most: START + 112.5 s.
    let mut b = Harness {
        relay,
        rng: a.rng.clone(),
        now: a.now,
        client: a.client,
    };
    let out = b.advance(START_MS + 112_499 - b.now);
    assert!(closes(&out, host).is_empty(), "{out:?}");
    let out = b.advance(1);
    assert_eq!(
        closed(&out, host),
        Some((1001, "peer disconnected".to_owned()))
    );
}
