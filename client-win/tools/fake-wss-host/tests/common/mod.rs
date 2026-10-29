//! Shared helpers for the integration tests: a synthetic source (no F: files
//! needed), a loopback harness around `server::serve`, and a receiver that
//! ACKs every frame and asks for keyframes through the crate's own gate.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Result;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bp_device_key::phase2::{
    sign_auth_finish, verify_auth_host, CtlKeys, CtlOpener, CtlSealer, EphemeralSecret, Side,
    Transcript,
};
use bp_device_key::{relay_origin, DeviceId, DeviceIdentity, Role};
use fake_wss_host::audio::Script;
use fake_wss_host::model::{Injections, Source};
use fake_wss_host::monitors::Knobs as MonitorKnobs;
use fake_wss_host::phase2::{is_plain, INPUT_KIND};
use fake_wss_host::protocol::{decode_frame, is_valid_epoch, ReceiveGate};
use fake_wss_host::rtc::Knobs;
use fake_wss_host::server::{self, ServerConfig, Snapshot};
use fake_wss_host::stream::Codec;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

/// A socket to the fake. On `/ws`, once `signal_hello_ticket` has run the viewer side of the
/// device-keys handshake, the socket has a channel: a JSON message goes out sealed in `ctl`
/// under its own type, binary input goes out as `ctl` kind `input`, and every `ctl` coming in
/// is opened before the caller reads it. Tests write and read endpoint messages in plain
/// form either way. Other sockets have no channel and pass through unchanged.
pub struct Ws {
    inner: WebSocketStream<MaybeTlsStream<TcpStream>>,
    channel: Option<ViewerChannel>,
}

impl std::fmt::Debug for Ws {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ws")
            .field("ctl", &self.channel.is_some())
            .finish_non_exhaustive()
    }
}

/// The viewer's side of the `ctl` channel.
struct ViewerChannel {
    sealer: CtlSealer,
    opener: CtlOpener,
}

impl Ws {
    fn new(inner: WebSocketStream<MaybeTlsStream<TcpStream>>) -> Self {
        Self {
            inner,
            channel: None,
        }
    }
}

impl ViewerChannel {
    /// A JSON endpoint message, sealed under its own type.
    fn seal_text(&mut self, text: &str) -> Option<String> {
        let kind = serde_json::from_str::<Value>(text)
            .ok()?
            .get("type")?
            .as_str()?
            .to_owned();
        self.seal(&kind, text.as_bytes())
    }

    fn seal(&mut self, kind: &str, plaintext: &[u8]) -> Option<String> {
        let (seq, data) = self.sealer.seal(kind, plaintext).ok()?;
        Some(
            json!({"type": "ctl", "kind": kind, "seq": seq, "data": URL_SAFE_NO_PAD.encode(data)})
                .to_string(),
        )
    }

    /// Opens one `ctl` envelope: text when its body is JSON of its own type, binary otherwise.
    fn open(&mut self, envelope: &Value) -> Option<Message> {
        let kind = envelope["kind"].as_str()?;
        let seq = envelope["seq"].as_u64()?;
        let data = URL_SAFE_NO_PAD.decode(envelope["data"].as_str()?).ok()?;
        let plain = self.opener.open(kind, seq, &data).ok()?;
        match serde_json::from_slice::<Value>(&plain) {
            Ok(body) if body["type"] == kind => {
                Some(Message::Text(String::from_utf8(plain).ok()?.into()))
            }
            _ => Some(Message::Binary(plain.into())),
        }
    }
}

impl Stream for Ws {
    type Item = std::result::Result<Message, tungstenite::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let next = std::task::ready!(Pin::new(&mut this.inner).poll_next(cx));
        Poll::Ready(match (next, this.channel.as_mut()) {
            (Some(Ok(Message::Text(text))), Some(channel)) => {
                let opened = serde_json::from_str::<Value>(&text)
                    .ok()
                    .filter(|value| value["type"] == "ctl")
                    .and_then(|value| channel.open(&value));
                Some(Ok(opened.unwrap_or(Message::Text(text))))
            }
            (next, _) => next,
        })
    }
}

