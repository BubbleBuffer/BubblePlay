//! A synthetic relay for session tests. It serves `/ws` and `/media-ws` on
//! 127.0.0.1 from a script and logs every handshake and text message it gets.
//!
//! `/ws` runs the phase 2 viewer admission: hello, `register-challenge` with
//! [`FAKE_NONCE`], `register-proof` (checked with `verify_relay_proof` against the
//! relay's origin; a bad proof closes 1008), then `hello-ok` with [`FAKE_TICKET`].
//! `/media-ws` answers any hello; the tests assert its ticket.

use std::future::pending;
use std::net::TcpListener as StdListener;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bp_device_key::{verify_relay_proof, Role};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::WebSocketStream;

type ServerSocket = WebSocketStream<TcpStream>;

/// The `register-challenge` nonce the fake relay sends.
pub(crate) const FAKE_NONCE: [u8; 32] = [0x5a; 32];
/// The media ticket in the fake relay's `/ws` hello-ok (32 bytes of base64url).
pub(crate) const FAKE_TICKET: &str = "4OHi4-Tl5ufo6err7O3u7_Dx8vP09fb3-Pn6-_z9_v8";

/// What one accepted connection does. Connections use scenarios in order; the
/// last one repeats.
#[derive(Clone, Default)]
pub(crate) struct Scenario {
    /// `mediaEpoch` sent in the `/ws` hello-ok.
    pub epoch: String,
    /// Answer the `/ws` hello with `{"type":"error","message":...}` and close.
    pub signal_error: Option<String>,
    /// Refuse the WebSocket upgrade with this HTTP status.
    pub refuse: Option<u16>,
    /// Stop answering on `/ws` after hello-ok, keeping the socket open.
    pub signal_silent: bool,
    /// Refuse the viewer after its proof (close 1008, as for a key not admitted).
    pub refuse_viewer: bool,
    /// Value of `present` in the `media-peer` sent after the media hello-ok.
    pub host_present: bool,
    /// Sent on `/media-ws`, in order, after the media hello-ok and `media-peer`.
    pub media: Vec<Step>,
    /// Stop answering on `/media-ws` after `media`, keeping the socket open.
    pub media_silent: bool,
}

impl Scenario {
    pub(crate) fn new(epoch: &str) -> Self {
        Self {
            epoch: epoch.to_owned(),
            ..Self::default()
        }
    }
}

#[derive(Clone)]
pub(crate) enum Step {
    Binary(Vec<u8>),
    Text(String),
    Pause(Duration),
}

/// What the relay saw from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Logged {
    /// A WebSocket upgrade request (accepted or refused).
    Connect {
        path: String,
        query: String,
        origin: Option<String>,
        cookie: Option<String>,
    },
    /// A text message from the client on `path`.
    Text { path: String, text: String },
    /// A binary message from the client (never expected).
    Binary { path: String },
}

pub(crate) struct FakeRelay {
    /// The `http://127.0.0.1:<port>` origin to put in the config.
    pub origin: String,
    pub log: Receiver<Logged>,
    stop: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl FakeRelay {
    /// Start an owned relay. Drop stops and joins the listener and all socket
    /// tasks, including when a test panics.
    pub(crate) fn start(scenarios: Vec<Scenario>) -> FakeRelay {
        assert!(
            !scenarios.is_empty(),
            "a fake relay needs at least one scenario"
        );
        let listener = StdListener::bind("127.0.0.1:0").expect("bind loopback");
        let origin = format!("http://{}", listener.local_addr().expect("local addr"));
        listener.set_nonblocking(true).expect("nonblocking");
        let (log_tx, log_rx) = mpsc::channel();
        let state = Arc::new(State {
            origin: origin.clone(),
            scenarios,
            ws_count: Mutex::new(0),
            last_ws: Mutex::new(Scenario::default()),
        });
        let (stop, mut stopped) = oneshot::channel();
        let listener_thread = thread::spawn(move || {
            // Current-thread: block_on drives the spawned connection tasks too.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("relay runtime");
            runtime.block_on(async move {
                let listener = TcpListener::from_std(listener).expect("tokio listener");
                let mut tasks = JoinSet::new();
                loop {
                    tokio::select! {
                        biased;
                        _ = &mut stopped => break,
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { break };
                            tasks.spawn(handle(stream, state.clone(), log_tx.clone()));
                        }
                        result = tasks.join_next(), if !tasks.is_empty() => {
                            if let Some(Err(error)) = result { panic!("fake relay task failed: {error}"); }
                        }
                    }
                }
                // Cancel pending handshakes, silent sockets and scripted pauses,
                // then wait for every owned task to be dropped.
                tasks.shutdown().await;
            });
        });
        FakeRelay {
            origin,
            log: log_rx,
            stop: Some(stop),
            handle: Some(listener_thread),
        }
    }

