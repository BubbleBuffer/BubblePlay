//! Local socket contracts for media registration, feedback, receipt timing and login.
//! Every listener, connection task and client thread belongs to a test guard.

use std::net::TcpListener as StdListener;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc as async_mpsc;
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_hdr_async, WebSocketStream};

use super::protocol::{encode_frame, FrameHeader};
use super::session::{run_with_timing, Timing};
use super::{
    login_with_cookies, Codec, CookieJar, DecoderFeedback, LoginRequest, NetConfig, NetError,
    NetEvent, TransportStats, VideoFrame, GATE_COOKIE, SESSION_COOKIE,
};

const EPOCH: &str = "0123456789abcdef0123456789abcdef";
const WAIT: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(5);
type Socket = WebSocketStream<TcpStream>;

#[derive(Debug)]
enum Seen {
    Text(&'static str, Value),
    Ready(&'static str),
    RejectedStart,
    Login(String),
}

struct RelayState {
    stop: AtomicBool,
    release_second: AtomicBool,
    hello_sent: AtomicBool,
    peer_sent: AtomicBool,
    started: AtomicBool,
    peer_first: bool,
    log: Sender<Seen>,
}

struct OwnedRelay {
    origin: String,
    state: Arc<RelayState>,
    log: Receiver<Seen>,
    send: async_mpsc::UnboundedSender<Vec<u8>>,
    handle: Option<JoinHandle<()>>,
}

impl OwnedRelay {
    fn new(peer_first: bool) -> Self {
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let (log_tx, log) = mpsc::channel();
        let (send, media_rx) = async_mpsc::unbounded_channel();
        let state = Arc::new(RelayState {
            stop: AtomicBool::new(false),
            release_second: AtomicBool::new(false),
            hello_sent: AtomicBool::new(false),
            peer_sent: AtomicBool::new(false),
            started: AtomicBool::new(false),
            peer_first,
            log: log_tx,
        });
        let worker_state = state.clone();
        let handle = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = TcpListener::from_std(listener).unwrap();
                let media_rx = Arc::new(tokio::sync::Mutex::new(Some(media_rx)));
                let mut tasks = JoinSet::new();
                while !worker_state.stop.load(SeqCst) {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.unwrap();
                            tasks.spawn(serve_connection(stream, worker_state.clone(), media_rx.clone()));
                        }
                        _ = tokio::time::sleep(TICK) => {}
                        result = tasks.join_next(), if !tasks.is_empty() => {
                            if let Some(Err(error)) = result { panic!("local relay task failed: {error}"); }
                        }
                    }
                }
                // Abort socket reads/handshakes as well as idle tasks, then join them.
                tasks.shutdown().await;
            });
        });
        Self {
            origin,
            state,
            log,
            send,
            handle: Some(handle),
        }
    }

    fn shutdown(&mut self) {
        self.state.stop.store(true, SeqCst);
        if let Some(handle) = self.handle.take() {
            join_owned(handle);
        }
    }
}

