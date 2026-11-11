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
use bp_relay_core::gate::GATE_HOLD_MS;
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
    wait_cookie: Mutex<Option<String>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    pub async fn start(env: &Env, turn: Option<Turn>) -> Server {
        Server::start_with_hold(env, turn, GATE_HOLD_MS).await
    }

    /// A relay whose gate holds an unapproved navigation for `gate_hold_ms` instead of the 30 s.
    pub async fn start_with_hold(env: &Env, turn: Option<Turn>, gate_hold_ms: u64) -> Server {
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
            max_unauthenticated_connections: 16,
            max_unauthenticated_per_prefix: 4,
            upgrade_rate_per_prefix: 30,
            ntfy_publish_limit: 10,
            ntfy_publish_window_s: 600,
            auth_failure_limit: 5,
            auth_failure_window_s: 600,

            turn,
            ntfy: Some(Ntfy {
                url: env.ntfy_url.clone(),
                topic: "bp".to_string(),
                token: None,
            }),
            approval_ttl_s: 90,
            trusted_proxy_cidrs: Vec::new(),
            gate_hold_ms,
        };
        let task = tokio::spawn(async move {
            run(&cfg, listener).await.expect("server run");
        });
        Server {
            addr,
            task,
            wait_cookie: Mutex::new(None),
        }
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
#[allow(clippy::result_large_err)] // the tests read the refusal status from tungstenite's error
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

