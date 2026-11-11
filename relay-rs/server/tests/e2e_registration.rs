//! Real-socket e2e: host registration, the version probe, the admin API and restart persistence.

mod common;

use std::fs;

use bp_device_key::DeviceIdentity;
use common::*;
use serde_json::{Value, json};

fn admin_get(server: &Server, bearer: &str) -> reqwest::RequestBuilder {
    http()
        .get(format!("{}/__gate/manage/pcs", base(server.addr)))
        .header("Authorization", format!("Bearer {bearer}"))
}

#[tokio::test]
async fn e2e_host_hello_registers_and_persists_pc() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(1);
    let (_ws, ok) = register_host(&server, &host, Some(TOKEN)).await;
    assert_eq!(ok["pc"], pc_id(&host));
    assert!(ok["ticket"].as_str().is_some_and(|t| !t.is_empty()));
    let pcs = fs::read_to_string(env.state_dir().join("pcs.json")).unwrap();
    assert!(
        pcs.contains(&pc_id(&host)),
        "pcs.json names the registered PC"
    );
}

#[tokio::test]
async fn e2e_host_reregistration_with_same_key_is_accepted() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(2);
    let (_first, _) = register_host(&server, &host, Some(TOKEN)).await;
    // A known PC needs only its own key; no TOKEN on the second hello.
    let (_second, ok) = register_host(&server, &host, None).await;
    assert_eq!(ok["pc"], pc_id(&host));
}

#[tokio::test]
async fn e2e_host_with_mismatched_public_key_is_refused() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let claimed = identity(3);
    let other: DeviceIdentity = identity(4);
    let mut ws = connect(
        server.addr,
        &format!("/host-ws?pc={}", pc_id(&claimed)),
        None,
        None,
    )
    .await
    .unwrap();
    // The id belongs to one key and the public field names another.
    send_json(
        &mut ws,
        json!({
            "type": "hello",
            "role": "host",
            "id": pc_id(&claimed),
            "public": b64(&other.public_key()),
            "token": TOKEN,
        }),
    )
    .await;
    assert_eq!(
        expect_close(&mut ws).await,
        (1008, "invalid hello".to_string())
    );
}

#[tokio::test]
async fn e2e_unknown_pc_without_token_is_refused() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(5);
    let mut ws = connect(
        server.addr,
        &format!("/host-ws?pc={}", pc_id(&host)),
        None,
        None,
    )
    .await
    .unwrap();
    send_json(
        &mut ws,
        json!({
            "type": "hello",
            "role": "host",
            "id": pc_id(&host),
            "public": b64(&host.public_key()),
        }),
    )
    .await;
    // No challenge: the socket is closed before any register-challenge.
    assert_eq!(
        expect_close(&mut ws).await,
        (1008, "invalid hello".to_string())
    );
    let pcs_file = env.state_dir().join("pcs.json");
    if pcs_file.exists() {
        let pcs = fs::read_to_string(pcs_file).unwrap();
        assert!(!pcs.contains(&pc_id(&host)), "a refused PC is not stored");
    }
}

#[tokio::test]
async fn e2e_unknown_pc_with_wrong_token_is_refused() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(6);
    let mut ws = connect(
        server.addr,
        &format!("/host-ws?pc={}", pc_id(&host)),
        None,
        None,
    )
    .await
    .unwrap();
    send_json(
        &mut ws,
        json!({
            "type": "hello",
            "role": "host",
            "id": pc_id(&host),
            "public": b64(&host.public_key()),
            "token": "not-the-token",
        }),
    )
    .await;
    assert_eq!(
        expect_close(&mut ws).await,
        (1008, "invalid hello".to_string())
    );
}

#[tokio::test]
async fn e2e_version_probe_answers_then_closes_1000() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let pc = pc_id(&identity(7));
    let mut ws = connect(
        server.addr,
        &format!("/host-version-ws?pc={pc}"),
        None,
        None,
    )
    .await
    .unwrap();
    send_json(
        &mut ws,
        json!({ "type": "hello", "role": "version-probe", "token": TOKEN }),
    )
    .await;
    next_json_of(&mut ws, "hello-ok").await;
    assert_eq!(
        expect_close(&mut ws).await,
        (1000, "version-probe-complete".to_string())
    );

    let mut bad = connect(
        server.addr,
        &format!("/host-version-ws?pc={pc}"),
        None,
        None,
    )
    .await
    .unwrap();
    send_json(
        &mut bad,
        json!({ "type": "hello", "role": "version-probe", "token": "wrong" }),
    )
    .await;
    assert_eq!(
        expect_close(&mut bad).await,
        (1008, "invalid hello".to_string())
    );
}