impl Drop for OwnedRelay {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn join_owned<T>(handle: JoinHandle<T>) -> Option<T> {
    let deadline = Instant::now() + WAIT;
    while !handle.is_finished() && Instant::now() < deadline {
        thread::sleep(TICK);
    }
    if !handle.is_finished() {
        // Avoid a second panic while unwinding; the failure is still visible.
        if thread::panicking() {
            eprintln!("owned local test thread did not stop within {WAIT:?}");
            return None;
        }
        panic!("owned local test thread did not stop within {WAIT:?}");
    }
    match handle.join() {
        Ok(value) => Some(value),
        Err(error) if !thread::panicking() => std::panic::resume_unwind(error),
        Err(_) => None,
    }
}

async fn serve_connection(
    stream: TcpStream,
    state: Arc<RelayState>,
    media_rx: Arc<tokio::sync::Mutex<Option<async_mpsc::UnboundedReceiver<Vec<u8>>>>>,
) {
    let mut prefix = [0u8; 4];
    if stream.peek(&mut prefix).await.unwrap_or(0) == 4 && &prefix == b"POST" {
        serve_login(stream, &state).await;
        return;
    }
    let path_slot = Arc::new(std::sync::Mutex::new(String::new()));
    let capture = path_slot.clone();
    let ws = accept_hdr_async(stream, move |request: &Request, response: Response| {
        *capture.lock().unwrap() = request.uri().path().to_owned();
        if request.uri().path() == "/media-ws" {
            assert_eq!(
                request.uri().query(),
                Some(format!("epoch={EPOCH}").as_str())
            );
        }
        Ok(response)
    })
    .await;
    let Ok(ws) = ws else { return };
    let path = path_slot.lock().unwrap().clone();
    serve_socket(ws, path, state, media_rx).await;
}

async fn serve_socket(
    mut ws: Socket,
    path: String,
    state: Arc<RelayState>,
    media_rx: Arc<tokio::sync::Mutex<Option<async_mpsc::UnboundedReceiver<Vec<u8>>>>>,
) {
    // The handlers operate on any socket owner; keep all connection work in this task.
    if path == "/ws" {
        let Some(hello) = socket_json(&mut ws, "/ws", &state).await else {
            return;
        };
        assert_eq!(hello, json!({"type":"hello", "role":"client"}));
        socket_send(&mut ws, json!({"type":"hello-ok", "mediaEpoch":EPOCH})).await;
        while let Some(message) = socket_json(&mut ws, "/ws", &state).await {
            if message["type"] == "media-control" && message["action"] == "start" {
                if !state.hello_sent.load(SeqCst) || !state.peer_sent.load(SeqCst) {
                    let _ = state.log.send(Seen::RejectedStart);
                    socket_send(
                        &mut ws,
                        json!({"type":"error", "message":"start before media registration"}),
                    )
                    .await;
                    return;
                }
                assert_eq!(message["epoch"], EPOCH);
                state.started.store(true, SeqCst);
            } else if message["type"] == "ping" {
                socket_send(&mut ws, json!({"type":"pong"})).await;
            }
        }
    } else if path == "/media-ws" {
        let Some(hello) = socket_json(&mut ws, "/media-ws", &state).await else {
            return;
        };
        assert_eq!(hello, json!({"type":"hello", "role":"media-client"}));
        let first = if state.peer_first {
            "media-peer"
        } else {
            "hello-ok"
        };
        let second = if state.peer_first {
            "hello-ok"
        } else {
            "media-peer"
        };
        ready(&mut ws, first, &state).await;
        while !state.release_second.load(SeqCst) {
            tokio::time::sleep(TICK).await;
        }
        ready(&mut ws, second, &state).await;
        let mut commands = media_rx
            .lock()
            .await
            .take()
            .expect("one media connection per test");
        loop {
            tokio::select! {
                packet = commands.recv() => {
                    let Some(packet) = packet else { return };
                    assert!(state.started.load(SeqCst), "test sends frames only after start");
                    if ws.send(Message::Binary(packet.into())).await.is_err() { return; }
                }
                message = socket_json(&mut ws, "/media-ws", &state) => {
                    let Some(message) = message else { return };
                    if message["type"] == "ping" { socket_send(&mut ws, json!({"type":"pong"})).await; }
                }
            }
        }
    } else {
        panic!("unexpected local socket path {path}");
    }
}

async fn socket_send(ws: &mut Socket, value: Value) {
    ws.send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn socket_json(ws: &mut Socket, path: &'static str, state: &RelayState) -> Option<Value> {
    loop {
        match ws.next().await? {
            Ok(Message::Text(text)) => {
                let value: Value = serde_json::from_str(text.as_str()).unwrap();
                let _ = state.log.send(Seen::Text(path, value.clone()));
                return Some(value);
            }
            Ok(Message::Close(_)) | Err(_) => return None,
            _ => {}
        }
    }
}

async fn ready(ws: &mut Socket, kind: &'static str, state: &RelayState) {
    let value = if kind == "hello-ok" {
        json!({"type":kind})
    } else {
        json!({"type":kind,"present":true})
    };
    socket_send(ws, value).await;
    if kind == "hello-ok" {
        state.hello_sent.store(true, SeqCst);
    } else {
        state.peer_sent.store(true, SeqCst);
    }
    let _ = state.log.send(Seen::Ready(kind));
}

async fn serve_login(mut stream: TcpStream, state: &RelayState) {
    let mut bytes = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0, "HTTP request ended early");
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() < 16384, "bounded synthetic login request");
        let text = String::from_utf8_lossy(&bytes);
        if let Some(end) = text.find("\r\n\r\n") {
            let length: usize = text[..end]
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            if bytes.len() >= end + 4 + length {
                break;
            }
        }
    }
    let request = String::from_utf8(bytes).unwrap();
    let _ = state.log.send(Seen::Login(request));
    stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\nSet-Cookie: __Host-gate=rotated-gate; Path=/; Secure\r\nSet-Cookie: __Host-bp-session=rotated-session; Path=/; Secure\r\n\r\n").await.unwrap();
}

