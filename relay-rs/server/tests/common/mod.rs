//! Shared harness for the e2e tests. Each test boots the real server (`server::run`) on a
//! 127.0.0.1 socket, with an ntfy recorder on another 127.0.0.1 socket, and talks to it with
//! reqwest (HTTP) and tokio-tungstenite (WebSocket). Nothing is mocked on the relay side.

#![allow(dead_code)]

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bp_device_key::{DeviceIdentity, Role};
use bp_relay_server::config::{Config, Ntfy, Secret, Turn};
use bp_relay_server::server::run;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

pub const ORIGIN: &str = "https://relay.test";
pub const TOKEN: &str = "tok-test";
pub const ADMIN: &str = "adm-test-token";
pub const WAIT: Duration = Duration::from_secs(10);

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type Recorder = Arc<Mutex<Vec<Value>>>;

/// One received WebSocket frame, reduced to what the tests check.
#[derive(Debug)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
    Close(u16, String),
    End,
}

/// A temp root with assets/ and state/, plus an ntfy recorder on 127.0.0.1.
pub struct Env {
    pub root: tempfile::TempDir,
    pub ntfy: Recorder,
    ntfy_url: String,
    recorder: JoinHandle<()>,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.recorder.abort();
    }
}

impl Env {
    pub async fn new() -> Env {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("assets")).unwrap();
        fs::write(root.path().join("assets/index.html"), "home").unwrap();
        fs::create_dir_all(root.path().join("state")).unwrap();
        let ntfy: Recorder = Arc::default();
        let app = Router::new()
            .route("/ntfy", post(record))
            .with_state(ntfy.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorder = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Env {
            root,
            ntfy,
            ntfy_url: format!("http://{addr}/ntfy"),
            recorder,
        }
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.path().join("state")
    }

