//! M2 real-socket e2e (relay-rs M2 sections 1 to 4): the signed viewer hello on the
//! server, native (Origin-less) viewers, the admitted list persisted in pcs.json and
//! listed by the admin API, pairing, and ctl. Every test name starts with `m2_`.

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bp_device_key::{DeviceIdentity, Role};
use common::*;
use serde_json::{Value, json};
use tokio::time::sleep;

/// Sends the signed viewer hello for `who` (with an optional pairing value) and
/// answers the register-challenge. The relay's next frame is left to the test.
async fn prove_viewer(ws: &mut Ws, who: &DeviceIdentity, pair: Option<&[u8; 32]>) {
    let mut hello = json!({
        "type": "hello",
        "role": "viewer",
        "id": who.id().to_string(),
        "public": b64(&who.public_key()),
        "turn": false,
    });
    if let Some(pair) = pair {
        hello["pair"] = json!(b64(pair));
    }
    send_json(ws, hello).await;
    let challenge = next_json_of(ws, "register-challenge").await;
    let nonce: [u8; 32] = URL_SAFE_NO_PAD
        .decode(challenge["nonce"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let signature = who.sign_relay_proof(Role::Viewer, ORIGIN, &nonce);
    send_json(
        ws,
        json!({ "type": "register-proof", "signature": b64(&signature) }),
    )
    .await;
}

/// A viewer socket on /ws with no Origin, the way a native client connects.
async fn native_viewer(server: &Server, pc: &str) -> Ws {
    connect(server.addr, &format!("/ws?pc={pc}"), None, None)
        .await
        .expect("the upgrade is accepted without an Origin")
}

/// The admin listing: one JSON object per PC.
async fn admin_rows(server: &Server) -> Vec<Value> {
    let response = http()
        .get(format!("{}/__gate/manage/pcs", base(server.addr)))
        .header("Authorization", format!("Bearer {ADMIN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    read_json(response)
        .await
        .as_array()
        .cloned()
        .expect("the listing is a JSON array")
}

fn row_for<'a>(rows: &'a [Value], pc: &str) -> &'a Value {
    rows.iter()
        .find(|row| row["id"].as_str() == Some(pc))
        .expect("the PC is listed")
}

/// Polls `path` until it contains `needle`, so the test does not race the write.
async fn wait_for_text(path: &Path, needle: &str) {
    let deadline = Instant::now() + WAIT;
    while !fs::read_to_string(path).is_ok_and(|text| text.contains(needle)) {
        assert!(
            Instant::now() < deadline,
            "{needle} was not saved to {}",
            path.display()
        );
        sleep(Duration::from_millis(25)).await;
    }
}

fn auth_hello_json(who: &DeviceIdentity) -> Value {
    json!({
        "type": "auth-hello",
        "id": who.id().to_string(),
        "public": b64(&who.public_key()),
        "nonce": b64(&[9u8; 32]),
        "ephemeral": b64(&[8u8; 32]),
        "caps": ["input"],
    })
}

fn auth_host_json(host: &DeviceIdentity) -> Value {
    json!({
        "type": "auth-host",
        "id": host.id().to_string(),
        "public": b64(&[1u8; 32]),
        "nonce": b64(&[9u8; 32]),
        "ephemeral": b64(&[8u8; 32]),
        "signature": b64(&[5u8; 64]),
    })
}

#[tokio::test]
async fn m2_admitted_native_viewer_gets_a_working_media_ticket() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(1);
    let (_host_ws, _) = register_host(&server, &host, Some(TOKEN)).await;
    let pc = pc_id(&host);

    let mut viewer = native_viewer(&server, &pc).await;
    prove_viewer(&mut viewer, &viewer_identity(), None).await;
    let ok = next_json_of(&mut viewer, "hello-ok").await;
    let ticket = ok["ticket"].as_str().expect("ticket").to_owned();
    let epoch = ok["mediaEpoch"].as_str().expect("mediaEpoch").to_owned();

    // The media socket has no Origin either, and the ticket admits it.
    let mut media = connect(
        server.addr,
        &format!("/media-ws?pc={pc}&epoch={epoch}"),
        None,
        None,
    )
    .await
    .expect("the media upgrade is accepted");
    send_json(
        &mut media,
        json!({ "type": "hello", "role": "media-client", "ticket": ticket }),
    )
    .await;
    let media_ok = next_json_of(&mut media, "hello-ok").await;
    assert_eq!(media_ok["type"], json!("hello-ok"));
}

#[tokio::test]
async fn m2_unlisted_native_viewer_is_closed_1008_with_empty_reason() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(1);
    let (_host_ws, _) = register_host(&server, &host, Some(TOKEN)).await;
    let mut viewer = native_viewer(&server, &pc_id(&host)).await;
    prove_viewer(&mut viewer, &identity(203), None).await;
    assert_eq!(expect_close(&mut viewer).await, (1008, String::new()));
}

