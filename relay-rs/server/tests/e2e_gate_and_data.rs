//! Real-socket e2e: the ntfy gate in front of HTTP and /ws, the media and file data sockets
//! (forwarding, tickets, epochs, slow peers), and /healthz.

mod common;

use std::net::SocketAddr;

use bp_relay_server::config::{Secret, Turn};
use common::*;
use serde_json::json;

/// The status of a WebSocket upgrade that the server refuses.
async fn refused(addr: SocketAddr, path: &str, origin: Option<&str>, cookie: Option<&str>) -> u16 {
    match connect(addr, path, origin, cookie).await {
        Ok(_) => panic!("upgrade {path} should be refused"),
        Err(err) => http_status(&err).expect("a refusal is an HTTP response"),
    }
}

/// A BPF1 data packet: 16-byte header (magic, kind 3 = data, reserved bytes, sequence, control
/// sequence 0) followed by the payload.
fn file_packet(payload: &[u8]) -> Vec<u8> {
    let mut packet = b"BPF1".to_vec();
    packet.extend_from_slice(&[3, 0, 0, 0]);
    packet.extend_from_slice(&[0; 8]);
    packet.extend_from_slice(payload);
    packet
}

/// A host and one browser viewer, both registered, plus the epoch and tickets a data socket
/// needs.
#[allow(dead_code)]
struct Session {
    host_ws: Ws,
    viewer_ws: Ws,
    pc: String,
    epoch: String,
    host_ticket: String,
    viewer_ticket: String,
    cookie: String,
}

async fn session(server: &Server, env: &Env, seed: u8) -> Session {
    let cookie = browser_cookie(server, env).await;
    let host = identity(seed);
    let pc = pc_id(&host);
    let (host_ws, host_ok) = register_host(server, &host, Some(TOKEN)).await;
    let (viewer_ws, viewer_ok) = viewer_hello(server, &pc, Some(cookie.as_str()), false).await;
    Session {
        host_ws,
        viewer_ws,
        epoch: viewer_ok["mediaEpoch"].as_str().unwrap().to_string(),
        viewer_ticket: viewer_ok["ticket"].as_str().unwrap().to_string(),
        host_ticket: host_ok["ticket"].as_str().unwrap().to_string(),
        pc,
        cookie,
    }
}

/// A navigation without a cookie gets nothing while it waits: ntfy gets the prefix only, and the
/// approval answers the same held request with a 302 to the URL it asked for (query included).
#[tokio::test]
async fn e2e_held_navigation_is_admitted_by_an_approval_within_the_hold() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let held = tokio::spawn(navigate(&server, "/pair?x=1").send());

    let ntfy = env.wait_for_ntfy().await;
    assert!(ntfy["message"].as_str().unwrap().contains("client prefix"));
    assert!(!ntfy["message"].as_str().unwrap().contains("127.0.0.1"));
    assert_eq!(ntfy["actions"][0]["label"], "Approve");
    assert_eq!(ntfy["actions"][1]["label"], "Deny");
    assert!(
        !held.is_finished(),
        "nothing is answered before the decision"
    );

    decide(&server, &ntfy_bearer(&ntfy, 0), "approve").await;
    let admitted = tokio::time::timeout(WAIT, held)
        .await
        .expect("the held navigation is answered")
        .unwrap()
        .unwrap();
    assert_eq!(admitted.status(), 302);
    assert_eq!(
        admitted.headers().get("location").unwrap(),
        &format!("{ORIGIN}/pair?x=1")
    );
    let cookie = gate_cookie_of(&admitted).expect("the gate cookie is set");
    assert!(admitted.bytes().await.unwrap().is_empty());

    let page = http()
        .get(format!("{}/pair?x=1", base(server.addr)))
        .header("Cookie", cookie.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
}

/// With no decision, the held navigation ends when the hold does: the connection closes without a
/// status line, a header or a byte of body.
#[tokio::test]
async fn e2e_held_navigation_is_dropped_without_content_when_the_hold_ends() {
    let env = Env::new().await;
    let server = Server::start_with_hold(&env, None, 700).await;
    let (received, took) = raw_navigation(server.addr, "/").await;
    assert!(
        received.is_empty(),
        "a dropped navigation gets no bytes, got {:?}",
        String::from_utf8_lossy(&received)
    );
    assert!(
        took >= std::time::Duration::from_millis(600),
        "held for {took:?}"
    );
    env.wait_for_ntfy().await;
}