struct Client {
    relay: OwnedRelay,
    events: Receiver<NetEvent>,
    feedback: Sender<DecoderFeedback>,
    stop: Arc<AtomicBool>,
    stats: Arc<TransportStats>,
    handle: Option<JoinHandle<Result<(), NetError>>>,
    seen: Vec<Seen>,
}

impl Client {
    fn new(peer_first: bool, capacity: usize) -> Self {
        let relay = OwnedRelay::new(peer_first);
        // Exercise ws origin normalization as well as the local HTTP relay.
        let config = NetConfig::new(
            relay.origin.replacen("http://", "ws://", 1),
            Codec::H264,
            CookieJar::new(),
        );
        let (event_tx, events) = mpsc::sync_channel(capacity);
        let (feedback, feedback_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(TransportStats::new());
        let (worker_stop, worker_stats) = (stop.clone(), stats.clone());
        let handle = thread::spawn(move || {
            run_with_timing(
                config,
                event_tx,
                feedback_rx,
                worker_stop,
                worker_stats,
                Timing {
                    tick: TICK,
                    handshake: WAIT,
                    heartbeat: Duration::from_secs(10),
                    dead: Duration::from_secs(20),
                    first_frame: Duration::from_secs(20),
                    keyframe_interval: Duration::from_millis(250),
                    backoff_min: TICK,
                    backoff_max: TICK,
                },
            )
        });
        Self {
            relay,
            events,
            feedback,
            stop,
            stats,
            handle: Some(handle),
            seen: Vec::new(),
        }
    }

    fn collect_for(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.relay.log.recv_timeout(left) {
                Ok(item) => self.seen.push(item),
                Err(_) => break,
            }
        }
    }

    fn wait_seen(&mut self, predicate: impl Fn(&Seen) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !self.seen.iter().any(&predicate) {
            let left = deadline
                .checked_duration_since(Instant::now())
                .expect("local relay event timed out");
            self.seen.push(
                self.relay
                    .log
                    .recv_timeout(left)
                    .expect("local relay event"),
            );
        }
    }

    fn register(&mut self) {
        self.wait_seen(|item| matches!(item, Seen::Ready(_)));
        self.relay.state.release_second.store(true, SeqCst);
        self.wait_seen(|item| matches!(item, Seen::Text("/ws", v) if v["phase"] == "request"));
    }

    fn packet(&self, key: bool, generation: u32, seq: u32) -> Instant {
        let sent_at = Instant::now();
        self.relay
            .send
            .send(encode_frame(
                &FrameHeader {
                    key,
                    refine: false,
                    codec: Codec::H264,
                    generation,
                    seq,
                    capture_ts_us: 999_000,
                    width: 640,
                    height: 360,
                },
                b"synthetic-annex-b",
            ))
            .unwrap();
        sent_at
    }