impl Sink<Message> for Ws {
    type Error = tungstenite::Error;

    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> std::result::Result<(), Self::Error> {
        let item = match self.channel.as_mut() {
            None => item,
            Some(channel) => match item {
                Message::Text(text) if !is_plain(&text) => match channel.seal_text(&text) {
                    Some(sealed) => Message::Text(sealed.into()),
                    None => Message::Text(text),
                },
                Message::Binary(bytes) => match channel.seal(INPUT_KIND, &bytes) {
                    Some(sealed) => Message::Text(sealed.into()),
                    None => Message::Binary(bytes),
                },
                other => other,
            },
        };
        Pin::new(&mut self.inner).start_send(item)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

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
    spawn_full(
        sources,
        injections,
        exit_after,
        drain,
        input_record,
        audio,
        rtc,
        MonitorKnobs::default(),
    )
    .await
}

/// A run with the monitor surface as `monitors` describes (off by default),
/// and optionally the WebRTC answerer. Recordings are synthetic.
pub async fn spawn_monitored(
    sources: Vec<Arc<dyn Source>>,
    exit_after: Duration,
    drain: Duration,
    monitors: MonitorKnobs,
    rtc: Option<Knobs>,
) -> Harness {
    spawn_full(
        sources,
        no_injections(),
        exit_after,
        drain,
        None,
        None,
        rtc,
        monitors,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn spawn_full(
    sources: Vec<Arc<dyn Source>>,
    injections: Injections,
    exit_after: Duration,
    drain: Duration,
    input_record: Option<PathBuf>,
    audio: Option<Script>,
    rtc: Option<Knobs>,
    monitors: MonitorKnobs,
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
        monitors,
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
    Ok(Ws::new(ws))
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

/// The next message of `kind`. Anything else on the socket is skipped, so a test does not
/// depend on the post-handshake `monitor-list` being present.
pub async fn until_type(ws: &mut Ws, kind: &str) -> Value {
    for _ in 0..16 {
        let value = next_json(ws, Duration::from_secs(2))
            .await
            .expect("a message from the fake");
        if value["type"] == kind {
            return value;
        }
    }
    panic!("no {kind} message arrived");
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

/// The viewer identity the handshake tests admit: the seed
/// `9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60`.
pub const VIEWER_SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];

pub fn viewer_identity() -> DeviceIdentity {
    DeviceIdentity::from_seed(&VIEWER_SEED)
}

/// `/ws` hello as a viewer: the signed hello, the challenge, the proof, then
/// hello-ok. Returns the socket, its epoch and the media ticket.
pub async fn signal_hello_ticket(addr: SocketAddr) -> (Ws, String, String) {
    let identity = viewer_identity();
    let mut ws = open(addr, "/ws").await.expect("open /ws");
    send_json(
        &mut ws,
        json!({
            "type": "hello",
            "role": "viewer",
            "id": identity.id().to_string(),
            "public": identity.public_b64(),
        }),
    )
    .await;
    let challenge = next_json(&mut ws, Duration::from_secs(2))
        .await
        .expect("register-challenge");
    assert_eq!(challenge["type"], "register-challenge", "{challenge}");
    let nonce: [u8; 32] = URL_SAFE_NO_PAD
        .decode(challenge["nonce"].as_str().expect("nonce"))
        .expect("nonce is base64url")
        .try_into()
        .expect("nonce is 32 bytes");
    // The origin the client signs for: the relay's ws:// URL, canonicalised.
    let origin = relay_origin(&format!("ws://{addr}")).expect("loopback origin");
    let signature = identity.sign_relay_proof(Role::Viewer, &origin, &nonce);
    send_json(
        &mut ws,
        json!({"type": "register-proof", "signature": URL_SAFE_NO_PAD.encode(signature)}),
    )
    .await;
    let reply = next_json(&mut ws, Duration::from_secs(2))
        .await
        .expect("hello-ok");
    assert_eq!(reply["type"], "hello-ok", "{reply}");
    let epoch = reply["mediaEpoch"].as_str().expect("mediaEpoch").to_owned();
    assert!(is_valid_epoch(&epoch), "bad epoch {epoch}");
    let ticket = reply["ticket"].as_str().expect("ticket").to_owned();
    let ticket_bytes = URL_SAFE_NO_PAD
        .decode(&ticket)
        .expect("ticket is base64url");
    assert_eq!(ticket_bytes.len(), 32, "ticket is 32 bytes");
    viewer_handshake(&mut ws, &origin, &epoch).await;
    (ws, epoch, ticket)
}

/// The viewer side of the device-keys handshake on an admitted `/ws`: `auth-hello`, then
/// `auth-host` checked against the transcript, then `auth-finish`. The socket then has its
/// channel. The viewer's `caps` go first, and the PC's sealed `caps` comes back.
async fn viewer_handshake(ws: &mut Ws, origin: &str, epoch: &str) {
    let identity = viewer_identity();
    let viewer_nonce = [0x5a; 32];
    let viewer_secret = EphemeralSecret::from_bytes([0x6b; 32]);
    let viewer_ephemeral = viewer_secret.public_key();
    send_json(
        ws,
        json!({
            "type": "auth-hello",
            "id": identity.id().to_string(),
            "public": identity.public_b64(),
            "nonce": URL_SAFE_NO_PAD.encode(viewer_nonce),
            "ephemeral": URL_SAFE_NO_PAD.encode(viewer_ephemeral),
            "caps": ["control-aead-v1", "dtls-bind-v1"],
        }),
    )
    .await;
    let host = next_json(ws, Duration::from_secs(2))
        .await
        .expect("auth-host");
    assert_eq!(host["type"], "auth-host", "{host}");
    let pc_id = DeviceId::parse(host["id"].as_str().expect("pc id")).expect("pc id parses");
    let pc_public = decode32(&host["public"]);
    let pc_nonce = decode32(&host["nonce"]);
    let pc_ephemeral = decode32(&host["ephemeral"]);
    let pc_signature: [u8; 64] = URL_SAFE_NO_PAD
        .decode(host["signature"].as_str().expect("signature"))
        .expect("signature is base64url")
        .try_into()
        .expect("signature is 64 bytes");
    let h = Transcript {
        relay_origin: origin,
        epoch,
        viewer_id: identity.id(),
        viewer_public: identity.public_key(),
        pc_id,
        pc_public,
        viewer_nonce,
        pc_nonce,
        viewer_ephemeral,
        pc_ephemeral,
    }
    .hash();
    verify_auth_host(&pc_public, &h, &pc_signature).expect("auth-host verifies");
    let shared = viewer_secret
        .diffie_hellman(&pc_ephemeral)
        .expect("key agreement");
    let (sealer, opener) = CtlKeys::derive(&shared, &h).channel(Side::Viewer);
    let finish = sign_auth_finish(&identity, &h, &pc_signature);
    send_json(
        ws,
        json!({"type": "auth-finish", "signature": URL_SAFE_NO_PAD.encode(finish)}),
    )
    .await;
    ws.channel = Some(ViewerChannel { sealer, opener });
    send_json(
        ws,
        json!({"type": "caps", "caps": ["control-aead-v1", "dtls-bind-v1"]}),
    )
    .await;
    let caps = next_json(ws, Duration::from_secs(2))
        .await
        .expect("the PC's caps");
    assert_eq!(caps["type"], "caps", "{caps}");
    assert!(
        caps["caps"]
            .as_array()
            .is_some_and(|list| list.iter().any(|cap| cap == "control-aead-v1")),
        "{caps}"
    );
}

fn decode32(value: &Value) -> [u8; 32] {
    URL_SAFE_NO_PAD
        .decode(value.as_str().expect("base64url field"))
        .expect("base64url")
        .try_into()
        .expect("32 bytes")
}

/// `signal_hello_ticket` without the ticket.
pub async fn signal_hello(addr: SocketAddr) -> (Ws, String) {
    let (ws, epoch, _ticket) = signal_hello_ticket(addr).await;
    (ws, epoch)
}

/// `/media-ws?epoch=` hello with the ticket; the caller reads the reply.
pub async fn media_open(
    addr: SocketAddr,
    epoch: &str,
    ticket: &str,
) -> std::result::Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut ws = open(addr, &format!("/media-ws?epoch={epoch}")).await?;
    send_json(
        &mut ws,
        json!({"type": "hello", "role": "media-viewer", "ticket": ticket}),
    )
    .await;
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
    let (mut signal, epoch, ticket) = signal_hello_ticket(addr).await;
    let mut media = media_open(addr, &epoch, &ticket)
        .await
        .expect("media upgrade");
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
