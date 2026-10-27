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

#[tokio::test]
async fn e2e_viewer_without_cookie_gets_wait_page_and_ntfy_prefix_only() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let page = http()
        .get(format!("{}/", base(server.addr)))
        .header("Accept", "text/html")
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Mode", "navigate")
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains("Waiting for approval"));
    let poll = html
        .split("var token = \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();

    let ntfy = env.wait_for_ntfy().await;
    assert!(ntfy["message"].as_str().unwrap().contains("client prefix"));
    assert!(
        !ntfy.to_string().contains(&poll),
        "ntfy never carries the poll token"
    );
    assert_eq!(ntfy["actions"][0]["label"], "Approve");
    assert_eq!(ntfy["actions"][1]["label"], "Deny");
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

    // A non-navigation request without the cookie gets the plain 403, not the wait page. The
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
        "media-client",
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
        "file-client",
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
        json!({ "type": "hello", "role": "media-client", "ticket": bogus }),
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
        "media-client",
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
        "media-client",
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

/// `/pair` is the browser page too. Without a cookie, a top-level GET gets the gate's wait page.
#[tokio::test]
async fn e2e_pair_without_cookie_gets_wait_page() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let page = http()
        .get(format!("{}/pair", base(server.addr)))
        .header("Accept", "text/html")
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Mode", "navigate")
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("Waiting for approval"));
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

/// A media round trip with the viewer's hello role string as given, old name or new.
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

/// The media viewer's new name takes `/media-ws` the same way its old name does.
#[tokio::test]
async fn e2e_media_viewer_role_names_both_forward() {
    media_round_trip("media-client", 30).await;
    media_round_trip("media-viewer", 31).await;
}

/// A file round trip with the viewer's hello role string as given, old name or new.
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

/// The file viewer's new name takes `/file-ws` the same way its old name does.
#[tokio::test]
async fn e2e_file_viewer_role_names_both_forward() {
    file_round_trip("file-client", 32).await;
    file_round_trip("file-viewer", 33).await;
}

/// The signalling viewer takes `/ws` under its new name `viewer` and its old name `client`.
#[tokio::test]
async fn e2e_signal_viewer_role_names_both_take_ws() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let cookie = browser_cookie(&server, &env).await;
    let mut hosts = Vec::new();
    for (seed, role) in [(34u8, "client"), (35u8, "viewer")] {
        let host = identity(seed);
        let (host_ws, _) = register_host(&server, &host, Some(TOKEN)).await;
        hosts.push(host_ws);
        let mut ws = connect(
            server.addr,
            &format!("/ws?pc={}", pc_id(&host)),
            Some(ORIGIN),
            Some(cookie.as_str()),
        )
        .await
        .unwrap();
        let ok = viewer_join_as(&mut ws, role, &viewer_identity(), false, None).await;
        assert!(ok["mediaEpoch"].is_string(), "{role} takes /ws");
    }
}
