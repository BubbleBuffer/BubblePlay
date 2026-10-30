//! Phase 2 end to end against the native relay (`bp-relay-server`), started in-process on an
//! ephemeral loopback port (Windows only).
//!
//! What it runs, in order:
//! 1. An isolated relay: `bp_relay_server::server::run` on its own tokio runtime, bound to
//!    127.0.0.1:0, with state and assets in a temp directory and test-only tokens.
//! 2. The real host `Signaling::connect_phase2` registers the fixture PC identity. The real host
//!    controller opens pairing through `DevicesCommand::StartPairing`.
//! 3. A scripted viewer pairs over `/ws?pc=ID`. It takes `mediaEpoch` from its hello-ok as text
//!    and checks it is 32 lowercase hex. It runs the pairing handshake and MACs from
//!    `bp_device_key::phase2`, then checks the PC's SAS against the Devices window's
//!    `PairRequest`. The test sends `DevicesCommand::Allow(id)` for that request and expects
//!    `pair-done` and `PairDone`.
//! 4. The test waits until the relay's `pcs.json` lists the viewer as admitted for this PC.
//! 5. The trusted viewer reconnects normally. Its hello-ok epoch must differ from the pairing
//!    epoch. It completes the handshake, opens the encrypted `caps` and runs an encrypted
//!    ping/pong.
//!
//! Not covered: WebRTC media, DTLS binding, and file, clipboard, cursor and input traffic. The
//! test exchanges only the authenticated ctl channel (caps, ping and pong).
//!
//! The relay is a path dev-dependency (`../relay-rs/server`), so the normal host test run
//! (win-check.sh, CI) builds and runs this test; it spawns no process.

#![cfg(windows)]

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bp_device_key::phase2::{
    sign_auth_finish, verify_auth_host, CtlKeys, CtlOpener, EphemeralSecret, PairingCode, Sas,
    Side, Transcript,
};
use bp_device_key::{DeviceId, DeviceIdentity as ViewerIdentity, Role};
use bp_relay_server::config::{Config, Secret};
use bubbleplay_host::device_key::DeviceIdentity as HostIdentity;
use bubbleplay_host::devices_ui_api::{channel, DevicesCommand, DevicesEvent, UiEnds};
use bubbleplay_host::signaling::{SignalEvent, Signaling};
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use tungstenite::{Message, WebSocket};

const FIXTURE: &str = "../fixtures/device-keys-contract.json";
/// The vendored shared fixture: the current key file format lives only here.
const SHARED_FIXTURE: &str = "../fixtures/shared/device-keys-contract.json";
/// Upper bound for every wait on the relay or the host.
const STEP: Duration = Duration::from_secs(20);
/// Read timeout slice: each read returns quickly so the host can be pumped in between.
const SLICE: Duration = Duration::from_millis(50);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
/// Host pumping after a state change, so the relay's ordering settles before the next step.
const SETTLE: Duration = Duration::from_millis(300);

#[test]
fn phase2_pairing_then_trusted_reconnect_through_bp_relay() {
    run().expect("phase 2 relay end-to-end");
}