/// A deny drops the held navigation at once, with no content, and the prefix then cools down: the
/// next navigation is dropped too and opens no second approval.
#[tokio::test]
async fn e2e_denied_navigation_is_dropped_without_content() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let addr = server.addr;
    let held = tokio::spawn(async move { raw_navigation(addr, "/").await });
    let ntfy = env.wait_for_ntfy().await;
    decide(&server, &ntfy_bearer(&ntfy, 1), "deny").await;
    let (received, took) = tokio::time::timeout(WAIT, held)
        .await
        .expect("the deny ends the hold")
        .unwrap();
    assert!(received.is_empty(), "a denied navigation gets no bytes");
    assert!(took < std::time::Duration::from_secs(10));

    let (again, _) = raw_navigation(server.addr, "/").await;
    assert!(again.is_empty(), "a cooling prefix is dropped");
    assert_eq!(
        env.ntfy.lock().unwrap().len(),
        1,
        "no second approval opens"
    );
}

/// An approval that lands after the hold ended is claimed by the next navigation from the prefix,
/// once: the reload gets the 302 with the cookie, and the one after it is dropped again.
#[tokio::test]
async fn e2e_approval_after_the_hold_is_claimed_once_by_the_next_navigation() {
    let env = Env::new().await;
    let server = Server::start_with_hold(&env, None, 300).await;
    let (received, _) = raw_navigation(server.addr, "/").await;
    assert!(received.is_empty());
    let ntfy = env.wait_for_ntfy().await;
    decide(&server, &ntfy_bearer(&ntfy, 0), "approve").await;

    let reload = navigate(&server, "/").send().await.unwrap();
    assert_eq!(reload.status(), 302);
    assert!(
        gate_cookie_of(&reload).is_some(),
        "the reload claims the cookie"
    );

    let (again, _) = raw_navigation(server.addr, "/").await;
    assert!(again.is_empty(), "the approval is claimed only once");
}

#[tokio::test]
async fn e2e_approval_grants_cookie_and_asset_access() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let cookie = browser_cookie(&server, &env).await;
    assert!(cookie.starts_with("__Host-gate="));

    let client = http();
    let with_cookie = client
        .get(format!("{}/", base(server.addr)))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(with_cookie.status(), 200);
    assert_eq!(with_cookie.text().await.unwrap(), "home");

    // A non-navigation request without the cookie gets the plain 403, and is not held. The
    // gate treats a request with no Sec-Fetch-Dest as a navigation, so this one states that it is
    // a subresource fetch.
    let bare = client
        .get(format!("{}/", base(server.addr)))
        .header("Sec-Fetch-Dest", "empty")
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), 403);
}

#[tokio::test]
async fn e2e_ws_with_cookie_gets_hello_ok_with_epoch() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let cookie = browser_cookie(&server, &env).await;
    let (_host, _) = register_host(&server, &identity(10), Some(TOKEN)).await;
    let pc = pc_id(&identity(10));
    let (_ws, ok) = viewer_hello(&server, &pc, Some(cookie.as_str()), false).await;
    let epoch = ok["mediaEpoch"].as_str().unwrap();
    assert_eq!(epoch.len(), 32, "epoch is 16 bytes in hex");
    assert!(ok["ticket"].as_str().is_some_and(|t| !t.is_empty()));
    assert_eq!(
        ok["iceServers"].as_array().unwrap().len(),
        1,
        "STUN only without TURN"
    );
}

#[tokio::test]
async fn e2e_ws_wrong_origin_is_404() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let cookie = browser_cookie(&server, &env).await;
    let path = format!("/ws?pc={}", pc_id(&identity(11)));
    assert_eq!(
        refused(
            server.addr,
            &path,
            Some("https://evil.test"),
            Some(cookie.as_str())
        )
        .await,
        404
    );
}

#[tokio::test]
async fn e2e_ws_origin_without_valid_cookie_is_accepted_then_closed_1008_empty() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let cookie = browser_cookie(&server, &env).await;
    let (_host, _) = register_host(&server, &identity(11), Some(TOKEN)).await;
    let path = format!("/ws?pc={}", pc_id(&identity(11)));
    // No cookie at all, then a cookie the gate does not hold (an expired one is refused the same
    // way). The upgrade succeeds so the browser sees a 1008, not a 1006 it would retry.
    for presented in [None, Some("__Host-gate=not-a-cookie")] {
        let mut ws = connect(server.addr, &path, Some(ORIGIN), presented)
            .await
            .expect("the upgrade is accepted");
        assert_eq!(expect_close(&mut ws).await, (1008, String::new()));
    }
    // The same path with the approved cookie still answers hello-ok.
    let (_ws, ok) =
        viewer_hello(&server, &pc_id(&identity(11)), Some(cookie.as_str()), false).await;
    assert_eq!(ok["mediaEpoch"].as_str().map(str::len), Some(32));
}