/// Asserts that nothing arrives on `ws` for `quiet`. A frame, or the socket ending,
/// fails the test.
pub async fn expect_quiet(ws: &mut Ws, quiet: Duration) {
    if let Ok(next) = timeout(quiet, ws.next()).await {
        panic!("expected silence, got {next:?}");
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
    // Every test host admits the fixed viewer identity, as a real host publishes its list.
    send_json(
        &mut ws,
        json!({
            "type": "admit",
            "viewers": [viewer_identity().id().to_string()],
            "remotePairing": false,
        }),
    )
    .await;
    // The relay handles this socket's frames in order. The pong means the admit has been applied,
    // so a viewer that connects next is not refused by a race.
    send_json(&mut ws, json!({ "type": "ping" })).await;
    next_json_of(&mut ws, "pong").await;
    (ws, ok)
}

/// The viewer identity the e2e helpers sign with. Test hosts admit it.
pub const VIEWER_SEED: u8 = 200;

pub fn viewer_identity() -> DeviceIdentity {
    identity(VIEWER_SEED)
}

/// Sends the signed viewer hello for `who` (with an optional pairing value), answers the
/// register-challenge, and returns the hello-ok or refusal the relay sends next.
pub async fn viewer_join_signed(
    ws: &mut Ws,
    who: &DeviceIdentity,
    turn: bool,
    pair: Option<&[u8; 32]>,
) -> Value {
    viewer_join_as(ws, "viewer", who, turn, pair).await
}

/// `viewer_join_signed` with `role` as the hello's role string: the viewer's new name, or the old
/// one it replaces. The proof is the same for both.
pub async fn viewer_join_as(
    ws: &mut Ws,
    role: &str,
    who: &DeviceIdentity,
    turn: bool,
    pair: Option<&[u8; 32]>,
) -> Value {
    let mut hello = json!({
        "type": "hello",
        "role": role,
        "id": who.id().to_string(),
        "public": b64(&who.public_key()),
        "turn": turn,
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
    next_json_of(ws, "hello-ok").await
}

/// A viewer's signalling socket on /ws, with the Origin and gate cookie a browser sends. It
/// signs the hello with the fixed viewer identity, which the host admits.
pub async fn viewer_hello(
    server: &Server,
    pc: &str,
    cookie: Option<&str>,
    turn: bool,
) -> (Ws, Value) {
    let mut ws = connect(server.addr, &format!("/ws?pc={pc}"), Some(ORIGIN), cookie)
        .await
        .unwrap();
    let ok = viewer_join_signed(&mut ws, &viewer_identity(), turn, None).await;
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
    let viewer_side = role.ends_with("viewer");
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

/// A top-level navigation to `path`, retaining this browser's waiting cookie.
pub fn navigate(server: &Server, path: &str) -> reqwest::RequestBuilder {
    let request = http()
        .get(format!("{}{path}", base(server.addr)))
        .header("Accept", "text/html")
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Mode", "navigate");
    match server.wait_cookie.lock().unwrap().as_ref() {
        Some(cookie) => request.header("Cookie", cookie),
        None => request,
    }
}

/// The initial response sets the pending browser secret before any request
/// can enter the silent hold. Keep it for this test browser's later reloads.
pub async fn bootstrap_navigation(server: &Server, path: &str) {
    let response = navigate(server, path).send().await.unwrap();
    assert_eq!(response.status(), 302);
    let set = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(set.starts_with("__Host-bp_wait="));
    for attribute in ["Secure", "HttpOnly", "SameSite=Strict", "Path=/"] {
        assert!(set.contains(attribute), "{attribute}");
    }
    *server.wait_cookie.lock().unwrap() = Some(set.split(';').next().unwrap().to_string());
}

pub fn waiting_cookie(server: &Server) -> Option<String> {
    server.wait_cookie.lock().unwrap().clone()
}

/// The approve (index 0) or deny (index 1) bearer of an ntfy publish.
pub fn ntfy_bearer(ntfy: &Value, index: usize) -> String {
    ntfy["actions"][index]["headers"]["Authorization"]
        .as_str()
        .unwrap()
        .strip_prefix("Bearer ")
        .unwrap()
        .to_string()
}

/// POSTs a decision with `bearer` and `action` and checks the 204.
pub async fn decide(server: &Server, bearer: &str, action: &str) {
    let decision = http()
        .post(format!("{}/__gate/decision", base(server.addr)))
        .header("Authorization", format!("Bearer {bearer}"))
        .body(action.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(decision.status(), 204);
}

/// The `__Host-gate=<value>` pair a response sets, if any.
pub fn gate_cookie_of(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("__Host-gate="))
        .map(|value| value.split(';').next().unwrap().to_string())
}

/// Runs a browser through the bootstrap redirect and cookie-bearing hold,
/// approves it through ntfy, and returns the `__Host-gate=<value>` cookie pair.
pub async fn browser_cookie(server: &Server, env: &Env) -> String {
    bootstrap_navigation(server, "/").await;
    let held = tokio::spawn(navigate(server, "/").send());
    let ntfy = env.wait_for_ntfy().await;
    decide(server, &ntfy_bearer(&ntfy, 0), "approve").await;
    let admitted = timeout(Duration::from_secs(10), held)
        .await
        .expect("the held navigation is answered after the approval")
        .unwrap()
        .unwrap();
    assert_eq!(admitted.status(), 302);
    assert_eq!(
        admitted.headers().get("location").unwrap(),
        &format!("{ORIGIN}/")
    );
    gate_cookie_of(&admitted).expect("the admitted navigation sets the gate cookie")
}

/// Sends a raw HTTP/1.1 navigation with the supplied waiting cookie and reads until the server closes the
/// connection. Returns every byte the server wrote, and how long it took.
pub async fn raw_navigation(
    addr: SocketAddr,
    path: &str,
    cookie: Option<String>,
) -> (Vec<u8>, Duration) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let started = std::time::Instant::now();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let cookie = cookie
        .map(|value| format!("Cookie: {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: text/html\r\n\
         Sec-Fetch-Dest: document\r\nSec-Fetch-Mode: navigate\r\n{cookie}\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut received = Vec::new();
    // A reset counts as a close: what was read before it is kept.
    let _ = timeout(Duration::from_secs(40), stream.read_to_end(&mut received))
        .await
        .expect("the server ends the connection");
    (received, started.elapsed())
}