    fn frame(&self) -> VideoFrame {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .expect("local video frame timed out");
            if let NetEvent::Frame(frame) = self.events.recv_timeout(left).expect("net event") {
                return frame;
            }
        }
    }

    fn presentations(&self) -> Vec<&Value> {
        self.seen
            .iter()
            .filter_map(|seen| match seen {
                Seen::Text("/ws", v) if v["type"] == "first-frame" && v["phase"] == "presented" => {
                    Some(v)
                }
                _ => None,
            })
            .collect()
    }

    fn finish(mut self) {
        self.stop.store(true, SeqCst);
        let result = join_owned(self.handle.take().unwrap()).expect("client join");
        self.relay.shutdown();
        assert_eq!(result, Ok(()));
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, SeqCst);
        // Stop the relay first too, so any pending socket await is interrupted.
        self.relay.shutdown();
        if let Some(handle) = self.handle.take() {
            let _ = join_owned(handle);
        }
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "local client condition timed out"
        );
        thread::sleep(TICK);
    }
}

#[test]
fn panic_cleanup_stops_client_and_relay_during_incomplete_media_registration() {
    let mut address = String::new();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut client = Client::new(false, 8);
        client.wait_seen(|item| matches!(item, Seen::Ready(_)));
        address = client.relay.origin.trim_start_matches("http://").to_owned();
        // Drop runs with one socket waiting for its second registration control.
        panic!("synthetic test failure to exercise owned cleanup");
    }));
    assert!(outcome.is_err());
    assert!(
        std::net::TcpStream::connect(&address).is_err(),
        "local listener survived panic cleanup"
    );
}

#[test]
fn signaling_start_waits_for_both_media_controls_in_either_order() {
    for peer_first in [false, true] {
        let mut client = Client::new(peer_first, 8);
        client.wait_seen(|item| matches!(item, Seen::Ready(_)));
        client.collect_for(Duration::from_millis(150));
        assert!(
            !client.seen.iter().any(|item| matches!(item,
            Seen::Text("/ws", v) if v["action"] == "start" || v["phase"] == "request")),
            "client must wait for the second registration control: {:?}",
            client.seen
        );
        assert_eq!(client.stats.snapshot().established, 0);
        client.register();
        assert!(!client
            .seen
            .iter()
            .any(|item| matches!(item, Seen::RejectedStart)));
        let second_ready = client
            .seen
            .iter()
            .rposition(|item| matches!(item, Seen::Ready(_)))
            .unwrap();
        let start = client
            .seen
            .iter()
            .position(|item| {
                matches!(item,
            Seen::Text("/ws", v) if v["action"] == "start")
            })
            .unwrap();
        let request = client
            .seen
            .iter()
            .position(|item| {
                matches!(item,
            Seen::Text("/ws", v) if v["phase"] == "request")
            })
            .unwrap();
        assert!(second_ready < start && start < request);
        client.finish();
    }
}

