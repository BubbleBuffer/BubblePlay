//! Handshake, epoch, control, flow cap and replay, over real loopback sockets.

mod common;

use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bp_device_key::Role;
use common::*;
use fake_wss_host::model::Injections;
use fake_wss_host::protocol::decode_frame;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

const WAIT: Duration = Duration::from_secs(2);

fn no_injections() -> Injections {
    Injections::default()
}

#[tokio::test]
async fn hello_gets_hello_ok_with_a_fresh_epoch_each_time() {
    let harness = spawn(
        synthetic(60, 30),
        no_injections(),
        Duration::from_millis(800),
        Duration::from_millis(100),
    )
    .await;
    let (first, epoch_a) = signal_hello(harness.addr).await;
    drop(first);
    let (mut second, epoch_b) = signal_hello(harness.addr).await;
    assert_ne!(epoch_a, epoch_b, "a new hello rotates the epoch");
    assert_eq!(epoch_a.len(), 32);

    let snap = harness.finish().await;
    assert_eq!(snap.epochs, vec![epoch_a, epoch_b.clone()]);
    assert_eq!(snap.counters.epochs_issued, 2);
    // Run end closes every socket the server still holds.
    let closed = next_json(&mut second, WAIT).await;
    assert!(
        closed.is_none(),
        "the signalling socket is closed at run end"
    );
}

#[tokio::test]
async fn media_socket_must_match_the_current_epoch() {
    let harness = spawn(
        synthetic(60, 30),
        no_injections(),
        Duration::from_millis(1200),
        Duration::from_millis(100),
    )
    .await;
    let (mut signal, epoch_a, ticket_a) = signal_hello_ticket(harness.addr).await;

    // A well-formed but wrong epoch is refused at the upgrade.
    match media_open(harness.addr, &"0".repeat(32), &ticket_a).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), 409),
        other => panic!("expected HTTP 409 for a wrong epoch, got {other:?}"),
    }
    // A malformed epoch is refused too.
    assert!(matches!(
        media_open(harness.addr, "NOT-HEX", &ticket_a).await,
        Err(WsError::Http(_))
    ));

    // The current epoch is accepted: both readiness controls are on media.
    let mut media = media_open(harness.addr, &epoch_a, &ticket_a)
        .await
        .expect("upgrade with current epoch");
    let reply = next_json(&mut media, WAIT).await.expect("media hello-ok");
    assert_eq!(reply["type"], "hello-ok", "{reply}");
    assert!(reply["registeredAtMs"].is_u64(), "{reply}");
    let media_peer = next_json(&mut media, WAIT)
        .await
        .expect("media-socket peer");
    assert_eq!(media_peer["type"], "media-peer");
    assert_eq!(media_peer["present"], true);
    let peer = next_json(&mut signal, WAIT).await.expect("media-peer");
    assert_eq!(peer["type"], "media-peer");
    assert_eq!(peer["present"], true);

    // A new /ws hello rotates the epoch and closes the stale media socket.
    let (_signal_b, epoch_b, ticket_b) = signal_hello_ticket(harness.addr).await;
    assert_ne!(epoch_a, epoch_b);
    assert!(
        next_json(&mut media, WAIT).await.is_none(),
        "stale media socket is closed"
    );
    match media_open(harness.addr, &epoch_a, &ticket_a).await {
        Err(WsError::Http(response)) => {
            assert_eq!(response.status(), 409, "old epoch is stale now")
        }
        other => panic!("expected HTTP 409 for a stale epoch, got {other:?}"),
    }
    let mut fresh = media_open(harness.addr, &epoch_b, &ticket_b)
        .await
        .expect("upgrade with the new epoch");
    assert_eq!(
        next_json(&mut fresh, WAIT).await.expect("hello-ok")["type"],
        "hello-ok"
    );

    let snap = harness.finish().await;
    assert!(
        snap.counters.stale_epoch_control >= 3,
        "three refusals: {}",
        snap.counters.stale_epoch_control
    );
    assert_eq!(snap.counters.media_attaches, 2);
}