    pub(crate) fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.handle.take() {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if !handle.is_finished() {
                if thread::panicking() {
                    eprintln!("fake relay did not stop within five seconds");
                    return;
                }
                panic!("fake relay did not stop within five seconds");
            }
            if let Err(error) = handle.join() {
                if !thread::panicking() {
                    std::panic::resume_unwind(error);
                }
            }
        }
    }
}

impl Drop for FakeRelay {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct State {
    origin: String,
    scenarios: Vec<Scenario>,
    ws_count: Mutex<usize>,
    last_ws: Mutex<Scenario>,
}

impl State {
    /// `/ws` takes the next scenario. `/media-ws` uses the scenario of the latest `/ws`.
    fn pick(&self, path: &str) -> Scenario {
        if path == "/ws" {
            let mut count = self.ws_count.lock().unwrap();
            let index = (*count).min(self.scenarios.len() - 1);
            *count += 1;
            let scenario = self.scenarios[index].clone();
            *self.last_ws.lock().unwrap() = scenario.clone();
            scenario
        } else {
            self.last_ws.lock().unwrap().clone()
        }
    }
}

async fn handle(stream: TcpStream, state: Arc<State>, log: Sender<Logged>) {
    stream
        .set_nodelay(true)
        .expect("low-latency fake relay socket");
    let plan: Arc<Mutex<Option<(String, Scenario)>>> = Arc::new(Mutex::new(None));
    let slot = plan.clone();
    let callback_log = log.clone();
    // Tungstenite fixes this callback's error type to its HTTP response.
    #[allow(clippy::result_large_err)]
    let accepted = accept_hdr_async(stream, move |request: &Request, response: Response| {
        let path = request.uri().path().to_owned();
        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let _ = callback_log.send(Logged::Connect {
            path: path.clone(),
            query: request.uri().query().unwrap_or("").to_owned(),
            origin: header("origin"),
            cookie: header("cookie"),
        });
        let scenario = state.pick(&path);
        if let Some(status) = scenario.refuse {
            let refused: ErrorResponse = tungstenite::http::Response::builder()
                .status(status)
                .body(Some("refused".to_owned()))
                .expect("refusal response");
            return Err(refused);
        }
        *slot.lock().unwrap() = Some((path, scenario));
        Ok(response)
    })
    .await;
    let Ok(ws) = accepted else { return };
    let origin = state.origin.clone();
    let Some((path, scenario)) = plan.lock().unwrap().take() else {
        return;
    };
    match path.as_str() {
        "/ws" => serve_signal(ws, scenario, &origin, log).await,
        "/media-ws" => serve_media(ws, scenario, log).await,
        _ => {}
    }
}

/// Next text message from the client. Binary messages are logged and skipped.
async fn read_text(ws: &mut ServerSocket, path: &str, log: &Sender<Logged>) -> Option<String> {
    loop {
        match ws.next().await? {
            Ok(Message::Text(text)) => {
                let text = text.as_str().to_owned();
                let _ = log.send(Logged::Text {
                    path: path.to_owned(),
                    text: text.clone(),
                });
                return Some(text);
            }
            Ok(Message::Binary(_)) => {
                let _ = log.send(Logged::Binary {
                    path: path.to_owned(),
                });
            }
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

async fn send_json(ws: &mut ServerSocket, value: Value) {
    let _ = ws.send(Message::Text(value.to_string().into())).await;
}

fn message_type(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    value.get("type")?.as_str().map(str::to_owned)
}

/// Whether `proof` answers [`FAKE_NONCE`] for the key in `hello` at `origin`.
fn proof_verifies(hello: &str, proof: &str, origin: &str) -> bool {
    let (Ok(hello), Ok(proof)) = (
        serde_json::from_str::<Value>(hello),
        serde_json::from_str::<Value>(proof),
    ) else {
        return false;
    };
    let decode = |value: &Value| URL_SAFE_NO_PAD.decode(value.as_str()?).ok();
    let (Some(id), Some(public), Some(signature)) = (
        hello["id"].as_str(),
        decode(&hello["public"]),
        decode(&proof["signature"]),
    ) else {
        return false;
    };
    hello["role"] == "viewer"
        && proof["type"] == "register-proof"
        && verify_relay_proof(Role::Viewer, origin, &FAKE_NONCE, id, &public, &signature).is_ok()
}

async fn close_policy(ws: &mut ServerSocket) {
    let _ = ws
        .send(Message::Close(Some(CloseFrame {
            code: CloseCode::Policy,
            reason: "".into(),
        })))
        .await;
}

async fn serve_signal(mut ws: ServerSocket, scenario: Scenario, origin: &str, log: Sender<Logged>) {
    let path = "/ws";
    let Some(hello) = read_text(&mut ws, path, &log).await else {
        return;
    };
    if let Some(message) = &scenario.signal_error {
        send_json(&mut ws, json!({ "type": "error", "message": message })).await;
        return;
    }
    send_json(
        &mut ws,
        json!({ "type": "register-challenge", "nonce": URL_SAFE_NO_PAD.encode(FAKE_NONCE) }),
    )
    .await;
    let Some(proof) = read_text(&mut ws, path, &log).await else {
        return;
    };
    if scenario.refuse_viewer || !proof_verifies(&hello, &proof, origin) {
        close_policy(&mut ws).await;
        return;
    }
    send_json(
        &mut ws,
        json!({
            "type": "hello-ok",
            "pc": "77VH-7M56-8GJK-Y12J",
            "mediaEpoch": scenario.epoch,
            "iceServers": [],
            "ticket": FAKE_TICKET,
        }),
    )
    .await;
    if scenario.signal_silent {
        let _held = ws;
        pending::<()>().await;
        return;
    }
    while let Some(text) = read_text(&mut ws, path, &log).await {
        if message_type(&text).as_deref() == Some("ping") {
            send_json(&mut ws, json!({ "type": "pong" })).await;
        }
    }
}

async fn serve_media(mut ws: ServerSocket, scenario: Scenario, log: Sender<Logged>) {
    let path = "/media-ws";
    if read_text(&mut ws, path, &log).await.is_none() {
        return;
    }
    send_json(&mut ws, json!({ "type": "hello-ok" })).await;
    send_json(
        &mut ws,
        json!({ "type": "media-peer", "present": scenario.host_present }),
    )
    .await;
    for step in &scenario.media {
        match step {
            Step::Binary(bytes) => {
                let _ = ws.send(Message::Binary(bytes.clone().into())).await;
            }
            Step::Text(text) => {
                let _ = ws.send(Message::Text(text.clone().into())).await;
            }
            Step::Pause(duration) => sleep(*duration).await,
        }
    }
    if scenario.media_silent {
        let _held = ws;
        pending::<()>().await;
        return;
    }
    while let Some(text) = read_text(&mut ws, path, &log).await {
        if message_type(&text).as_deref() == Some("ping") {
            send_json(&mut ws, json!({ "type": "pong" })).await;
        }
    }
}
