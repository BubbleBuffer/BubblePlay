//! Shared helpers for the integration tests: a synthetic source (no F: files
//! needed), a loopback harness around `server::serve`, and a receiver that
//! ACKs every frame and asks for keyframes through the crate's own gate.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use fake_wss_host::audio::Script;
use fake_wss_host::model::{Injections, Source};
use fake_wss_host::protocol::{decode_frame, is_valid_epoch, ReceiveGate};
use fake_wss_host::rtc::Knobs;
use fake_wss_host::server::{self, ServerConfig, Snapshot};
use fake_wss_host::stream::Codec;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Access units of `payload_bytes` each; every `gop`-th unit is an IDR. The
/// payload is not a real NAL stream, which the server never parses.
pub struct Synthetic {
    pub codec: Codec,
    pub len: usize,
    pub gop: usize,
    pub payload_bytes: usize,
}

impl Source for Synthetic {
    fn codec(&self) -> Codec {
        self.codec
    }
    fn width(&self) -> u32 {
        1920
    }
    fn height(&self) -> u32 {
        1080
    }
    fn len(&self) -> usize {
        self.len
    }
    fn is_key(&self, index: usize) -> bool {
        index.is_multiple_of(self.gop)
    }
    fn has_param_sets(&self, _index: usize) -> bool {
        true
    }
    fn next_key_at_or_after(&self, index: usize) -> usize {
        let next = index.div_ceil(self.gop) * self.gop;
        if next >= self.len {
            0
        } else {
            next
        }
    }
    fn payload(&self, index: usize) -> Vec<u8> {
        let mut out = vec![0x5a; self.payload_bytes];
        out[..4].copy_from_slice(&(index as u32).to_be_bytes());
        out
    }
}

pub fn synthetic(len: usize, gop: usize) -> Arc<dyn Source> {
    synthetic_codec(Codec::H264, len, gop)
}

pub fn synthetic_codec(codec: Codec, len: usize, gop: usize) -> Arc<dyn Source> {
    Arc::new(Synthetic {
        codec,
        len,
        gop,
        payload_bytes: 1000,
    })
}

pub struct Harness {
    pub addr: SocketAddr,
    pub run: JoinHandle<Result<Snapshot>>,
}

pub async fn spawn(
    source: Arc<dyn Source>,
    injections: Injections,
    exit_after: Duration,
    drain: Duration,
) -> Harness {
    spawn_many(vec![source], injections, exit_after, drain).await
}

/// A run that offers every source; `start` chooses among them.
pub async fn spawn_many(
    sources: Vec<Arc<dyn Source>>,
    injections: Injections,
    exit_after: Duration,
    drain: Duration,
) -> Harness {
    spawn_with(sources, injections, exit_after, drain, None, None, None).await
}

/// A run with the audio lane on. `sources` may be empty: an audio-only run.
pub async fn spawn_audio(
    sources: Vec<Arc<dyn Source>>,
    script: Script,
    exit_after: Duration,
    drain: Duration,
) -> Harness {
    spawn_with(
        sources,
        no_injections(),
        exit_after,
        drain,
        None,
        Some(script),
        None,
    )
    .await
}

/// A run that also records every binary message received on `/ws` to `record`.
pub async fn spawn_recording(
    source: Arc<dyn Source>,
    exit_after: Duration,
    drain: Duration,
    record: &Path,
) -> Harness {
    spawn_with(
        vec![source],
        no_injections(),
        exit_after,
        drain,
        Some(record.to_path_buf()),
        None,
        None,
    )
    .await
}

/// A run with the WebRTC answerer on. The browser's offer arrives on `/ws`;
/// no media socket is used.
pub async fn spawn_webrtc(
    sources: Vec<Arc<dyn Source>>,
    exit_after: Duration,
    knobs: Knobs,
) -> Harness {
    spawn_with(
        sources,
        no_injections(),
        exit_after,
        Duration::ZERO,
        None,
        None,
        Some(knobs),
    )
    .await
}

pub fn no_injections() -> Injections {
    Injections::default()
}

async fn spawn_with(
    sources: Vec<Arc<dyn Source>>,
    injections: Injections,
    exit_after: Duration,
    drain: Duration,
    input_record: Option<PathBuf>,
    audio: Option<Script>,
    rtc: Option<Knobs>,
) -> Harness {
    let listener = server::bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let config = ServerConfig {
        sources,
        fps: 60,
        bitrate_bps: 20_000_000,
        injections,
        exit_after,
        drain,
        max_connections: 8,
        input_record,
        audio,
        rtc,
    };
    Harness {
        addr,
        run: tokio::spawn(server::serve(listener, config)),
    }
}

impl Harness {
    /// Wait for the run to finish and return its snapshot.
    pub async fn finish(self) -> Snapshot {
        self.run.await.expect("server task").expect("serve")
    }
}

pub async fn open(
    addr: SocketAddr,
    path: &str,
) -> std::result::Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let (ws, _) = connect_async(format!("ws://{addr}{path}")).await?;
    if let MaybeTlsStream::Plain(tcp) = ws.get_ref() {
        tcp.set_nodelay(true)?;
    }
    Ok(ws)
}

pub async fn send_json(ws: &mut Ws, value: Value) {
    ws.send(Message::Text(value.to_string().into()))
        .await
        .expect("send");
}