#[tokio::test]
async fn e2e_ws_with_turn_adds_credentialed_ice_server() {
    let env = Env::new().await;
    let turn = Turn {
        host: "turn.test".to_string(),
        secret: Secret::new("turn-secret"),
        ttl_s: 3600,
    };
    let server = Server::start(&env, Some(turn)).await;
    let cookie = browser_cookie(&server, &env).await;
    let (_host, _) = register_host(&server, &identity(12), Some(TOKEN)).await;
    let pc = pc_id(&identity(12));
    let (_on, on) = viewer_hello(&server, &pc, Some(cookie.as_str()), true).await;
    let servers = on["iceServers"].as_array().unwrap();
    assert_eq!(
        servers.len(),
        2,
        "STUN plus TURN when the viewer asks for TURN"
    );
    assert!(servers[1]["credential"].is_string());
    let (_off, off) = viewer_hello(&server, &pc, Some(cookie.as_str()), false).await;
    assert_eq!(off["iceServers"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn e2e_media_forwards_both_ways() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let s = session(&server, &env, 20).await;
    let (mut host_media, _) = data_socket(
        &server,
        "/host-media-ws",
        &s.pc,
        &s.epoch,
        "media-host",
        &s.host_ticket,
        None,
    )
    .await;
    let (mut viewer_media, _) = data_socket(
        &server,
        "/media-ws",
        &s.pc,
        &s.epoch,
        "media-viewer",
        &s.viewer_ticket,
        Some(s.cookie.as_str()),
    )
    .await;

    send_binary(&mut host_media, vec![9u8; 64]).await;
    assert_eq!(next_binary(&mut viewer_media).await, vec![9u8; 64]);

    send_json(
        &mut viewer_media,
        json!({ "type": "keyframe", "reason": "test" }),
    )
    .await;
    let keyframe = next_json_of(&mut host_media, "keyframe").await;
    assert_eq!(keyframe["reason"], "test");
}

#[tokio::test]
async fn e2e_file_socket_forwards_both_ways_and_answers_ping() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let s = session(&server, &env, 21).await;
    let (mut file_host, _) = data_socket(
        &server,
        "/host-file-ws",
        &s.pc,
        &s.epoch,
        "file-host",
        &s.host_ticket,
        None,
    )
    .await;
    let (mut file_client, _) = data_socket(
        &server,
        "/file-ws",
        &s.pc,
        &s.epoch,
        "file-viewer",
        &s.viewer_ticket,
        Some(s.cookie.as_str()),
    )
    .await;

    let packet = file_packet(b"hello");
    send_binary(&mut file_host, packet.clone()).await;
    assert_eq!(next_binary(&mut file_client).await, packet);
    send_binary(&mut file_client, packet.clone()).await;
    assert_eq!(next_binary(&mut file_host).await, packet);

    send_json(&mut file_client, json!({ "type": "ping" })).await;
    assert_eq!(next_json_of(&mut file_client, "pong").await["type"], "pong");
}

#[tokio::test]
async fn e2e_wrong_ticket_closes_1008_with_empty_reason() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let s = session(&server, &env, 22).await;
    let bogus = b64(&[1u8; 32]);

    let mut host_side = connect(
        server.addr,
        &format!("/host-media-ws?pc={}&epoch={}", s.pc, s.epoch),
        None,
        None,
    )
    .await
    .unwrap();
    send_json(
        &mut host_side,
        json!({ "type": "hello", "role": "media-host", "ticket": bogus }),
    )
    .await;
    assert_eq!(expect_close(&mut host_side).await, (1008, String::new()));

    let mut viewer_side = connect(
        server.addr,
        &format!("/media-ws?pc={}&epoch={}", s.pc, s.epoch),
        Some(ORIGIN),
        Some(s.cookie.as_str()),
    )
    .await
    .unwrap();
    send_json(
        &mut viewer_side,
        json!({ "type": "hello", "role": "media-viewer", "ticket": bogus }),
    )
    .await;
    assert_eq!(expect_close(&mut viewer_side).await, (1008, String::new()));
}