fn run() -> Result<()> {
    let fixture = fixture()?;
    let host_key = serde_json::to_string(&shared_fixture()?["keyFiles"]["plain"]["file"])?;
    let host = HostIdentity::from_plain_key_json(&host_key).context("fixture PC key")?;
    let host_id = host.id().to_string();
    ensure!(
        host_id == text_field(&fixture["handshake"]["pc"], "id")?,
        "fixture PC key does not match the handshake fixture"
    );
    let viewer_seed = seed_from_hex(text_field(&fixture["handshake"]["viewer"], "seedHex")?)?;
    let viewer = ViewerIdentity::from_seed(&viewer_seed);
    let viewer_id = viewer.id().to_string();
    ensure!(
        viewer_id == text_field(&fixture["handshake"]["viewer"], "id")?,
        "fixture viewer seed does not match its ID"
    );

    // Declaration order matters: drops run in reverse, so the harness and the relay are gone
    // before their directory is removed and before the environment is restored.
    let work = TempDir::new()?;
    let mut env_guard = EnvGuard::new();
    let local_app_data = work.join("localappdata");
    fs::create_dir_all(&local_app_data)?;
    // The host keeps its trust list under LOCALAPPDATA, so this never touches a real user list.
    env_guard.set("LOCALAPPDATA", &local_app_data);

    let relay = Relay::start(&work)?;
    let (port, origin, token) = (relay.port, relay.origin.clone(), relay.token.clone());
    let target = Target {
        port,
        origin: &origin,
        host_id: &host_id,
        host_public: host.public_key(),
    };

    let (ends, ui) = channel();
    let signaling = Signaling::connect_phase2(
        &format!("ws://127.0.0.1:{port}/host-ws"),
        &token,
        &host,
        Arc::new(Mutex::new(ends)),
    )
    .context("host Signaling::connect_phase2")?;
    let mut harness = Harness {
        host: signaling,
        ui,
        peer_left: false,
    };
    ensure!(
        !harness.host.application_ready(),
        "a fresh host must not be ready"
    );

    harness
        .ui
        .commands
        .send(DevicesCommand::StartPairing)
        .map_err(|_| anyhow!("Devices command channel closed"))?;
    let waiting = harness.devices_event("PairWaiting", |event| {
        matches!(event, DevicesEvent::PairWaiting { .. })
    })?;
    let DevicesEvent::PairWaiting {
        code_display,
        pc_id,
        ..
    } = waiting
    else {
        bail!("PairWaiting expected");
    };
    ensure!(pc_id == host_id, "PairWaiting names another PC");
    harness.settle()?;

    let pairing_epoch = pair_viewer(&mut harness, &target, &viewer, &code_display)?;
    harness.settle()?;

    let trust = trusted_devices_contains(&viewer_id)?;
    ensure!(
        trust,
        "PairDone was reported but the trust list does not hold the viewer"
    );

    let state_dir = work.join("state");
    wait_for_admitted(&mut harness, &state_dir, &host_id, &viewer_id)?;

    reconnect_and_ping(&mut harness, &target, &viewer, &pairing_epoch)?;
    Ok(())
}

/// The relay and PC a viewer talks to.
struct Target<'a> {
    port: u16,
    origin: &'a str,
    host_id: &'a str,
    host_public: [u8; 32],
}

/// The fixture file, read-only.
fn fixture() -> Result<Value> {
    read_json(FIXTURE)
}

fn shared_fixture() -> Result<Value> {
    read_json(SHARED_FIXTURE)
}

fn read_json(relative: &str) -> Result<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).context("parse device-keys-contract.json")
}

fn trusted_devices_contains(viewer_id: &str) -> Result<bool> {
    let path = bubbleplay_host::trusted_devices::default_path()?;
    let list = bubbleplay_host::trusted_devices::load(&path)?;
    Ok(list.devices().iter().any(|device| device.id == viewer_id))
}

/// Drives the pairing handshake to `pair-done` and returns the pairing epoch from its hello-ok.
fn pair_viewer(
    harness: &mut Harness,
    target: &Target,
    viewer: &ViewerIdentity,
    code_display: &str,
) -> Result<String> {
    let code = PairingCode::parse(code_display).context("PairWaiting code")?;
    let pc_id = DeviceId::parse(target.host_id)?;
    let mut admitted = register(harness, target, viewer, Some(code.admit_hash(&pc_id)))?;
    ensure!(
        admitted.pairing,
        "pairing hello-ok must carry pairing: true"
    );
    let hs = handshake(
        harness,
        &mut admitted,
        target,
        viewer,
        &["control-aead-v1", "dtls-bind-v1", "pair-v1"],
    )?;

    let pair_key = code.pair_key();
    let viewer_mac = pair_key.mac(Side::Viewer, &hs.transcript);
    send_json(
        &mut admitted.ws,
        json!({"type": "pair-proof", "mac": b64(&viewer_mac)}),
    )?;
    let proof = next_json(harness, &mut admitted.ws, "pair-proof", "pair-proof")?;
    let pc_mac: Vec<u8> = decode_b64(text_field(&proof, "mac")?)?;
    pair_key
        .verify_mac(Side::Pc, &hs.transcript, &pc_mac)
        .context("PC pair MAC does not verify")?;

    let expected_sas = Sas::from_transcript_hash(&hs.transcript).display();
    let request = harness.devices_event("PairRequest", |event| {
        matches!(event, DevicesEvent::PairRequest { .. })
    })?;
    let DevicesEvent::PairRequest {
        id, sas_display, ..
    } = request
    else {
        bail!("PairRequest expected");
    };
    ensure!(
        id == viewer.id().to_string(),
        "PairRequest names another viewer"
    );
    ensure!(
        sas_display == expected_sas,
        "Devices window SAS does not match the transcript SAS"
    );

    harness
        .ui
        .commands
        .send(DevicesCommand::Allow(id))
        .map_err(|_| anyhow!("Devices command channel closed"))?;
    next_json(harness, &mut admitted.ws, "pair-done", "pair-done")?;
    let done = harness.devices_event("PairDone", |event| {
        matches!(event, DevicesEvent::PairDone { .. })
    })?;
    let DevicesEvent::PairDone { id, .. } = done else {
        bail!("PairDone expected");
    };
    ensure!(
        id == viewer.id().to_string(),
        "PairDone names another viewer"
    );

    ensure!(
        !harness.host.application_ready(),
        "pairing must never enable application traffic"
    );
    let pairing_epoch = admitted.epoch.clone();
    drop(admitted);
    Ok(pairing_epoch)
}