    /// The first ntfy publish the relay sent, once one arrives.
    pub async fn wait_for_ntfy(&self) -> Value {
        timeout(WAIT, async {
            loop {
                let first = self.ntfy.lock().unwrap().first().cloned();
                if let Some(value) = first {
                    return value;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("no ntfy publish arrived")
    }
}

async fn record(State(store): State<Recorder>, body: Bytes) -> StatusCode {
    if let Ok(value) = serde_json::from_slice::<Value>(&body) {
        store.lock().unwrap().push(value);
    }
    StatusCode::OK
}

/// A running relay on 127.0.0.1. Dropping it aborts the server task; the state files stay in
/// the Env's state directory, so a second Server over the same Env is a restart.
pub struct Server {
    pub addr: SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    pub async fn start(env: &Env, turn: Option<Turn>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cfg = Config {
            public_origin: ORIGIN.to_string(),
            listen: addr,
            assets_dir: env.root.path().join("assets"),
            state_dir: env.state_dir(),
            token: Secret::new(TOKEN),
            admin_token: Secret::new(ADMIN),
            max_connections: 32,
            turn,
            ntfy: Some(Ntfy {
                url: env.ntfy_url.clone(),
                topic: "bp".to_string(),
                token: None,
            }),
            approval_ttl_s: 90,
            trusted_proxy_cidrs: Vec::new(),
        };
        let task = tokio::spawn(async move {
            run(&cfg, listener).await.expect("server run");
        });
        Server { addr, task }
    }
}

pub fn base(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

/// A JSON body. reqwest is built without its json feature, so the body is parsed here.
pub async fn read_json(response: reqwest::Response) -> Value {
    let text = response.text().await.unwrap();
    serde_json::from_str(&text).expect("body is JSON")
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// The device identity for a fixed seed byte. No key generation is needed.
pub fn identity(seed: u8) -> DeviceIdentity {
    DeviceIdentity::from_seed(&[seed; 32])
}

pub fn pc_id(identity: &DeviceIdentity) -> String {
    identity.id().to_string()
}

pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Opens a WebSocket. A refused upgrade comes back as Err with the HTTP status inside.
pub async fn connect(
    addr: SocketAddr,
    path: &str,
    origin: Option<&str>,
    cookie: Option<&str>,
) -> Result<Ws, tungstenite::Error> {
    let mut request = format!("ws://{addr}{path}").into_client_request()?;
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("Origin", HeaderValue::from_str(origin).unwrap());
    }
    if let Some(cookie) = cookie {
        request
            .headers_mut()
            .insert("Cookie", HeaderValue::from_str(cookie).unwrap());
    }
    let (ws, _) = connect_async(request).await?;
    Ok(ws)
}

pub fn http_status(err: &tungstenite::Error) -> Option<u16> {
    match err {
        tungstenite::Error::Http(response) => Some(response.status().as_u16()),
        _ => None,
    }
}

pub async fn next_frame(ws: &mut Ws) -> Frame {
    loop {
        let next = timeout(WAIT, ws.next())
            .await
            .expect("timed out waiting for a frame");
        let message = match next {
            None | Some(Err(_)) => return Frame::End,
            Some(Ok(message)) => message,
        };
        return match message {
            Message::Text(text) => {
                let text: &str = &text;
                Frame::Text(text.to_string())
            }
            Message::Binary(bytes) => Frame::Binary(bytes.to_vec()),
            Message::Close(Some(frame)) => {
                let reason: &str = &frame.reason;
                Frame::Close(u16::from(frame.code), reason.to_string())
            }
            Message::Close(None) => Frame::Close(1005, String::new()),
            _ => continue,
        };
    }
}

pub async fn send_json(ws: &mut Ws, value: Value) {
    ws.send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

pub async fn send_binary(ws: &mut Ws, bytes: Vec<u8>) {
    ws.send(Message::Binary(bytes.into())).await.unwrap();
}

/// The next text message whose `type` is `kind`. Other messages are skipped.
pub async fn next_json_of(ws: &mut Ws, kind: &str) -> Value {
    loop {
        match next_frame(ws).await {
            Frame::Text(text) => {
                let value: Value = serde_json::from_str(&text).expect("relay sent JSON");
                if value["type"] == kind {
                    return value;
                }
            }
            other => panic!("socket ended while waiting for {kind}: {other:?}"),
        }
    }
}

pub async fn next_binary(ws: &mut Ws) -> Vec<u8> {
    loop {
        match next_frame(ws).await {
            Frame::Binary(bytes) => return bytes,
            Frame::Text(_) => continue,
            other => panic!("socket ended before a binary frame: {other:?}"),
        }
    }
}

/// Reads until the relay closes the socket. Anything queued before the close is skipped.
pub async fn expect_close(ws: &mut Ws) -> (u16, String) {
    loop {
        match next_frame(ws).await {
            Frame::Close(code, reason) => return (code, reason),
            Frame::End => panic!("socket ended without a close frame"),
            _ => continue,
        }
    }
}

/// A host's signalling socket: hello, register-challenge, register-proof, hello-ok. The
/// `signer` identity both proves the key and names the PC. Returns the socket and hello-ok
/// (its `ticket` and `pc`).
pub async fn register_host(
    server: &Server,
    signer: &DeviceIdentity,
    token: Option<&str>,
) -> (Ws, Value) {
    let pc = pc_id(signer);
    let mut ws = connect(server.addr, &format!("/host-ws?pc={pc}"), None, None)
        .await
        .unwrap();
    let mut hello = json!({
        "type": "hello",
        "role": "host",
        "id": pc,
        "public": b64(&signer.public_key()),
    });
    if let Some(token) = token {
        hello["token"] = json!(token);
    }
    send_json(&mut ws, hello).await;
    let challenge = next_json_of(&mut ws, "register-challenge").await;
    let nonce: [u8; 32] = URL_SAFE_NO_PAD
        .decode(challenge["nonce"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let signature = signer.sign_relay_proof(Role::Host, ORIGIN, &nonce);
    send_json(
        &mut ws,
        json!({ "type": "register-proof", "signature": b64(&signature) }),
    )
    .await;
    let ok = next_json_of(&mut ws, "hello-ok").await;
    (ws, ok)
}

/// A viewer's signalling socket on /ws, with the Origin and gate cookie a browser sends.
pub async fn viewer_hello(
    server: &Server,
    pc: &str,
    cookie: Option<&str>,
    turn: bool,
) -> (Ws, Value) {
    let mut ws = connect(server.addr, &format!("/ws?pc={pc}"), Some(ORIGIN), cookie)
        .await
        .unwrap();
    send_json(
        &mut ws,
        json!({ "type": "hello", "role": "viewer", "turn": turn }),
    )
    .await;
    let ok = next_json_of(&mut ws, "hello-ok").await;
    (ws, ok)
}

/// A media or file data socket: upgrade on `path` with the epoch, then the data hello with the
/// ticket. Viewer-side sockets pass the Origin and cookie; host-side ones pass neither.
pub async fn data_socket(
    server: &Server,
    path: &str,
    pc: &str,
    epoch: &str,
    role: &str,
    ticket: &str,
    viewer_cookie: Option<&str>,
) -> (Ws, Value) {
    let viewer_side = role.ends_with("client");
    let (origin, cookie) = if viewer_side {
        (Some(ORIGIN), viewer_cookie)
    } else {
        (None, None)
    };
    let mut ws = connect(
        server.addr,
        &format!("{path}?pc={pc}&epoch={epoch}"),
        origin,
        cookie,
    )
    .await
    .unwrap();
    send_json(
        &mut ws,
        json!({ "type": "hello", "role": role, "ticket": ticket }),
    )
    .await;
    let ok = next_json_of(&mut ws, "hello-ok").await;
    (ws, ok)
}

/// Runs a browser through the gate: navigation without a cookie, then the ntfy approve
/// bearer, then the poll that trades the poll token for the cookie. Returns the Cookie header
/// value, `__Host-gate=<value>`.
pub async fn browser_cookie(server: &Server, env: &Env) -> String {
    let client = http();
    let page = client
        .get(format!("{}/", base(server.addr)))
        .header("Accept", "text/html")
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Mode", "navigate")
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let poll = token_in_page(&page.text().await.unwrap());
    let ntfy = env.wait_for_ntfy().await;
    let approve = ntfy["actions"][0]["headers"]["Authorization"]
        .as_str()
        .unwrap()
        .strip_prefix("Bearer ")
        .unwrap()
        .to_string();
    let decision = client
        .post(format!("{}/__gate/decision", base(server.addr)))
        .header("Authorization", format!("Bearer {approve}"))
        .body("approve")
        .send()
        .await
        .unwrap();
    assert_eq!(decision.status(), 204);
    let poll_reply = client
        .post(format!("{}/__gate/poll", base(server.addr)))
        .header("Content-Type", "application/json")
        .body(json!({ "pending": poll }).to_string())
        .send()
        .await
        .unwrap();
    let cookie = poll_reply
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("__Host-gate="))
        .expect("approved poll sets the gate cookie")
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let state: Value = read_json(poll_reply).await;
    assert_eq!(state["state"], "approved");
    cookie
}

/// The poll token the wait page carries in its script.
fn token_in_page(html: &str) -> String {
    let start = html
        .find("var token = \"")
        .expect("wait page has a poll token")
        + 13;
    let end = start + html[start..].find('"').unwrap();
    html[start..end].to_string()
}