/// The next binary message's bytes, or `None` after `wait` or on close. Text
/// messages are skipped.
pub async fn next_binary(ws: &mut Ws, wait: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + wait;
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        match timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => return Some(bytes.to_vec()),
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

/// Inbound packet records, excluding the separately marked lifecycle rows.
pub fn read_records(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).expect("read input record");
    text.lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("each record is one JSON line"))
        .filter(|row| row["type"].is_number())
        .collect()
}

/// The next text message parsed as JSON, or `None` after `wait` or on close.
pub async fn next_json(ws: &mut Ws, wait: Duration) -> Option<Value> {
    let deadline = Instant::now() + wait;
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        match timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).ok()?;
                if value["type"] == "input-capabilities" {
                    continue;
                }
                return Some(value);
            }
            Ok(Some(Ok(Message::Binary(_)))) | Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

/// `/ws` hello as the browser sends it; returns the socket and its epoch.
pub async fn signal_hello(addr: SocketAddr) -> (Ws, String) {
    let mut ws = open(addr, "/ws").await.expect("open /ws");
    send_json(&mut ws, json!({"type": "hello", "role": "client"})).await;
    let reply = next_json(&mut ws, Duration::from_secs(2))
        .await
        .expect("hello-ok");
    assert_eq!(reply["type"], "hello-ok", "{reply}");
    let epoch = reply["mediaEpoch"].as_str().expect("mediaEpoch").to_owned();
    assert!(is_valid_epoch(&epoch), "bad epoch {epoch}");
    (ws, epoch)
}

/// `/media-ws?epoch=` hello; the caller reads the reply.
pub async fn media_open(
    addr: SocketAddr,
    epoch: &str,
) -> std::result::Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut ws = open(addr, &format!("/media-ws?epoch={epoch}")).await?;
    send_json(&mut ws, json!({"type": "hello", "role": "media-client"})).await;
    Ok(ws)
}

/// The browser's start, with the codec named the way the browser names it.
pub fn start_message(epoch: &str, codec: &str) -> Value {
    json!({
        "type": "media-control", "action": "start", "protocol": 4, "epoch": epoch,
        "codec": codec, "streamMode": "desktop", "reason": "client-start", "quality": "high"
    })
}

/// A protocol-4 start that opts the media epoch in to audio.
pub fn start_with_audio(epoch: &str, codec: &str) -> Value {
    let mut value = start_message(epoch, codec);
    value["audio"] = json!(1);
    value
}

/// Start the fake streaming on a fresh session with media attached.
pub async fn streaming_session(addr: SocketAddr, codec: &str) -> (Ws, Ws, String) {
    let (mut signal, epoch) = signal_hello(addr).await;
    let mut media = media_open(addr, &epoch).await.expect("media upgrade");
    let reply = next_json(&mut media, Duration::from_secs(2))
        .await
        .expect("media hello-ok");
    assert_eq!(reply["type"], "hello-ok", "{reply}");
    let peer = next_json(&mut media, Duration::from_secs(2))
        .await
        .expect("media-socket peer");
    assert_eq!(peer["type"], "media-peer", "{peer}");
    assert_eq!(peer["present"].as_bool(), Some(true), "{peer}");
    // The fake also supplies an informational signaling notice; consume it
    // before tests wait for first-frame replies on that lane.
    let _ = next_json(&mut signal, Duration::from_secs(2))
        .await
        .expect("signaling peer notice");
    send_json(&mut signal, start_message(&epoch, codec)).await;
    (signal, media, epoch)
}

/// One streaming session that ACKs every frame until `until` or until the
/// socket closes. Returns what the receiver saw and the epoch it used.
pub async fn ack_session(addr: SocketAddr, codec: &str, until: Instant) -> (Receiver, String) {
    let (signal, mut media, epoch) = streaming_session(addr, codec).await;
    let mut log = Receiver::default();
    let mut gate = ReceiveGate::new();
    ack_until(&mut media, until, &mut log, &mut gate).await;
    drop(signal);
    (log, epoch)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Got {
    pub generation: u32,
    pub seq: u32,
    pub key: bool,
    /// The codec byte from the frame header (1 = HEVC, 2 = H.264).
    pub codec: u8,
}

#[derive(Debug, Default)]
pub struct Receiver {
    pub frames: Vec<Got>,
    pub keyframe_requests: Vec<String>,
    pub closed: bool,
}

/// Read and ACK frames until `until` or the socket closes. Each frame is
/// ACKed on receipt, before the gate, as the browser does. Keyframe requests
/// from the gate are sent back as v4 text.
pub async fn ack_until(ws: &mut Ws, until: Instant, log: &mut Receiver, gate: &mut ReceiveGate) {
    loop {
        let Some(remaining) = until.checked_duration_since(Instant::now()) else {
            return;
        };
        match timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                let (header, _payload) = decode_frame(&bytes).expect("valid BPV4 frame");
                let ack = json!({
                    "type": "ack", "generation": header.generation, "seq": header.seq, "recvTsMs": 0
                });
                send_json(ws, ack).await;
                log.frames.push(Got {
                    generation: header.generation,
                    seq: header.seq,
                    key: header.key,
                    codec: header.codec,
                });
                let verdict = gate.accept(&header);
                if let Some(reason) = verdict.request_keyframe {
                    send_json(ws, json!({"type": "keyframe", "reason": reason})).await;
                    log.keyframe_requests.push(reason);
                }
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) => {
                log.closed = true;
                return;
            }
            Ok(Some(Ok(_))) => {}
            Err(_) => return,
        }
    }
}
