//! Viewer signalling and media: epochs, TURN credentials, replacement and
//! departure, forwarding between the host and the viewer, host capability
//! caching, the pending buffer, and per-PC isolation. Ports the viewer,
//! room and two-PC subtests of routing_test.go and server_test.go.

mod relay_harness;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use bp_relay_core::{Action, ConnId, PENDING_LIMIT, Route, TurnConfig};
use hmac::{Hmac, Mac};
use relay_harness::*;
use serde_json::json;
use sha1::Sha1;

/// A host, its viewer, the host and viewer media sockets, and the join actions.
type MediaPair = (Host, Viewer, ConnId, ConnId, Vec<Action>);

fn is_hex_32(text: &str) -> bool {
    text.len() == 32 && text.chars().all(|c| c.is_ascii_hexdigit())
}

#[test]
fn viewer_hello_returns_epoch_ticket_and_stun_by_default() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let (viewer, actions) = h.viewer_hello(&host.pc, false);
    assert!(
        is_hex_32(&viewer.epoch),
        "16 random bytes as hex: {}",
        viewer.epoch
    );
    assert_eq!(viewer.ticket.len(), 43, "32 bytes, unpadded base64url");
    assert_eq!(
        viewer.ice,
        json!([{"urls": "stun:stun.l.google.com:19302"}])
    );
    assert_eq!(
        hello_ok(&actions, viewer.id)["mediaEpoch"],
        json!(viewer.epoch)
    );
}

#[test]
fn viewer_turn_credentials_follow_the_config() {
    let mut h = Harness::with(|c| {
        c.turn = Some(TurnConfig {
            host: "turn.example.test".to_owned(),
            secret: "turn-secret".to_owned(),
            ttl_s: 3600,
        });
    });
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let (viewer, _) = h.viewer_hello(&host.pc, true);

    let ice = viewer.ice.as_array().expect("iceServers array");
    assert_eq!(ice.len(), 2);
    assert_eq!(ice[0], json!({"urls": "stun:stun.l.google.com:19302"}));
    assert_eq!(
        ice[1]["urls"],
        json!([
            "turn:turn.example.test:3478?transport=udp",
            "turn:turn.example.test:3478?transport=tcp"
        ])
    );
    // now = 1,000,000 ms, so expiry = 1000 s + 3600 s.
    let username = ice[1]["username"].as_str().expect("username");
    let (expiry, suffix) = username.split_once(':').expect("expiry:suffix");
    assert_eq!(expiry, "4600");
    assert_eq!(URL_SAFE_NO_PAD.decode(suffix).expect("base64url").len(), 12);

    let mut mac = Hmac::<Sha1>::new_from_slice(b"turn-secret").expect("key");
    mac.update(username.as_bytes());
    let expected = STANDARD.encode(mac.finalize().into_bytes());
    assert_eq!(ice[1]["credential"], json!(expected));
}

#[test]
fn second_viewer_replaces_the_first_silently_and_rotates_the_epoch() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let (first, _) = h.viewer_hello(&host.pc, false);
    let (second, actions) = h.viewer_hello(&host.pc, false);
    assert_ne!(first.epoch, second.epoch);
    assert_eq!(
        closed(&actions, first.id),
        Some((1008, "peer replaced".to_owned()))
    );
    // The host hears only the new viewer's admission: no peer-left for the old one.
    let to_host = sent_json(&actions, host.id);
    assert_eq!(to_host.len(), 1, "{to_host:?}");
    assert_eq!(to_host[0]["type"], json!("peer-joined"));
    assert_eq!(to_host[0]["mediaEpoch"], json!(second.epoch));
}

#[test]
fn replaced_viewers_media_sockets_close_as_stale() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let (first, _) = h.viewer_hello(&host.pc, false);
    let (media, actions) = h.data_hello(
        Route::MediaWs,
        &host.pc,
        &first.epoch,
        "media-client",
        &first.ticket,
    );
    assert!(sent_types(&actions, media).contains(&"hello-ok".to_owned()));

    let (_second, actions) = h.viewer_hello(&host.pc, false);
    assert_eq!(
        closed(&actions, media),
        Some((1001, "stale epoch".to_owned()))
    );
}