#[tokio::test]
async fn m2_browser_viewer_with_a_gate_cookie_is_admitted() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(1);
    let (_host_ws, _) = register_host(&server, &host, Some(TOKEN)).await;
    let cookie = browser_cookie(&server, &env).await;
    let (_ws, ok) = viewer_hello(&server, &pc_id(&host), Some(cookie.as_str()), false).await;
    assert!(ok["ticket"].as_str().is_some_and(|t| !t.is_empty()));
}

#[tokio::test]
async fn m2_origin_without_a_gate_cookie_is_still_closed_1008() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(1);
    let (_host_ws, _) = register_host(&server, &host, Some(TOKEN)).await;
    let path = format!("/ws?pc={}", pc_id(&host));
    let mut ws = connect(server.addr, &path, Some(ORIGIN), None)
        .await
        .expect("the upgrade is accepted so the browser sees a 1008");
    assert_eq!(expect_close(&mut ws).await, (1008, String::new()));
}

#[tokio::test]
async fn m2_admitted_list_survives_a_restart_through_pcs_json() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(1);
    let pc = pc_id(&host);
    let (mut host_ws, _) = register_host(&server, &host, Some(TOKEN)).await;
    let extra = identity(204).id().to_string();
    send_json(
        &mut host_ws,
        json!({
            "type": "admit",
            "viewers": [viewer_identity().id().to_string(), extra],
            "remotePairing": false,
        }),
    )
    .await;
    wait_for_text(&env.state_dir().join("pcs.json"), &extra).await;

    drop(server);
    let server = Server::start(&env, None).await;
    let rows = admin_rows(&server).await;
    assert_eq!(row_for(&rows, &pc)["admitted"], json!(2));
}

#[tokio::test]
async fn m2_admin_listing_shows_the_count_and_no_viewer_ids() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(1);
    let pc = pc_id(&host);
    let (_host_ws, _) = register_host(&server, &host, Some(TOKEN)).await;

    let rows = admin_rows(&server).await;
    assert_eq!(row_for(&rows, &pc)["admitted"], json!(1));
    let body = serde_json::to_string(&rows).unwrap();
    assert!(!body.contains(&viewer_identity().id().to_string()));
}

#[tokio::test]
async fn m2_pairing_over_real_sockets_forwards_auth_and_drops_offers() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(1);
    let (mut host_ws, _) = register_host(&server, &host, Some(TOKEN)).await;
    let pc = pc_id(&host);
    let admit = [9u8; 32];
    send_json(
        &mut host_ws,
        json!({ "type": "pair-open", "admit": b64(&admit) }),
    )
    .await;
    // The pong comes after pair-open is applied, so the window is open before the viewer knocks.
    send_json(&mut host_ws, json!({ "type": "ping" })).await;
    next_json_of(&mut host_ws, "pong").await;

    let stranger = identity(203);
    let mut viewer = native_viewer(&server, &pc).await;
    prove_viewer(&mut viewer, &stranger, Some(&admit)).await;
    let ok = next_json_of(&mut viewer, "hello-ok").await;
    assert_eq!(ok["pairing"], json!(true));
    assert!(ok.get("ticket").is_none());

    // The offer is dropped, so the host's next text frame is the auth-hello.
    send_json(&mut viewer, json!({ "type": "offer", "sdp": "v=0" })).await;
    send_json(&mut viewer, auth_hello_json(&stranger)).await;
    let Frame::Text(text) = next_frame(&mut host_ws).await else {
        panic!("the host receives the auth-hello as text");
    };
    let forwarded: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(forwarded["type"], json!("auth-hello"));

    send_json(&mut host_ws, auth_host_json(&host)).await;
    let back = next_json_of(&mut viewer, "auth-host").await;
    assert_eq!(back["type"], json!("auth-host"));
}

#[tokio::test]
async fn m2_ctl_round_trips_between_host_and_admitted_viewer() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(1);
    let (mut host_ws, _) = register_host(&server, &host, Some(TOKEN)).await;
    let mut viewer = native_viewer(&server, &pc_id(&host)).await;
    prove_viewer(&mut viewer, &viewer_identity(), None).await;
    next_json_of(&mut viewer, "hello-ok").await;

    send_json(
        &mut viewer,
        json!({ "type": "ctl", "kind": "input", "seq": 1, "data": "" }),
    )
    .await;
    let from_viewer = next_json_of(&mut host_ws, "ctl").await;
    assert_eq!(from_viewer["seq"], json!(1));

    send_json(
        &mut host_ws,
        json!({ "type": "ctl", "kind": "input", "seq": 2, "data": "" }),
    )
    .await;
    let from_host = next_json_of(&mut viewer, "ctl").await;
    assert_eq!(from_host["seq"], json!(2));
}