/// Pumps the host until the relay's `pcs.json` lists the viewer as admitted for this PC. The
/// admit is queued when pairing finishes (`ended()` on `PairFinished`), so the wait needs no
/// fixed sleep.
fn wait_for_admitted(
    harness: &mut Harness,
    state_dir: &Path,
    host_id: &str,
    viewer_id: &str,
) -> Result<()> {
    let until = Instant::now() + STEP;
    loop {
        harness.pump()?;
        if admitted_on_disk(&state_dir.join("pcs.json"), host_id, viewer_id) {
            return Ok(());
        }
        ensure!(
            Instant::now() < until,
            "relay state does not list the viewer as admitted for this PC"
        );
        thread::sleep(SLICE);
    }
}

/// A missing or unparsable file is not an answer yet. The relay writes it atomically.
fn admitted_on_disk(path: &Path, host_id: &str, viewer_id: &str) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    value["pcs"].as_array().is_some_and(|pcs| {
        pcs.iter().any(|pc| {
            pc["id"].as_str() == Some(host_id)
                && pc["admitted"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(viewer_id)))
        })
    })
}

/// Reconnects the trusted viewer normally. Checks that the epoch rotated, then the encrypted
/// caps and ping/pong.
fn reconnect_and_ping(
    harness: &mut Harness,
    target: &Target,
    viewer: &ViewerIdentity,
    pairing_epoch: &str,
) -> Result<()> {
    harness.peer_left = false;
    let mut admitted = register(harness, target, viewer, None)
        .context("trusted viewer was refused by the relay after its admit was persisted")?;
    ensure!(
        !admitted.pairing,
        "trusted viewer must not be admitted in pairing mode"
    );
    ensure!(
        admitted.epoch != pairing_epoch,
        "normal reconnect did not rotate the media epoch"
    );
    let Handshake {
        transcript,
        secret,
        pc_ephemeral,
    } = handshake(
        harness,
        &mut admitted,
        target,
        viewer,
        &["control-aead-v1", "dtls-bind-v1"],
    )?;

    let shared = secret
        .diffie_hellman(&pc_ephemeral)
        .context("viewer ephemeral did not agree with the PC")?;
    let (mut sealer, mut opener) = CtlKeys::derive(&shared, &transcript).channel(Side::Viewer);

    let caps = open_ctl(harness, &mut admitted.ws, &mut opener, "caps", 0)?;
    // Decision 24: established, but not live until the viewer latches WebRTC or WSS.
    ensure!(
        harness.host.viewer_epoch().is_some() && !harness.host.application_ready(),
        "normal handshake did not establish, or went live before the transport latch"
    );
    let listed = caps["caps"].as_array().context("caps is not an array")?;
    let has = |cap: &str| listed.iter().any(|item| item.as_str() == Some(cap));
    ensure!(
        has("control-aead-v1") && has("dtls-bind-v1"),
        "encrypted caps do not list the PC's capabilities"
    );

    let (seq, data) = sealer.seal("ping", json!({"type": "ping"}).to_string().as_bytes())?;
    send_json(
        &mut admitted.ws,
        json!({"type": "ctl", "kind": "ping", "seq": seq, "data": b64(&data)}),
    )?;
    // The PC's first frame was caps (seq 0), so its pong is seq 1.
    let pong = open_ctl(harness, &mut admitted.ws, &mut opener, "pong", 1)?;
    ensure!(
        pong["type"].as_str() == Some("pong"),
        "encrypted reply is not a pong"
    );

    // Earlier PeerLeft events are expected: the end of pairing and the reconnect's peer-joined
    // both park the previous viewer. Only a teardown after the pong is a failure.
    harness.peer_left = false;
    harness.settle()?;
    ensure!(
        harness.host.viewer_epoch() == Some(admitted.epoch.as_str()),
        "host does not hold the reconnected viewer's epoch"
    );
    ensure!(
        !harness.peer_left,
        "the PC tore down the viewer after the encrypted pong"
    );
    Ok(())
}