#[tokio::test]
async fn e2e_admin_list_and_delete_closes_host() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let host = identity(8);
    let pc = pc_id(&host);
    let (mut host_ws, _) = register_host(&server, &host, Some(TOKEN)).await;

    // Wrong or missing bearer is a plain 404, the same as an unknown route.
    let no_bearer = http()
        .get(format!("{}/__gate/manage/pcs", base(server.addr)))
        .send()
        .await
        .unwrap();
    assert_eq!(no_bearer.status(), 404);
    assert_eq!(
        admin_get(&server, "wrong").send().await.unwrap().status(),
        404
    );

    let rows: Value = read_json(admin_get(&server, ADMIN).send().await.unwrap()).await;
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == pc.as_str())
        .expect("registered PC is listed");
    assert_eq!(row["online"], true);

    let delete = http()
        .delete(format!("{}/__gate/manage/pcs/{pc}", base(server.addr)))
        .header("Authorization", format!("Bearer {ADMIN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(delete.status(), 204);
    assert_eq!(
        expect_close(&mut host_ws).await,
        (1008, "peer removed".to_string())
    );

    let after: Value = read_json(admin_get(&server, ADMIN).send().await.unwrap()).await;
    assert!(
        after
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["id"] != pc.as_str()),
        "deleted PC is gone from the listing"
    );
    let again = http()
        .delete(format!("{}/__gate/manage/pcs/{pc}", base(server.addr)))
        .header("Authorization", format!("Bearer {ADMIN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 404);
}

#[tokio::test]
async fn e2e_restart_keeps_pcs_and_gate_cookie() {
    let env = Env::new().await;
    let host = identity(9);
    let pc = pc_id(&host);
    let cookie = {
        let first = Server::start(&env, None).await;
        let (_host_ws, _) = register_host(&first, &host, Some(TOKEN)).await;
        browser_cookie(&first, &env).await
        // `first` drops here: the server task is aborted, the state files remain.
    };

    let second = Server::start(&env, None).await;
    let rows: Value = read_json(admin_get(&second, ADMIN).send().await.unwrap()).await;
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == pc.as_str())
        .expect("PC survives the restart");
    assert_eq!(row["online"], false, "no host has reconnected yet");

    let page = http()
        .get(format!("{}/", base(second.addr)))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(page.text().await.unwrap(), "home");
}

#[tokio::test]
async fn e2e_preauth_large_declared_frame_is_closed_before_payload_arrives() {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    use tokio_tungstenite::tungstenite::Message;

    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let path = format!("/host-ws?pc={}", pc_id(&identity(91)));
    let mut ws = connect(server.addr, &path, None, None).await.unwrap();
    // A valid masked binary frame declares 1 MiB but sends no payload. A
    // receiver that waits to buffer the frame cannot finish this test.
    let mut header = vec![0x82, 0xff];
    header.extend_from_slice(&(1024_u64 * 1024).to_be_bytes());
    header.extend_from_slice(&[1, 2, 3, 4]);
    ws.get_mut().write_all(&header).await.unwrap();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(1), ws.next())
        .await
        .expect("pre-auth frame limit must reject the header before reading the payload");
    assert!(
        matches!(closed, None | Some(Err(_)) | Some(Ok(Message::Close(_)))),
        "got {closed:?}"
    );
}

#[tokio::test]
async fn e2e_http_header_and_body_reads_have_ten_second_deadlines() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let mut headers = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    let mut body = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    headers
        .write_all(b"GET / HTTP/1.1\r\nHost: relay.test\r\n")
        .await
        .unwrap();
    body.write_all(
        b"POST /__gate/decision HTTP/1.1\r\nHost: relay.test\r\nContent-Length: 10\r\n\r\nx",
    )
    .await
    .unwrap();
    let (header_response, body_response) =
        tokio::time::timeout(std::time::Duration::from_secs(12), async {
            let mut first = Vec::new();
            let _ = headers.read_to_end(&mut first).await;
            let mut second = [0u8; 4096];
            let count = body.read(&mut second).await.unwrap();
            (first, second[..count].to_vec())
        })
        .await
        .expect("incomplete headers and bodies must expire at10s");
    assert!(
        header_response.is_empty()
            || String::from_utf8_lossy(&header_response).starts_with("HTTP/1.1 408")
    );
    assert!(String::from_utf8_lossy(&body_response).starts_with("HTTP/1.1 408"));
}