#[test]
fn decoded_ack_and_presented_proof_are_separate_and_stale_feedback_cannot_present() {
    let mut client = Client::new(false, 8);
    client.register();
    let before_send = client.packet(true, 7, 1);
    client.wait_seen(
        |item| matches!(item, Seen::Text("/media-ws", v) if v["type"] == "ack" && v["seq"] == 1),
    );
    let ack_observed = Instant::now();
    // Leave the event queued, proving receipt time is not the decoder dequeue time.
    thread::sleep(Duration::from_millis(120));
    let first = client.frame();
    assert!(before_send <= first.received_at && first.received_at <= ack_observed);
    assert!(first.received_at.elapsed() >= Duration::from_millis(120));
    assert_eq!(
        (first.generation, first.seq, first.capture_ts_us),
        (7, 1, 999_000)
    );
    client
        .feedback
        .send(DecoderFeedback::Decoded {
            local_epoch: first.local_epoch,
            generation: 7,
            seq: 1,
        })
        .unwrap();
    wait_until(|| client.stats.snapshot().decoded == 1);
    client
        .feedback
        .send(DecoderFeedback::Presented {
            local_epoch: first.local_epoch - 1,
            generation: 7,
            seq: 1,
        })
        .unwrap();
    client
        .feedback
        .send(DecoderFeedback::Presented {
            local_epoch: first.local_epoch,
            generation: 6,
            seq: 1,
        })
        .unwrap();
    wait_until(|| client.stats.snapshot().stale_feedback == 1);
    client.collect_for(Duration::from_millis(100));
    assert!(
        client.presentations().is_empty(),
        "decode and stale presentations cannot complete proof"
    );
    client.packet(false, 7, 2);
    client.wait_seen(
        |item| matches!(item, Seen::Text("/media-ws", v) if v["type"] == "ack" && v["seq"] == 2),
    );
    let ack = client
        .seen
        .iter()
        .find_map(|item| match item {
            Seen::Text("/media-ws", v) if v["type"] == "ack" && v["seq"] == 2 => Some(v),
            _ => None,
        })
        .unwrap();
    assert_eq!(ack["decodedSeq"], 1);
    client
        .feedback
        .send(DecoderFeedback::Presented {
            local_epoch: first.local_epoch,
            generation: 7,
            seq: 1,
        })
        .unwrap();
    client.wait_seen(|item| matches!(item, Seen::Text("/ws", v) if v["phase"] == "presented"));
    client
        .feedback
        .send(DecoderFeedback::Presented {
            local_epoch: first.local_epoch,
            generation: 7,
            seq: 2,
        })
        .unwrap();
    client.collect_for(Duration::from_millis(100));
    let presented = client.presentations();
    assert_eq!(presented.len(), 1);
    assert_eq!(presented[0]["generation"], 7);
    let request = client
        .seen
        .iter()
        .find_map(|item| match item {
            Seen::Text("/ws", v) if v["phase"] == "request" => Some(v),
            _ => None,
        })
        .unwrap();
    assert_eq!(presented[0]["requestId"], request["requestId"]);
    assert_eq!(
        client.stats.snapshot().decoded,
        1,
        "presentation is not another decode"
    );
    client.finish();
}

#[test]
fn presentation_from_an_invalidated_generation_cannot_complete_first_frame() {
    let mut client = Client::new(false, 8);
    client.register();
    client.packet(true, 7, 1);
    let first = client.frame();
    client
        .feedback
        .send(DecoderFeedback::DecodeError {
            local_epoch: first.local_epoch,
            reason: "decoder-reset".to_owned(),
        })
        .unwrap();
    client.wait_seen(|item| matches!(item, Seen::Text("/media-ws", v) if v["type"] == "keyframe"));
    client
        .feedback
        .send(DecoderFeedback::Presented {
            local_epoch: first.local_epoch,
            generation: 7,
            seq: 1,
        })
        .unwrap();
    client.collect_for(Duration::from_millis(100));
    assert!(
        client.presentations().is_empty(),
        "invalidated decoder output cannot prove presentation"
    );
    client.packet(true, 8, 1);
    let current = client.frame();
    assert_eq!(current.generation, 8);
    client
        .feedback
        .send(DecoderFeedback::Presented {
            local_epoch: current.local_epoch,
            generation: 8,
            seq: 1,
        })
        .unwrap();
    client.wait_seen(|item| matches!(item, Seen::Text("/ws", v) if v["phase"] == "presented"));
    assert_eq!(client.presentations()[0]["generation"], 8);
    client.finish();
}