struct Admitted {
    ws: WebSocket<TcpStream>,
    epoch: String,
    pairing: bool,
}

/// The viewer's signed hello and register-proof. Returns the hello-ok, with the epoch kept as
/// the relay's text.
fn register(
    harness: &mut Harness,
    target: &Target,
    viewer: &ViewerIdentity,
    pair: Option<[u8; 32]>,
) -> Result<Admitted> {
    let mut ws = connect_viewer(target)?;
    let mut hello = json!({
        "type": "hello",
        "role": "viewer",
        "id": viewer.id().to_string(),
        "public": b64(&viewer.public_key()),
    });
    if let Some(pair) = pair {
        hello["pair"] = json!(b64(&pair));
    }
    send_json(&mut ws, hello)?;

    let challenge = next_json(harness, &mut ws, "register-challenge", "register-challenge")?;
    let nonce: [u8; 32] = decode_array(text_field(&challenge, "nonce")?)?;
    let signature = viewer.sign_relay_proof(Role::Viewer, target.origin, &nonce);
    send_json(
        &mut ws,
        json!({"type": "register-proof", "signature": b64(&signature)}),
    )?;

    let ok = next_json(harness, &mut ws, "hello-ok", "hello-ok")?;
    let epoch = text_field(&ok, "mediaEpoch")?;
    ensure!(
        is_relay_epoch(epoch),
        "hello-ok mediaEpoch is not 32 lowercase hex"
    );
    let pairing = ok.get("pairing").and_then(Value::as_bool).unwrap_or(false);
    Ok(Admitted {
        ws,
        epoch: epoch.to_owned(),
        pairing,
    })
}

struct Handshake {
    /// The transcript hash `H`.
    transcript: [u8; 32],
    secret: EphemeralSecret,
    pc_ephemeral: [u8; 32],
}

/// Viewer side of auth-hello, auth-host and auth-finish. The transcript uses the epoch from this
/// viewer's own hello-ok, byte for byte.
fn handshake(
    harness: &mut Harness,
    admitted: &mut Admitted,
    target: &Target,
    viewer: &ViewerIdentity,
    caps: &[&str],
) -> Result<Handshake> {
    let viewer_nonce = random32();
    let secret = EphemeralSecret::from_bytes(random32());
    let viewer_ephemeral = secret.public_key();
    send_json(
        &mut admitted.ws,
        json!({
            "type": "auth-hello",
            "id": viewer.id().to_string(),
            "public": b64(&viewer.public_key()),
            "caps": caps,
            "nonce": b64(&viewer_nonce),
            "ephemeral": b64(&viewer_ephemeral),
        }),
    )?;

    let auth_host = next_json(harness, &mut admitted.ws, "auth-host", "auth-host")?;
    ensure!(
        text_field(&auth_host, "id")? == target.host_id,
        "auth-host names another PC"
    );
    let pc_public: [u8; 32] = decode_array(text_field(&auth_host, "public")?)?;
    ensure!(
        pc_public == target.host_public,
        "auth-host public key is not the host's"
    );
    let pc_nonce: [u8; 32] = decode_array(text_field(&auth_host, "nonce")?)?;
    let pc_ephemeral: [u8; 32] = decode_array(text_field(&auth_host, "ephemeral")?)?;
    let pc_signature: [u8; 64] = decode_array(text_field(&auth_host, "signature")?)?;

    let transcript = Transcript {
        relay_origin: target.origin,
        epoch: admitted.epoch.as_str(),
        viewer_id: viewer.id(),
        viewer_public: viewer.public_key(),
        pc_id: DeviceId::parse(target.host_id)?,
        pc_public,
        viewer_nonce,
        pc_nonce,
        viewer_ephemeral,
        pc_ephemeral,
    }
    .hash();
    verify_auth_host(&pc_public, &transcript, &pc_signature)
        .context("auth-host signature does not verify")?;

    let finish = sign_auth_finish(viewer, &transcript, &pc_signature);
    send_json(
        &mut admitted.ws,
        json!({"type": "auth-finish", "signature": b64(&finish)}),
    )?;
    Ok(Handshake {
        transcript,
        secret,
        pc_ephemeral,
    })
}