/// Registers a host, opens a viewer, and joins a host media socket and a
/// viewer media socket to the current epoch. Returns the ids and the viewer
/// media join actions.
fn media_pair(h: &mut Harness) -> MediaPair {
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let viewer = h.open_viewer(&host.pc);
    let (host_media, actions) = h.data_hello(
        Route::HostMediaWs,
        &host.pc,
        &viewer.epoch,
        "media-host",
        &host.ticket,
    );
    assert!(sent_types(&actions, host_media).contains(&"hello-ok".to_owned()));
    let (viewer_media, actions) = h.data_hello(
        Route::MediaWs,
        &host.pc,
        &viewer.epoch,
        "media-client",
        &viewer.ticket,
    );
    (host, viewer, host_media, viewer_media, actions)
}

#[test]
fn host_and_viewer_media_sockets_announce_each_other() {
    let mut h = Harness::new();
    let (_host, _viewer, host_media, viewer_media, actions) = media_pair(&mut h);
    let present = json!({"type": "media-peer", "present": true});
    assert!(sent_json(&actions, viewer_media).contains(&present));
    assert!(sent_json(&actions, host_media).contains(&present));
    let ok = hello_ok(&actions, viewer_media);
    assert_eq!(ok["registeredAtMs"], json!(START_MS));
}

#[test]
fn media_frames_flow_from_the_host_to_the_viewer_only() {
    let mut h = Harness::new();
    let (_host, _viewer, host_media, viewer_media, _) = media_pair(&mut h);

    let frame = vec![7u8; 29];
    let out = h.binary(host_media, &frame);
    assert_eq!(sent_binary(&out, viewer_media), vec![frame]);

    // A frame no longer than the 28-byte header is dropped, and so is any
    // frame the viewer sends.
    assert!(h.binary(host_media, &[0u8; 28]).is_empty());
    assert!(h.binary(viewer_media, &[0u8; 29]).is_empty());

    let out = h.text(viewer_media, &json!({"type": "ping"}));
    assert_eq!(sent_types(&out, viewer_media), vec!["pong".to_owned()]);
}

#[test]
fn viewer_departure_closes_its_media_sockets() {
    let mut h = Harness::new();
    let (_host, viewer, _host_media, viewer_media, _) = media_pair(&mut h);
    let out = h.close(viewer.id);
    assert_eq!(
        closed(&out, viewer_media),
        Some((1001, "viewer departed".to_owned()))
    );
}

#[test]
fn viewer_bye_tells_the_host_and_closes_the_viewer() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let viewer = h.open_viewer(&host.pc);
    let out = h.text(viewer.id, &json!({"type": "bye"}));
    assert_eq!(sent_types(&out, host.id), vec!["peer-left".to_owned()]);
    assert_eq!(closed(&out, viewer.id), Some((1000, "bye".to_owned())));
}

#[test]
fn offer_and_answer_are_forwarded_verbatim() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let viewer = h.open_viewer(&host.pc);

    let offer = r#"{"type":"offer","sdp":"v=0"}"#;
    let out = h.raw(viewer.id, offer);
    assert_eq!(sent_text(&out, host.id), vec![offer.to_owned()]);

    let answer = r#"{"type":"answer","sdp":"v=0"}"#;
    let out = h.raw(host.id, answer);
    assert_eq!(sent_text(&out, viewer.id), vec![answer.to_owned()]);
}

#[test]
fn host_capabilities_are_cached_and_replayed_to_a_later_viewer() {
    let mut h = Harness::new();
    let a = identity(1);
    let (host, _) = h.register_host(&a, Some(TOKEN));
    let caps = r#"{"type":"host-capabilities","serviceMode":false,"coldBoot":false,"secureDesktop":false,"secureText":false}"#;
    h.raw(host.id, caps);

    let (later, actions) = h.viewer_hello(&host.pc, false);
    assert!(sent_text(&actions, later.id).contains(&caps.to_owned()));
}