#[tokio::test]
async fn a_bad_relay_proof_closes_1008_and_admits_nothing() {
    let harness = spawn(
        synthetic(60, 30),
        no_injections(),
        Duration::from_millis(600),
        Duration::from_millis(100),
    )
    .await;
    let identity = viewer_identity();
    let mut ws = open(harness.addr, "/ws").await.expect("open /ws");
    send_json(
        &mut ws,
        json!({
            "type": "hello",
            "role": "viewer",
            "id": identity.id().to_string(),
            "public": identity.public_b64(),
        }),
    )
    .await;
    let challenge = next_json(&mut ws, WAIT).await.expect("register-challenge");
    assert_eq!(challenge["type"], "register-challenge", "{challenge}");
    let nonce: [u8; 32] = URL_SAFE_NO_PAD
        .decode(challenge["nonce"].as_str().expect("nonce"))
        .expect("nonce is base64url")
        .try_into()
        .expect("nonce is 32 bytes");
    // Signed for an origin this relay does not serve: the proof must not verify.
    let signature = identity.sign_relay_proof(Role::Viewer, "https://relay.example", &nonce);
    send_json(
        &mut ws,
        json!({"type": "register-proof", "signature": URL_SAFE_NO_PAD.encode(signature)}),
    )
    .await;
    match ws.next().await {
        Some(Ok(Message::Close(Some(frame)))) => assert_eq!(frame.code, CloseCode::Policy),
        other => panic!("expected a 1008 close, got {other:?}"),
    }

    let snap = harness.finish().await;
    assert!(snap.epochs.is_empty(), "no epoch was issued");
}

#[tokio::test]
async fn a_media_ticket_from_an_earlier_signalling_socket_is_refused() {
    let harness = spawn(
        synthetic(60, 30),
        no_injections(),
        Duration::from_millis(900),
        Duration::from_millis(100),
    )
    .await;
    let (_first, epoch_a, ticket_a) = signal_hello_ticket(harness.addr).await;
    let (_second, epoch_b, ticket_b) = signal_hello_ticket(harness.addr).await;
    assert_ne!(epoch_a, epoch_b);

    // The ticket of the rotated-out signalling socket is refused, and so is an invented one.
    let mut stale = media_open(harness.addr, &epoch_b, &ticket_a)
        .await
        .expect("upgrade");
    assert!(
        next_json(&mut stale, WAIT).await.is_none(),
        "a stale ticket closes the media socket"
    );
    let mut invented = media_open(harness.addr, &epoch_b, "AAAA")
        .await
        .expect("upgrade");
    assert!(
        next_json(&mut invented, WAIT).await.is_none(),
        "an invented ticket closes the media socket"
    );
    let mut fresh = media_open(harness.addr, &epoch_b, &ticket_b)
        .await
        .expect("upgrade");
    assert_eq!(
        next_json(&mut fresh, WAIT).await.expect("hello-ok")["type"],
        "hello-ok"
    );

    let snap = harness.finish().await;
    assert_eq!(
        snap.counters.media_attaches, 1,
        "only the good ticket attaches"
    );
}