/// Reads the next ctl frame, checks its kind and sequence, opens it, and checks that the
/// plaintext's `type` equals the kind.
fn open_ctl(
    harness: &mut Harness,
    ws: &mut WebSocket<TcpStream>,
    opener: &mut CtlOpener,
    kind: &str,
    seq: u64,
) -> Result<Value> {
    let frame = next_json(harness, ws, kind, "ctl")?;
    ensure!(
        frame["kind"].as_str() == Some(kind),
        "ctl frame kind is not {kind}"
    );
    ensure!(
        frame["seq"].as_u64() == Some(seq),
        "ctl frame for {kind} has an unexpected sequence number"
    );
    let data: Vec<u8> = decode_b64(text_field(&frame, "data")?)?;
    let plain = opener
        .open(kind, seq, &data)
        .with_context(|| format!("encrypted {kind} did not open"))?;
    let value: Value =
        serde_json::from_slice(&plain).with_context(|| format!("{kind} plaintext is not JSON"))?;
    ensure!(
        value["type"].as_str() == Some(kind),
        "ctl plaintext type does not match kind {kind}"
    );
    Ok(value)
}

fn is_relay_epoch(text: &str) -> bool {
    text.len() == 32
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Host signaling plus the Devices channel, pumped together. Each `pump` is one bounded host poll.
struct Harness {
    host: Signaling,
    ui: UiEnds,
    peer_left: bool,
}

impl Harness {
    fn pump(&mut self) -> Result<()> {
        for event in self.host.poll()? {
            self.peer_left |= matches!(event, SignalEvent::PeerLeft);
        }
        Ok(())
    }

    fn settle(&mut self) -> Result<()> {
        let until = Instant::now() + SETTLE;
        while Instant::now() < until {
            self.pump()?;
            thread::sleep(SLICE);
        }
        Ok(())
    }

    /// Pumps the host until a Devices event matches. Other events are discarded.
    fn devices_event(
        &mut self,
        what: &str,
        wanted: impl Fn(&DevicesEvent) -> bool,
    ) -> Result<DevicesEvent> {
        let until = Instant::now() + STEP;
        loop {
            self.pump()?;
            while let Ok(event) = self.ui.events.try_recv() {
                if wanted(&event) {
                    return Ok(event);
                }
            }
            ensure!(
                Instant::now() < until,
                "timed out waiting for Devices event {what}"
            );
            thread::sleep(SLICE);
        }
    }
}

/// Reads one text frame from the viewer socket, pumping the host while it waits.
fn next_text(harness: &mut Harness, ws: &mut WebSocket<TcpStream>, what: &str) -> Result<String> {
    let until = Instant::now() + STEP;
    loop {
        harness.pump()?;
        match ws.read() {
            Ok(Message::Text(text)) => return Ok(text.as_str().to_owned()),
            Ok(Message::Close(_)) => {
                bail!("relay closed the viewer socket while waiting for {what}")
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => bail!("viewer socket failed while waiting for {what}: {error}"),
        }
        ensure!(Instant::now() < until, "timed out waiting for {what}");
    }
}

/// Reads one JSON text frame and checks its `type`. The message is never echoed.
fn next_json(
    harness: &mut Harness,
    ws: &mut WebSocket<TcpStream>,
    what: &str,
    kind: &str,
) -> Result<Value> {
    let text = next_text(harness, ws, what)?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("{what} is not JSON"))?;
    ensure!(
        value["type"].as_str() == Some(kind),
        "expected type {kind} while waiting for {what}"
    );
    Ok(value)
}

fn send_json(ws: &mut WebSocket<TcpStream>, value: Value) -> Result<()> {
    ws.send(Message::text(value.to_string()))
        .context("send to the relay")
}

/// Opens the viewer socket on the keyed route `/ws?pc=ID`, which the relay requires.
fn connect_viewer(target: &Target) -> Result<WebSocket<TcpStream>> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, target.port));
    let stream = TcpStream::connect_timeout(&addr, SOCKET_TIMEOUT).context("connect viewer")?;
    stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
    stream.set_nodelay(true)?;
    let url = format!("ws://127.0.0.1:{}/ws?pc={}", target.port, target.host_id);
    let (ws, _) = tungstenite::client::client(url, stream)
        .map_err(|error| anyhow!("viewer websocket handshake failed: {error}"))?;
    ws.get_ref().set_read_timeout(Some(SLICE))?;
    Ok(ws)
}