#[test]
fn viewer_messages_wait_for_the_host_and_flush_after_registration() {
    let mut h = Harness::new();
    let a = identity(1);
    // A viewer is admitted only through its PC's stored list, so the host
    // registers once and goes away before the viewer joins.
    let (first, _) = h.register_host(&a, Some(TOKEN));
    h.close(first.id);
    let viewer = h.open_viewer(&identity(1).id().to_string());
    let offer = r#"{"type":"offer","sdp":"v=0"}"#;
    assert!(
        h.raw(viewer.id, offer).is_empty(),
        "no host yet, nothing sent"
    );

    let (host, actions) = h.register_host(&a, Some(TOKEN));
    let texts = sent_text(&actions, host.id);
    assert!(texts[0].contains("hello-ok"));
    assert_eq!(texts.last(), Some(&offer.to_owned()));
}

#[test]
fn pending_buffer_keeps_the_newest_sixty_four_in_order() {
    let mut h = Harness::new();
    let a = identity(1);
    let (first, _) = h.register_host(&a, Some(TOKEN));
    h.close(first.id);
    let viewer = h.open_viewer(&a.id().to_string());
    for i in 0..(PENDING_LIMIT + 6) {
        let ice = json!({"type": "ice", "candidate": format!("c{i}")});
        assert!(h.text(viewer.id, &ice).is_empty());
    }

    let (host, actions) = h.register_host(&a, Some(TOKEN));
    let candidates: Vec<String> = sent_json(&actions, host.id)
        .iter()
        .filter(|m| m["type"] == "ice")
        .map(|m| m["candidate"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(candidates.len(), PENDING_LIMIT);
    assert_eq!(candidates.first().map(String::as_str), Some("c6"));
    assert_eq!(candidates.last().map(String::as_str), Some("c69"));
}

#[test]
fn two_pcs_keep_their_epochs_media_and_pending_apart() {
    let mut h = Harness::new();
    let a = identity(1);
    let b = identity(2);
    let (host_a, _) = h.register_host(&a, Some(TOKEN));
    let (host_b, _) = h.register_host(&b, Some(TOKEN));
    let va = h.open_viewer(&host_a.pc);
    let vb = h.open_viewer(&host_b.pc);
    assert_ne!(va.epoch, vb.epoch);

    // A's epoch does not open B's media socket.
    assert!(
        h.try_upgrade(req(Route::MediaWs, Some(&host_b.pc), Some(&va.epoch), true))
            .is_err()
    );
    // Rotating A's viewer leaves B's epoch valid.
    h.open_viewer(&host_a.pc);
    assert!(
        h.try_upgrade(req(Route::MediaWs, Some(&host_b.pc), Some(&vb.epoch), true))
            .is_ok()
    );
}

#[test]
fn pending_messages_and_capabilities_stay_in_their_own_room() {
    let mut h = Harness::new();
    let a = identity(1);
    let b = identity(2);
    let (first_a, _) = h.register_host(&a, Some(TOKEN));
    h.close(first_a.id);
    let va = h.open_viewer(&a.id().to_string());
    let offer = r#"{"type":"offer","sdp":"v=0"}"#;
    h.raw(va.id, offer);

    let (host_b, _) = h.register_host(&b, Some(TOKEN));
    let caps = r#"{"type":"host-capabilities","serviceMode":true,"coldBoot":false,"secureDesktop":false,"secureText":false}"#;
    h.raw(host_b.id, caps);
    let (vb, vb_actions) = h.viewer_hello(&b.id().to_string(), false);
    assert!(sent_text(&vb_actions, vb.id).contains(&caps.to_owned()));
    assert!(!sent_text(&vb_actions, vb.id).contains(&offer.to_owned()));

    // Room A's buffered offer reaches A's host, and B's capabilities never reach A.
    let (host_a, actions) = h.register_host(&a, Some(TOKEN));
    let texts = sent_text(&actions, host_a.id);
    assert!(texts.contains(&offer.to_owned()));
    assert!(!texts.contains(&caps.to_owned()));
    let (va2, va2_actions) = h.viewer_hello(&host_a.pc, false);
    assert!(!sent_text(&va2_actions, va2.id).contains(&caps.to_owned()));
}