#[tokio::test]
async fn control_is_validated_and_stale_epochs_are_ignored() {
    let harness = spawn(
        synthetic(60, 30),
        no_injections(),
        Duration::from_millis(900),
        Duration::from_millis(100),
    )
    .await;
    let (mut signal, epoch) = signal_hello(harness.addr).await;

    send_json(&mut signal, json!({"type": "ping"})).await;
    assert_eq!(
        next_json(&mut signal, WAIT).await.expect("pong")["type"],
        "pong"
    );

    // Stale epoch on a control message: ignored, counted, no start.
    send_json(&mut signal, start_message(&"0".repeat(32), "h264")).await;
    // Malformed JSON, missing epoch on protocol 4, and a bad reason token.
    signal
        .send(Message::Text("{\"type\":\"media-control\"".into()))
        .await
        .unwrap();
    send_json(
        &mut signal,
        json!({"type": "media-control", "action": "start", "protocol": 4}),
    )
    .await;
    send_json(
        &mut signal,
        json!({"type": "media-control", "action": "keyframe", "protocol": 4, "epoch": epoch, "reason": "bad reason"}),
    )
    .await;
    // A known-to-the-browser control the fake does not act on is counted by name.
    send_json(
        &mut signal,
        json!({"type": "media-control", "action": "monitor", "protocol": 4, "epoch": epoch}),
    )
    .await;
    // A control message on a signalling socket that has not said hello is refused.
    let mut bare = open(harness.addr, "/ws").await.unwrap();
    send_json(&mut bare, start_message(&epoch, "h264")).await;

    // Wrong protocol on the media-control path is refused too.
    send_json(
        &mut signal,
        json!({"type": "media-control", "action": "start", "protocol": 3, "epoch": epoch}),
    )
    .await;

    let snap = harness.finish().await;
    assert_eq!(snap.counters.starts, 0, "no start was accepted");
    assert!(
        snap.counters.stale_epoch_control >= 1,
        "stale epoch counted"
    );
    assert!(
        snap.counters.rejected_control >= 4,
        "rejected: {}",
        snap.counters.rejected_control
    );
    assert_eq!(snap.counters.unhandled_control.get("monitor"), Some(&1));
}

#[tokio::test]
async fn media_text_is_refused_until_the_media_hello() {
    let harness = spawn(
        synthetic(60, 30),
        no_injections(),
        Duration::from_millis(700),
        Duration::from_millis(100),
    )
    .await;
    let (_signal, epoch) = signal_hello(harness.addr).await;
    let mut media = open(harness.addr, &format!("/media-ws?epoch={epoch}"))
        .await
        .unwrap();
    // An ACK before hello is not a hello: refused, and the socket gets nothing.
    send_json(
        &mut media,
        json!({"type": "ack", "generation": 1, "seq": 1, "recvTsMs": 0}),
    )
    .await;
    assert!(next_json(&mut media, Duration::from_millis(200))
        .await
        .is_none());
    let snap = harness.finish().await;
    assert!(snap.counters.rejected_control >= 1);
    assert_eq!(snap.counters.media_attaches, 0);
}

#[tokio::test]
async fn flow_cap_holds_at_four_frames_without_acks() {
    let harness = spawn(
        synthetic(60, 30),
        no_injections(),
        Duration::from_millis(1400),
        Duration::from_millis(100),
    )
    .await;
    let (_signal, mut media, _epoch) = streaming_session(harness.addr, "h264").await;

    // Read for half a second without ACKing: exactly four binary frames arrive.
    let until = Instant::now() + Duration::from_millis(500);
    let mut binary = 0;
    while let Ok(Some(Ok(message))) = timeout(
        until.saturating_duration_since(Instant::now()),
        media.next(),
    )
    .await
    {
        if matches!(message, Message::Binary(_)) {
            binary += 1;
        }
    }
    assert_eq!(binary, 4, "the in-flight cap is four frames");

    let snap = harness.finish().await;
    assert_eq!(snap.counters.frames_sent, 4);
    assert_eq!(snap.counters.max_inflight, 4);
    assert!(
        snap.counters.backpressure_holds > 0,
        "the source is held, not skipped"
    );
}