/// The relay, served in-process on its own multi-threaded tokio runtime, so the blocking host and
/// viewer code in this test can talk to it. Dropping it shuts the runtime (and the relay) down.
struct Relay {
    port: u16,
    origin: String,
    token: String,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Relay {
    fn start(work: &TempDir) -> Result<Self> {
        let assets = work.join("assets");
        let state = work.join("state");
        fs::create_dir_all(&assets)?;
        fs::create_dir_all(&state)?;
        fs::write(
            assets.join("index.html"),
            "<!doctype html><title>bp-relay test</title>",
        )?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("relay runtime")?;
        // Bound before the relay starts, so the port is never released and taken by another
        // process in between.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let listener = {
            let _guard = runtime.enter();
            tokio::net::TcpListener::from_std(listener).context("relay listener")?
        };
        let origin = format!("http://{addr}");
        let token = random_hex();
        let config = Config {
            public_origin: origin.clone(),
            listen: addr,
            assets_dir: assets,
            state_dir: state,
            token: Secret::new(token.clone()),
            admin_token: Secret::new(random_hex()),
            max_connections: 32,
            turn: None,
            ntfy: None,
            approval_ttl_s: 90,
            trusted_proxy_cidrs: Vec::new(),
            // The relay's production hold (bp_relay_core::gate::GATE_HOLD_MS).
            gate_hold_ms: 30_000,
        };
        runtime.spawn(async move {
            if let Err(error) = bp_relay_server::server::run(&config, listener).await {
                eprintln!("in-process relay stopped: {error}");
            }
        });
        Ok(Self {
            port: addr.port(),
            origin,
            token,
            runtime: Some(runtime),
        })
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn random32() -> [u8; 32] {
    let mut out = [0u8; 32];
    OsRng.fill_bytes(&mut out);
    out
}

fn random_hex() -> String {
    random32()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn text_field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("field {key} is missing or not a string"))
}

/// Canonical base64url: a value that does not re-encode to the same text is refused.
fn decode_b64(text: &str) -> Result<Vec<u8>> {
    let bytes = URL_SAFE_NO_PAD
        .decode(text)
        .context("field is not base64url")?;
    ensure!(
        URL_SAFE_NO_PAD.encode(&bytes) == text,
        "field is not canonical base64url"
    );
    Ok(bytes)
}

fn decode_array<const N: usize>(text: &str) -> Result<[u8; N]> {
    decode_b64(text)?
        .try_into()
        .map_err(|_| anyhow!("field has the wrong length"))
}

fn seed_from_hex(text: &str) -> Result<[u8; 32]> {
    ensure!(text.len() == 64, "fixture seed is not 32 bytes");
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * index..2 * index + 2], 16)
            .context("fixture seed is not hex")?;
    }
    Ok(out)
}

/// A fresh directory under the system temp folder, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = env::temp_dir().join(format!("bp-dk2-relay-{}-{nanos}", std::process::id()));
        fs::create_dir(&path).with_context(|| format!("create {}", path.display()))?;
        Ok(Self(path))
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Sets environment variables for the test and restores the previous values on drop. This test
/// binary has one test, so nothing else reads them concurrently.
struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn set(&mut self, key: &'static str, value: impl AsRef<OsStr>) {
        self.0.push((key, env::var_os(key)));
        env::set_var(key, value);
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, previous) in self.0.drain(..).rev() {
            match previous {
                Some(value) => env::set_var(key, value),
                None => env::remove_var(key),
            }
        }
    }
}