#[tokio::test]
async fn e2e_new_viewer_epoch_evicts_stale_media_and_404s_old_epoch() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(23);
    let pc = pc_id(&host);
    let (_host_ws, host_ok) = register_host(&server, &host, Some(TOKEN)).await;
    let cookie = browser_cookie(&server, &env).await;

    let (mut first, first_ok) = viewer_hello(&server, &pc, Some(cookie.as_str()), false).await;
    let epoch1 = first_ok["mediaEpoch"].as_str().unwrap().to_string();
    let (_host_media, _) = data_socket(
        &server,
        "/host-media-ws",
        &pc,
        &epoch1,
        "media-host",
        host_ok["ticket"].as_str().unwrap(),
        None,
    )
    .await;
    let (mut stale_media, _) = data_socket(
        &server,
        "/media-ws",
        &pc,
        &epoch1,
        "media-viewer",
        first_ok["ticket"].as_str().unwrap(),
        Some(cookie.as_str()),
    )
    .await;

    let (_second, second_ok) = viewer_hello(&server, &pc, Some(cookie.as_str()), false).await;
    assert_ne!(second_ok["mediaEpoch"], first_ok["mediaEpoch"]);

    assert_eq!(
        expect_close(&mut first).await,
        (1008, "peer replaced".to_string())
    );
    assert_eq!(
        expect_close(&mut stale_media).await,
        (1001, "stale epoch".to_string())
    );
    let status = refused(
        server.addr,
        &format!("/media-ws?pc={pc}&epoch={epoch1}"),
        Some(ORIGIN),
        Some(cookie.as_str()),
    )
    .await;
    assert_eq!(status, 404, "an old epoch no longer upgrades");
}

#[tokio::test]
async fn e2e_slow_peer_is_closed_with_1013() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let s = session(&server, &env, 24).await;
    let (mut host_media, _) = data_socket(
        &server,
        "/host-media-ws",
        &s.pc,
        &s.epoch,
        "media-host",
        &s.host_ticket,
        None,
    )
    .await;
    // The viewer's media socket connects and then never reads.
    let (mut idle_viewer, _) = data_socket(
        &server,
        "/media-ws",
        &s.pc,
        &s.epoch,
        "media-viewer",
        &s.viewer_ticket,
        Some(s.cookie.as_str()),
    )
    .await;

    // 400 frames of 64 KiB is about 25 MB: far past the 80-frame queue and the kernel buffers.
    let chunk = vec![5u8; 64 * 1024];
    for _ in 0..400 {
        send_binary(&mut host_media, chunk.clone()).await;
    }
    assert_eq!(
        expect_close(&mut idle_viewer).await,
        (1013, "slow peer".to_string())
    );
}

#[tokio::test]
async fn e2e_healthz_loopback_200_bad_host_404_head_empty() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let client = http();
    let url = format!("{}/healthz", base(server.addr));

    assert_eq!(client.get(&url).send().await.unwrap().status(), 200);

    let head = client.head(&url).send().await.unwrap();
    assert_eq!(head.status(), 200);
    assert!(head.bytes().await.unwrap().is_empty(), "HEAD has no body");

    let bad_host = client
        .get(&url)
        .header("Host", "evil.test")
        .send()
        .await
        .unwrap();
    assert_eq!(bad_host.status(), 404);
}

// M2 section 1: a socket with no Origin needs no gate cookie. The upgrade is accepted, and the
// signed hello decides admission. A bare hello (no ID, no key) is refused 1008 with an empty
// reason. The admitted and unlisted cases are in e2e_m2.rs.
#[tokio::test]
async fn e2e_native_viewer_without_origin_upgrades_and_bare_hello_is_1008() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let pc = pc_id(&identity(40));
    let mut ws = connect(server.addr, &format!("/ws?pc={pc}"), None, None)
        .await
        .expect("the upgrade is accepted without a cookie");
    send_json(&mut ws, json!({ "type": "hello", "role": "viewer" })).await;
    assert_eq!(expect_close(&mut ws).await, (1008, String::new()));
}

