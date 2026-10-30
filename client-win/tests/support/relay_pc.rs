//! The PC side of the client's relay end-to-end tests (stage B3), against the native Rust
//! relay (`bp-relay-server`) started in-process.
//!
//! The client is a binary crate, so this file is compiled into its unit tests
//! (`src/session/mod.rs` includes it with `#[path]`); the tests are in
//! `src/session/relay_e2e_tests.rs`. All cryptography is `bp_device_key` (through the
//! client's `net::phase2::test_pc` helpers, which play the fixture "PC test" key); the
//! relay steps follow relay-rs/server/tests/common/mod.rs (`Server::start`,
//! `register_host`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bp_device_key::{DeviceIdentity, Role};
use bp_relay_server::config::{Config, Secret};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::net::phase2::{test_pc, Opened};

pub const TOKEN: &str = "tok-e2e";
/// How long the PC waits for any one message.
pub const WAIT: Duration = Duration::from_secs(10);

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The relay, serving on a loopback port. Its public origin is `http://127.0.0.1:<port>`,
/// the origin the client signs its proof and transcript over.
pub struct Relay {
    pub addr: SocketAddr,
    pub origin: String,
    dir: PathBuf,
    task: JoinHandle<()>,
}

impl Relay {
    pub async fn start() -> Relay {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let origin = format!("http://{addr}");
        let dir = std::env::temp_dir().join(format!(
            "bp-client-e2e-{}-{}",
            std::process::id(),
            addr.port()
        ));
        let (state_dir, assets_dir) = (dir.join("state"), dir.join("assets"));
        std::fs::create_dir_all(&state_dir).expect("state dir");
        std::fs::create_dir_all(&assets_dir).expect("assets dir");
        let cfg = Config {
            public_origin: origin.clone(),
            listen: addr,
            assets_dir,
            state_dir,
            token: Secret::new(TOKEN),
            admin_token: Secret::new("admin-e2e"),
            max_connections: 32,
            turn: None,
            ntfy: None,
            approval_ttl_s: 90,
            trusted_proxy_cidrs: Vec::new(),
            // The relay's production hold (bp_relay_core::gate::GATE_HOLD_MS).
            gate_hold_ms: 30_000,
        };
        let task = tokio::spawn(async move {
            let _ = bp_relay_server::server::run(&cfg, listener).await;
        });
        Relay {
            addr,
            origin,
            dir,
            task,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.addr)
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The fixture PC's identity (ID `77VH-7M56-8GJK-Y12J`).
pub fn pc_identity() -> DeviceIdentity {
    test_pc::pc_identity()
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub async fn send_json(ws: &mut Ws, value: Value) {
    ws.send(Message::Text(value.to_string().into()))
        .await
        .expect("PC send");
}

pub async fn send_text(ws: &mut Ws, text: String) {
    ws.send(Message::Text(text.into())).await.expect("PC send");
}

/// The next text message, parsed. Panics on a close, a timeout or a non-JSON text.
pub async fn next_json(ws: &mut Ws) -> Value {
    loop {
        let item = timeout(WAIT, ws.next())
            .await
            .expect("PC waited too long for a message");
        match item {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(text.as_str()).expect("JSON text")
            }
            Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => continue,
            other => panic!("PC socket ended: {other:?}"),
        }
    }
}

/// The next message of `kind`, skipping relay `pong`s only; anything else is a failure.
pub async fn next_of(ws: &mut Ws, kind: &str) -> Value {
    loop {
        let value = next_json(ws).await;
        match value["type"].as_str() {
            Some(found) if found == kind => return value,
            Some("pong") => continue,
            _ => panic!("PC expected {kind}, got {value}"),
        }
    }
}

/// Whether the socket stays without a text message (other than `pong`) for `quiet`.
pub async fn quiet(ws: &mut Ws, quiet: Duration) -> Option<Value> {
    loop {
        match timeout(quiet, ws.next()).await {
            Err(_) => return None,
            Ok(Some(Ok(Message::Text(text)))) => {
                let value: Value = serde_json::from_str(text.as_str()).expect("JSON text");
                if value["type"] != "pong" {
                    return Some(value);
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(other) => return Some(json!({ "ended": format!("{other:?}") })),
        }
    }
}

/// Wait until the relay has handled everything sent before on this socket.
pub async fn sync(ws: &mut Ws) {
    send_json(ws, json!({ "type": "ping" })).await;
    next_of(ws, "pong").await;
}

/// Register the fixture PC on `/host-ws` (TOKEN hello, `register-proof` as
/// `Role::Host`, `hello-ok`) and admit `viewers`. Returns the socket and its `hello-ok`.
pub async fn register_host(relay: &Relay, viewers: &[String]) -> (Ws, Value) {
    let pc = pc_identity();
    let (mut ws, _) = connect_async(relay.url(&format!("/host-ws?pc={}", pc.id())))
        .await
        .expect("host socket");
    send_json(
        &mut ws,
        json!({
            "type": "hello",
            "role": "host",
            "id": pc.id().to_string(),
            "public": pc.public_b64(),
            "token": TOKEN,
        }),
    )
    .await;
    let challenge = next_of(&mut ws, "register-challenge").await;
    let nonce: [u8; 32] = URL_SAFE_NO_PAD
        .decode(challenge["nonce"].as_str().expect("nonce"))
        .expect("base64url nonce")
        .try_into()
        .expect("32-byte nonce");
    let signature = pc.sign_relay_proof(Role::Host, &relay.origin, &nonce);
    send_json(
        &mut ws,
        json!({ "type": "register-proof", "signature": b64(&signature) }),
    )
    .await;
    let ok = next_of(&mut ws, "hello-ok").await;
    admit(&mut ws, viewers).await;
    (ws, ok)
}

/// Replace the PC's admit list; returns once the relay applied it.
pub async fn admit(ws: &mut Ws, viewers: &[String]) {
    send_json(
        ws,
        json!({ "type": "admit", "viewers": viewers, "remotePairing": false }),
    )
    .await;
    sync(ws).await;
}

/// One viewer connection as the PC sees it after the handshake.
pub struct Viewer {
    /// The `mediaEpoch` from the relay's `peer-joined`, exactly as received.
    pub epoch: String,
    pub pairing: bool,
    pub session: test_pc::PcSession,
    /// The viewer's `auth-hello`, parsed.
    pub hello: Value,
    /// The `seq` the viewer's next `ctl` must carry.
    next_seq: u64,
}

/// `peer-joined`, then the PC side of the handshake: `auth-host` for the viewer's
/// `auth-hello` over the relay origin and the announced epoch. With `finish`, the viewer's
/// `auth-finish` must verify; without, the caller checks what the viewer does instead.
pub async fn handshake(ws: &mut Ws, origin: &str, finish: bool) -> Viewer {
    let joined = next_of(ws, "peer-joined").await;
    let epoch = joined["mediaEpoch"]
        .as_str()
        .expect("peer-joined mediaEpoch")
        .to_owned();
    let pairing = joined["pairing"].as_bool().expect("peer-joined pairing");
    let hello = next_of(ws, "auth-hello").await;
    let (host, session) =
        test_pc::answer(&hello.to_string(), origin, &epoch).expect("a valid auth-hello");
    send_text(ws, host).await;
    if finish {
        let auth_finish = next_of(ws, "auth-finish").await;
        assert!(
            session.finish_ok(&auth_finish.to_string()),
            "the viewer's auth-finish verifies"
        );
    }
    Viewer {
        epoch,
        pairing,
        session,
        hello,
        next_seq: 0,
    }
}

impl Viewer {
    /// The viewer's next `ctl`, opened. Its envelope `seq` must be the next in order
    /// (0, 1, 2, ...); the opener checks tag and kind. Plain relay messages are skipped.
    pub async fn recv(&mut self, ws: &mut Ws) -> Opened {
        loop {
            let envelope = next_json(ws).await;
            if envelope["type"] != "ctl" {
                continue;
            }
            let seq = envelope["seq"].as_u64().expect("ctl seq");
            assert_eq!(seq, self.next_seq, "the viewer's ctl seq is in order");
            self.next_seq += 1;
            return self
                .session
                .ctl
                .open(&envelope)
                .expect("the viewer's ctl opens");
        }
    }

    /// The next opened `ctl` text of `kind`, skipping other kinds.
    pub async fn recv_text(&mut self, ws: &mut Ws, kind: &str) -> Value {
        loop {
            if let Opened::Text { kind: found, text } = self.recv(ws).await {
                if found == kind {
                    return serde_json::from_str(&text).expect("ctl JSON");
                }
            }
        }
    }

    /// Seal `value` in `ctl` and send it; returns the `seq` it went out with.
    pub async fn send(&mut self, ws: &mut Ws, value: Value) -> u64 {
        let sealed = self
            .session
            .ctl
            .seal_text(&value.to_string())
            .expect("seal");
        let seq = serde_json::from_str::<Value>(&sealed).expect("envelope")["seq"]
            .as_u64()
            .expect("seq");
        send_text(ws, sealed).await;
        seq
    }
}

/// The PC's media socket for `epoch`: `/host-media-ws` with the host ticket. Returns the
/// socket and the `media-peer` `present` flag the relay sent with its `hello-ok`.
pub async fn host_media(relay: &Relay, ticket: &str, epoch: &str) -> (Ws, bool) {
    let pc = pc_identity().id();
    let (mut ws, _) = connect_async(relay.url(&format!("/host-media-ws?pc={pc}&epoch={epoch}")))
        .await
        .expect("host media socket");
    send_json(
        &mut ws,
        json!({ "type": "hello", "role": "media-host", "ticket": ticket }),
    )
    .await;
    next_of(&mut ws, "hello-ok").await;
    let peer = next_of(&mut ws, "media-peer").await;
    (ws, peer["present"].as_bool().expect("media-peer present"))
}

/// A WSS v4 video frame: the 28-byte big-endian `BPV4` header (H.264) and `payload`.
pub fn bpv4_frame(key: bool, generation: u32, seq: u32, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(28 + payload.len());
    frame.extend_from_slice(&0x4250_5634u32.to_be_bytes());
    frame.push(4);
    frame.push(u8::from(key));
    frame.push(2);
    frame.push(0);
    frame.extend_from_slice(&generation.to_be_bytes());
    frame.extend_from_slice(&seq.to_be_bytes());
    frame.extend_from_slice(&1_000u64.to_be_bytes());
    frame.extend_from_slice(&640u16.to_be_bytes());
    frame.extend_from_slice(&360u16.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}