#[test]
fn receipt_drop_overflow_and_keyframe_windows_count_socket_events_then_expire() {
    for capacity in [1, 8] {
        let mut client = Client::new(false, capacity);
        client.register();
        client.packet(true, 7, 1);
        if capacity == 1 {
            // The connect reset occupies the sole slot; the ACKed keyframe overflows.
            wait_until(|| client.stats.snapshot().queue_overflows == 1);
        } else {
            assert_eq!(client.frame().seq, 1);
            client.packet(false, 7, 3); // Missing seq 2 invalidates the gate.
            wait_until(|| client.stats.snapshot().gate_drops == 1);
        }
        client.wait_seen(
            |item| matches!(item, Seen::Text("/media-ws", v) if v["type"] == "keyframe"),
        );
        let snapshot = client.stats.snapshot();
        assert_eq!(snapshot.received_fps, if capacity == 1 { 1.0 } else { 2.0 });
        assert_eq!(snapshot.queue_overflows_1s, u32::from(capacity == 1));
        assert_eq!(snapshot.gate_drops_1s, u32::from(capacity == 8));
        assert_eq!(snapshot.keyframe_requests_1s, 1);
        assert_eq!(
            snapshot.acks_sent, snapshot.frames_received,
            "dropped frames remain in raw receipt statistics"
        );
        thread::sleep(Duration::from_millis(1100));
        let expired = client.stats.snapshot();
        assert_eq!(
            (
                expired.received_fps,
                expired.queue_overflows_1s,
                expired.gate_drops_1s,
                expired.keyframe_requests_1s
            ),
            (0.0, 0, 0, 0)
        );
        assert_eq!(
            (
                expired.frames_received,
                expired.queue_overflows,
                expired.gate_drops,
                expired.keyframe_requests
            ),
            (
                snapshot.frames_received,
                snapshot.queue_overflows,
                snapshot.gate_drops,
                snapshot.keyframe_requests
            )
        );
        client.finish();
    }
}

#[test]
fn password_login_reuses_existing_gate_and_returns_rotated_credentials() {
    let relay = OwnedRelay::new(false);
    let existing = CookieJar::from_set_cookie(["__Host-gate=existing-approval; Path=/; Secure"]);
    let request = LoginRequest {
        password: "synthetic-password",
        totp: None,
        remember: true,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let rotated = runtime
        .block_on(login_with_cookies(&relay.origin, &request, &existing))
        .unwrap();
    let Seen::Login(raw) = relay.log.recv_timeout(WAIT).unwrap() else {
        panic!("expected login HTTP request");
    };
    let (headers, body) = raw.split_once("\r\n\r\n").unwrap();
    assert!(headers.starts_with("POST /__auth/login HTTP/1.1"));
    assert!(headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case("cookie: __Host-gate=existing-approval")));
    assert!(headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case(&format!("origin: {}", relay.origin))));
    assert_eq!(
        serde_json::from_str::<Value>(body).unwrap(),
        json!({"password":"synthetic-password","remember":true})
    );
    assert_eq!(rotated.get(GATE_COOKIE), Some("rotated-gate"));
    assert_eq!(rotated.get(SESSION_COOKIE), Some("rotated-session"));
    assert_eq!(existing.get(GATE_COOKIE), Some("existing-approval"));
}

#[test]
fn credential_debug_redacts_password_totp_and_retained_cookie_values() {
    let request = LoginRequest {
        password: "synthetic-password-marker",
        totp: Some("849271"),
        remember: true,
    };
    let cookies = CookieJar::from_set_cookie([
        "__Host-gate=synthetic-gate-marker",
        "__Host-bp-session=synthetic-session-marker",
    ]);
    let printed = format!(
        "{request:?} {:?}",
        NetConfig::new("http://127.0.0.1:1234", Codec::H264, cookies)
    );
    for secret in [
        request.password,
        request.totp.unwrap(),
        "synthetic-gate-marker",
        "synthetic-session-marker",
    ] {
        assert!(!printed.contains(secret), "credential escaped via Debug");
    }
    assert!(printed.contains("remember: true"));
    assert!(printed.contains("has_totp: true"));
}