/// With the gate cookie, GET and HEAD `/pair` serve the page as `/` does, and `/pair/x` stays 404.
#[tokio::test]
async fn e2e_pair_with_cookie_serves_the_page_and_nothing_below_it() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let cookie = browser_cookie(&server, &env).await;
    let client = http();
    let pair = client
        .get(format!("{}/pair", base(server.addr)))
        .header("Cookie", cookie.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(pair.status(), 200);
    assert_eq!(
        pair.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/html; charset=utf-8"
    );
    assert_eq!(pair.text().await.unwrap(), "home");

    let head = client
        .head(format!("{}/pair", base(server.addr)))
        .header("Cookie", cookie.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
    assert!(head.bytes().await.unwrap().is_empty());

    let below = client
        .get(format!("{}/pair/x", base(server.addr)))
        .header("Cookie", cookie.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(below.status(), 404);
}

/// A media round trip with the viewer's hello role string as given.
async fn media_round_trip(viewer_role: &str, seed: u8) {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let s = session(&server, &env, seed).await;
    let (mut host_media, _) = data_socket(
        &server,
        "/host-media-ws",
        &s.pc,
        &s.epoch,
        "media-host",
        &s.host_ticket,
        None,
    )
    .await;
    let (mut viewer_media, _) = data_socket(
        &server,
        "/media-ws",
        &s.pc,
        &s.epoch,
        viewer_role,
        &s.viewer_ticket,
        Some(s.cookie.as_str()),
    )
    .await;

    send_binary(&mut host_media, vec![9u8; 64]).await;
    assert_eq!(next_binary(&mut viewer_media).await, vec![9u8; 64]);
    send_json(
        &mut viewer_media,
        json!({ "type": "keyframe", "reason": viewer_role }),
    )
    .await;
    let keyframe = next_json_of(&mut host_media, "keyframe").await;
    assert_eq!(keyframe["reason"], viewer_role);
}

/// The old data role names are refused after the hello, with the reason an invalid hello.
async fn old_data_role_is_refused(path: &str, old: &str, seed: u8) {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let s = session(&server, &env, seed).await;
    let mut ws = connect(
        server.addr,
        &format!("{path}?pc={}&epoch={}", s.pc, s.epoch),
        Some(ORIGIN),
        Some(s.cookie.as_str()),
    )
    .await
    .unwrap();
    send_json(
        &mut ws,
        json!({ "type": "hello", "role": old, "ticket": s.viewer_ticket }),
    )
    .await;
    assert_eq!(
        expect_close(&mut ws).await,
        (1008, "invalid hello".to_string())
    );
}

/// The media viewer takes `/media-ws` as `media-viewer`; the old `media-client` is refused.
#[tokio::test]
async fn e2e_media_viewer_takes_media_ws_and_the_old_name_is_refused() {
    media_round_trip("media-viewer", 30).await;
    old_data_role_is_refused("/media-ws", "media-client", 31).await;
}

/// A file round trip with the viewer's hello role string as given.
async fn file_round_trip(viewer_role: &str, seed: u8) {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let s = session(&server, &env, seed).await;
    let (mut file_host, _) = data_socket(
        &server,
        "/host-file-ws",
        &s.pc,
        &s.epoch,
        "file-host",
        &s.host_ticket,
        None,
    )
    .await;
    let (mut file_client, _) = data_socket(
        &server,
        "/file-ws",
        &s.pc,
        &s.epoch,
        viewer_role,
        &s.viewer_ticket,
        Some(s.cookie.as_str()),
    )
    .await;

    let packet = file_packet(b"hello");
    send_binary(&mut file_host, packet.clone()).await;
    assert_eq!(next_binary(&mut file_client).await, packet);
    send_binary(&mut file_client, packet.clone()).await;
    assert_eq!(next_binary(&mut file_host).await, packet);
    send_json(&mut file_client, json!({ "type": "ping" })).await;
    assert_eq!(next_json_of(&mut file_client, "pong").await["type"], "pong");
}

/// The file viewer takes `/file-ws` as `file-viewer`; the old `file-client` is refused.
#[tokio::test]
async fn e2e_file_viewer_takes_file_ws_and_the_old_name_is_refused() {
    file_round_trip("file-viewer", 32).await;
    old_data_role_is_refused("/file-ws", "file-client", 33).await;
}

/// Only `viewer` takes `/ws`. The old `client` is refused on the hello, before any challenge.
#[tokio::test]
async fn e2e_signal_viewer_takes_ws_and_the_old_name_is_refused() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let cookie = browser_cookie(&server, &env).await;
    let old_host = identity(34);
    let (_old_host_ws, _) = register_host(&server, &old_host, Some(TOKEN)).await;
    let mut old = connect(
        server.addr,
        &format!("/ws?pc={}", pc_id(&old_host)),
        Some(ORIGIN),
        Some(cookie.as_str()),
    )
    .await
    .unwrap();
    let viewer = viewer_identity();
    send_json(
        &mut old,
        json!({
            "type": "hello",
            "role": "client",
            "id": viewer.id().to_string(),
            "public": b64(&viewer.public_key()),
            "turn": false,
        }),
    )
    .await;
    assert_eq!(expect_close(&mut old).await, (1008, String::new()));

    let host = identity(35);
    let (_host_ws, _) = register_host(&server, &host, Some(TOKEN)).await;
    let mut ws = connect(
        server.addr,
        &format!("/ws?pc={}", pc_id(&host)),
        Some(ORIGIN),
        Some(cookie.as_str()),
    )
    .await
    .unwrap();
    let ok = viewer_join_as(&mut ws, "viewer", &viewer_identity(), false, None).await;
    assert!(ok["mediaEpoch"].is_string(), "viewer takes /ws");
}