#[tokio::test]
async fn duplicate_and_unknown_acks_are_ignored() {
    let harness = spawn(
        synthetic(60, 30),
        no_injections(),
        Duration::from_millis(1000),
        Duration::from_millis(100),
    )
    .await;
    let (_signal, mut media, _epoch) = streaming_session(harness.addr, "h264").await;

    let first = next_frame(&mut media).await;
    let ack =
        json!({"type": "ack", "generation": first.generation, "seq": first.seq, "recvTsMs": 0});
    send_json(&mut media, ack.clone()).await;
    send_json(&mut media, ack).await; // replay of the same ACK
    send_json(&mut media, json!({"type": "ack", "generation": first.generation.wrapping_add(7), "seq": first.seq, "recvTsMs": 0})).await;

    let snap = harness.finish().await;
    assert!(
        snap.counters.frames_retired_by_ack >= 1,
        "the first ACK retires its frame"
    );
    assert!(
        snap.counters.acks_duplicate_or_stale >= 1,
        "the replay is counted and retires nothing"
    );
    assert_eq!(snap.counters.acks_unknown_generation, 1);
    assert_eq!(snap.counters.acks_received, 3);
}

#[tokio::test]
async fn codec_mismatch_fails_first_frame_and_starts_nothing() {
    let harness = spawn(
        synthetic(60, 30),
        no_injections(),
        Duration::from_millis(700),
        Duration::from_millis(100),
    )
    .await;
    let (mut signal, mut media, _epoch) = streaming_session(harness.addr, "hevc").await;
    send_json(
        &mut signal,
        json!({"type": "first-frame", "protocol": 1, "phase": "request", "requestId": "ff-1"}),
    )
    .await;
    let mut failed = None;
    while let Some(msg) = next_json(&mut signal, Duration::from_millis(600)).await {
        if msg["type"] == "first-frame" && msg["phase"] == "failed" {
            failed = Some(msg);
            break;
        }
    }
    let failed = failed.expect("first-frame failed for a codec mismatch");
    assert_eq!(failed["reason"], "codec-mismatch");
    assert_eq!(failed["requestId"], "ff-1");
    assert!(
        next_json(&mut media, Duration::from_millis(100))
            .await
            .is_none(),
        "no frame for a refused start"
    );
    let snap = harness.finish().await;
    assert_eq!(snap.counters.codec_mismatches, 1);
    assert_eq!(snap.counters.frames_sent, 0);
}

#[tokio::test]
async fn first_frame_is_admitted_then_ready_on_the_first_keyframe() {
    let harness = spawn(
        synthetic(60, 30),
        no_injections(),
        Duration::from_millis(900),
        Duration::from_millis(100),
    )
    .await;
    let (mut signal, mut media, _epoch) = streaming_session(harness.addr, "h264").await;
    send_json(
        &mut signal,
        json!({"type": "first-frame", "protocol": 1, "phase": "request", "requestId": "ff-2"}),
    )
    .await;
    let admitted = next_json(&mut signal, WAIT).await.expect("admitted");
    assert_eq!(admitted["phase"], "admitted");
    assert_eq!(admitted["requestId"], "ff-2");
    assert!(
        admitted["generation"].is_u64(),
        "admitted carries the generation"
    );
    let key = next_frame(&mut media).await;
    assert!(key.key, "the first frame of a session is an IDR");
    let ready = next_json(&mut signal, WAIT).await.expect("ready");
    assert_eq!(ready["phase"], "ready");
    send_json(
        &mut signal,
        json!({"type": "first-frame", "protocol": 1, "phase": "presented", "requestId": "ff-2"}),
    )
    .await;
    let snap = harness.finish().await;
    assert_eq!(snap.counters.first_frame_requests, 1);
    assert_eq!(snap.counters.first_frame_presented, 1);
}

/// The next binary frame, read raw: not ACKed, so the test controls the ACKs.
async fn next_frame(media: &mut Ws) -> Got {
    loop {
        match timeout(WAIT, media.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                let (header, _) = decode_frame(&bytes).expect("valid BPV4 frame");
                return Got {
                    generation: header.generation,
                    seq: header.seq,
                    key: header.key,
                    codec: header.codec,
                };
            }
            Ok(Some(Ok(_))) => continue,
            other => panic!("no frame within {WAIT:?}: {other:?}"),
        }
    }
}
