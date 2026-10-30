//! Real-socket e2e for `GET /presence`: it follows a registered host's socket, and a cookieless
//! query (with or without `Sec-Fetch-Dest`) never pushes ntfy, holds, or sets a cookie, even with
//! ntfy configured.

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};
use tokio::time::sleep;

async fn presence(server: &Server, ids: &str, dest: Option<&str>) -> reqwest::Response {
    let mut request = http().get(format!("{}/presence?ids={ids}", base(server.addr)));
    if let Some(dest) = dest {
        request = request.header("Sec-Fetch-Dest", dest);
    }
    request.send().await.unwrap()
}

async fn presence_json(server: &Server, ids: &str) -> Value {
    let response = presence(server, ids, None).await;
    assert_eq!(response.status(), 200);
    read_json(response).await
}

#[tokio::test]
async fn presence_follows_the_host_socket() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let pc = pc_id(&identity(1));
    let unknown = pc_id(&identity(2));
    let ids = format!("{pc},{unknown}");

    assert_eq!(
        presence_json(&server, &ids).await,
        json!({ pc.clone(): false, unknown.clone(): false })
    );
    let (host_ws, _) = register_host(&server, &identity(1), Some(TOKEN)).await;
    assert_eq!(
        presence_json(&server, &ids).await,
        json!({ pc.clone(): true, unknown.clone(): false })
    );
    drop(host_ws);
    // The relay sees the close asynchronously.
    let deadline = Instant::now() + WAIT;
    loop {
        let body = presence_json(&server, &ids).await;
        if body == json!({ pc.clone(): false, unknown.clone(): false }) {
            break;
        }
        assert!(Instant::now() < deadline, "host still online: {body}");
        sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn presence_never_pushes_ntfy_holds_or_sets_cookies() {
    let env = Env::new().await;
    let server = Server::start(&env, None).await;
    let pc = pc_id(&identity(1));
    for dest in [None, Some("document"), Some("empty")] {
        let started = Instant::now();
        let response = presence(&server, &pc, dest).await;
        assert_eq!(response.status(), 200, "dest {dest:?}");
        assert!(response.headers().get("set-cookie").is_none());
        assert_eq!(response.headers()["cache-control"], "no-store");
        // Answered at once, not held.
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    // Give a stray publish time to land, then check none did.
    sleep(Duration::from_millis(300)).await;
    assert!(env.ntfy.lock().unwrap().is_empty(), "presence pushed ntfy");
    // A real navigation from the same client still opens an approval: no cooldown or pending
    // was created by the presence queries.
    let navigation = tokio::spawn(navigate(&server, "/").send());
    let published = env.wait_for_ntfy().await;
    assert!(published["actions"].is_array());
    navigation.abort();
}
