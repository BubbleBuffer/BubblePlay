//! Stage B3: the client's real relay path against the native Rust relay
//! (`bp-relay-server`) started in-process, with the test playing the PC
//! (`tests/support/relay_pc.rs`). The client runs on its own threads exactly as
//! `--dev-relay` runs it (`net::run`, and `pair_into` for `--pair`); the PC and the relay
//! share the test's single-threaded runtime, so the test only waits asynchronously.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bp_device_key::phase2::{PairingCode, Sas, Side};
use bp_device_key::DeviceIdentity;
use futures_util::SinkExt;
use serde_json::{json, Value};

use super::relay_pc::*;
use super::{pair_into, viewer_auth_in, DevOptions};
use crate::net::phase2::{test_pc, Opened, CAP_CONTROL_AEAD, CAP_DTLS_BIND, MAX_NAME_CHARS};
use crate::net::CAP_CLIENT_LOGS;
use crate::net::{
    Codec, CtlLogEvent, CtlLogHook, DecoderFeedback, InputLink, NetConfig, NetError, NetEvent,
    TransportChoice, TransportStats, ViewerAuth,
};
use crate::store::pcs::PcStore;

/// How long the test waits for the client to reach a state.
const CLIENT_WAIT: Duration = Duration::from_secs(15);

/// Poll `check` (without blocking the runtime the relay runs on) until it yields.
async fn until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + CLIENT_WAIT;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The client's net thread, as the windowed and headless sessions start it.
struct Client {
    events: mpsc::Receiver<NetEvent>,
    feedback: mpsc::Sender<DecoderFeedback>,
    stop: Arc<AtomicBool>,
    stats: Arc<TransportStats>,
    thread: Option<JoinHandle<Result<(), NetError>>>,
}

impl Client {
    fn start(config: NetConfig) -> Client {
        let (events_tx, events) = mpsc::sync_channel(256);
        let (feedback, feedback_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(TransportStats::new());
        let thread = {
            let (stop, stats) = (stop.clone(), stats.clone());
            thread::spawn(move || crate::net::run(config, events_tx, feedback_rx, stop, stats))
        };
        Client {
            events,
            feedback,
            stop,
            stats,
            thread: Some(thread),
        }
    }

    /// The client's result once it ended by itself (a fatal error).
    async fn ended(&mut self) -> Result<(), NetError> {
        let thread = self.thread.take().expect("client thread");
        until("the client to end", || thread.is_finished().then_some(())).await;
        thread.join().expect("client thread panicked")
    }

    /// Ask the client to stop and return its result.
    async fn finish(&mut self) -> Result<(), NetError> {
        self.stop.store(true, Ordering::SeqCst);
        self.ended().await
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn viewer_identity(seed: u8) -> DeviceIdentity {
    DeviceIdentity::from_seed(&[seed; 32])
}

/// A WSS config for `viewer` against `relay`, trusting `paired`.
fn wss_config(
    relay: &Relay,
    viewer: DeviceIdentity,
    paired: Vec<(bp_device_key::DeviceId, [u8; 32])>,
) -> NetConfig {
    let (pc, _) = test_pc::fixture_pc();
    let mut config = NetConfig::new(
        relay.origin.clone(),
        Codec::H264,
        ViewerAuth::new(viewer, Some(pc)).with_paired(paired),
    );
    config.transport = TransportChoice::Wss;
    config
}

/// The PC's `caps` reply (its first ctl, seq 0).
async fn pc_caps(host: &mut Ws, pc: &mut Viewer) {
    let seq = pc
        .send(
            host,
            json!({ "type": "caps", "caps": [CAP_CONTROL_AEAD, CAP_DTLS_BIND] }),
        )
        .await;
    assert_eq!(seq, 0, "the PC's first ctl is seq 0");
}

/// The viewer's first ctl: `caps` with control-aead and a name (decision 19).
async fn viewer_caps(host: &mut Ws, pc: &mut Viewer) -> Value {
    let Opened::Text { kind, text } = pc.recv(host).await else {
        panic!("the viewer's first ctl is text");
    };
    assert_eq!(kind, "caps", "the viewer's first ctl is caps");
    let caps: Value = serde_json::from_str(&text).expect("caps JSON");
    assert!(caps["caps"]
        .as_array()
        .expect("caps list")
        .contains(&json!(CAP_CONTROL_AEAD)));
    let name = caps["name"].as_str().expect("caps name");
    assert!(!name.trim().is_empty() && name.chars().count() <= MAX_NAME_CHARS);
    caps
}

/// Tests 1 and 5: an admitted viewer with the PC's key pinned. hello, register-proof and
/// hello-ok with a ticket; the handshake; caps first on both sides; the media-viewer
/// ticket hello; then a JSON ctl PC->viewer and a binary "input" ctl viewer->PC.
#[tokio::test(flavor = "current_thread")]
async fn an_admitted_viewer_with_a_pinned_key_connects_and_exchanges_ctl_both_ways() {
    let relay = Relay::start().await;
    let viewer = viewer_identity(11);
    let viewer_id = viewer.id().to_string();
    let (mut host, host_ok) = register_host(&relay, std::slice::from_ref(&viewer_id)).await;
    let host_ticket = host_ok["ticket"].as_str().expect("host ticket").to_owned();

    let input = InputLink::new(Arc::new(|| {}));
    let mut config = wss_config(&relay, viewer, vec![test_pc::fixture_pc()]);
    config.input = Some(input.clone());
    let controls = config.controls.clone();
    let mut client = Client::start(config);

    let mut pc = handshake(&mut host, &relay.origin, true).await;
    assert!(!pc.pairing);
    assert_eq!(pc.hello["id"], viewer_id.as_str());
    viewer_caps(&mut host, &mut pc).await;
    pc_caps(&mut host, &mut pc).await;

    // The client opens /media-ws with the ticket from its hello-ok before it sends
    // `start`; the epoch is the relay's mediaEpoch, unchanged.
    let start = loop {
        let control = pc.recv_text(&mut host, "media-control").await;
        if control["action"] == "start" {
            break control;
        }
    };
    assert_eq!(start["epoch"], pc.epoch.as_str());
    let (mut media, viewer_present) = host_media(&relay, &host_ticket, &pc.epoch).await;
    assert!(
        viewer_present,
        "the relay accepted the viewer's media-viewer ticket hello"
    );
    media
        .send(tokio_tungstenite::tungstenite::Message::Binary(
            bpv4_frame(true, 7, 1, b"payload-1").into(),
        ))
        .await
        .expect("PC media send");
    let frame = until("the first frame", || {
        client.events.try_iter().find_map(|event| match event {
            NetEvent::Frame(frame) => Some(frame),
            _ => None,
        })
    })
    .await;
    assert_eq!((frame.generation, frame.seq, frame.key), (7, 1, true));
    client
        .feedback
        .send(DecoderFeedback::Presented {
            local_epoch: frame.local_epoch,
            generation: frame.generation,
            seq: frame.seq,
        })
        .expect("feedback");

    // PC -> viewer: a JSON kind after caps, seq 1.
    let list = json!({
        "type": "monitor-list",
        "epoch": pc.epoch,
        "monitors": [{
            "selector": "DISPLAY1", "name": "Main", "width": 1920, "height": 1080,
            "origin_x": 0, "origin_y": 0, "selected": true,
        }],
    });
    assert_eq!(pc.send(&mut host, list).await, 1);
    let monitors = until("the monitor list", || {
        let monitors = controls.snapshot().monitors;
        (!monitors.is_empty()).then_some(monitors)
    })
    .await;
    assert_eq!(monitors[0].selector, "DISPLAY1");
    assert_eq!((monitors[0].width, monitors[0].height), (1920, 1080));

    // Viewer -> PC: binary "input" inside ctl.
    let epoch = until("native input ready", || {
        let snapshot = input.snapshot();
        snapshot.ready.then_some(snapshot.epoch)
    })
    .await;
    let packet = crate::input::protocol::absolute(1, 1000, 2000);
    assert!(input.send(epoch, packet.to_vec()), "input accepted");
    let received = loop {
        if let Opened::Binary { kind, frame } = pc.recv(&mut host).await {
            assert_eq!(kind, "input");
            if frame.first() == packet.first() {
                break frame;
            }
        }
    };
    assert_eq!(
        received[3..],
        packet[3..],
        "the PC got the absolute position"
    );

    client.finish().await.expect("a clean stop");
    assert_eq!(client.stats.snapshot().attempts, 1);
}

/// Test 2: a PC answering with a key other than the pinned one is refused after
/// auth-host: no auth-finish, a fatal error naming the changed key, no retry.
#[tokio::test(flavor = "current_thread")]
async fn a_pinned_key_mismatch_is_refused_after_auth_host() {
    let relay = Relay::start().await;
    let viewer = viewer_identity(12);
    let (mut host, _) = register_host(&relay, &[viewer.id().to_string()]).await;
    let (pc_id, _) = test_pc::fixture_pc();
    let mut client = Client::start(wss_config(&relay, viewer, vec![(pc_id, [7; 32])]));

    let _pc = handshake(&mut host, &relay.origin, false).await;
    let error = client.ended().await.expect_err("the client refuses");
    assert!(
        matches!(&error, NetError::DeviceKeys(message) if message.contains("PC key changed")),
        "{error}"
    );
    let after = quiet(&mut host, Duration::from_millis(300)).await;
    assert!(
        after
            .as_ref()
            .is_none_or(|value| value["type"] != "auth-finish"),
        "no auth-finish after a key mismatch: {after:?}"
    );
    assert_eq!(client.stats.snapshot().attempts, 1);
}

/// Test 3: the real `--pair` path. pair-open with the admit hash, both MACs verify, the
/// same number on both sides, pair-done pins the PC in pcs.json; then the PC admits the
/// viewer and a normal connect with the stored key works.
#[tokio::test(flavor = "current_thread")]
async fn pairing_pins_the_pc_and_a_normal_connect_works_after() {
    let relay = Relay::start().await;
    let (mut host, _) = register_host(&relay, &[]).await;
    let pc_id = pc_identity().id();
    let code = PairingCode::generate().expect("pairing code");
    send_json(
        &mut host,
        json!({ "type": "pair-open", "admit": URL_SAFE_NO_PAD.encode(code.admit_hash(&pc_id)) }),
    )
    .await;
    sync(&mut host).await;

    let dir = std::env::temp_dir().join(format!(
        "bp-client-e2e-pcs-{}-{}",
        std::process::id(),
        relay.addr.port()
    ));
    std::fs::create_dir_all(&dir).expect("pcs dir");
    let args = |extra: &[&str]| {
        let mut args = vec![
            "--dev-relay".to_owned(),
            relay.origin.clone(),
            "--pc".to_owned(),
            pc_id.to_string(),
        ];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        DevOptions::from_args(args)
            .expect("dev options")
            .expect("dev mode")
    };
    let pair_options = args(&["--pair", code.as_str()]);
    let (lines_tx, lines) = mpsc::channel::<String>();
    let pairing = {
        let dir = dir.clone();
        let code = code.as_str().to_owned();
        thread::spawn(move || {
            let store = PcStore::in_dir(&dir);
            pair_into(
                &pair_options,
                &code,
                viewer_identity(13),
                &store,
                &mut |line: &str| {
                    let _ = lines_tx.send(line.to_owned());
                },
            )
        })
    };

    let pc = handshake(&mut host, &relay.origin, true).await;
    assert!(pc.pairing, "the relay opened a pairing session");
    let proof = next_of(&mut host, "pair-proof").await;
    let mac = URL_SAFE_NO_PAD
        .decode(proof["mac"].as_str().expect("pair-proof mac"))
        .expect("base64url mac");
    code.pair_key()
        .verify_mac(Side::Viewer, &pc.session.h, &mac)
        .expect("the viewer's pair-proof verifies");
    send_text(&mut host, pc.session.pair_proof(&code)).await;
    let number = until("the number to compare", || lines.try_recv().ok()).await;
    assert_eq!(number, Sas::from_transcript_hash(&pc.session.h).display());
    assert_eq!(
        until("the confirm line", || lines.try_recv().ok()).await,
        "Confirm on the PC"
    );
    send_json(&mut host, json!({ "type": "pair-done" })).await;
    until("pairing to end", || pairing.is_finished().then_some(())).await;
    let entry = pairing
        .join()
        .expect("pairing thread")
        .expect("pairing succeeds");
    let pc_public = pc_identity().public_key();
    assert_eq!(entry.id, pc_id.to_string());
    assert_eq!(entry.public, URL_SAFE_NO_PAD.encode(pc_public));
    assert_eq!(entry.relay, relay.origin);
    let store = PcStore::in_dir(&dir);
    assert!(dir.join("pcs.json").is_file());
    let pinned: Vec<(String, [u8; 32])> = store
        .pinned()
        .expect("pcs.json")
        .into_iter()
        .map(|(id, public)| (id.to_string(), public))
        .collect();
    assert_eq!(pinned, vec![(pc_id.to_string(), pc_public)]);

    // The pairing connection ends; the PC admits the newly paired viewer and a normal
    // connect trusts the stored key.
    next_of(&mut host, "peer-left").await;
    let viewer_id = viewer_identity(13).id().to_string();
    admit(&mut host, &[viewer_id]).await;
    let auth = viewer_auth_in(&args(&[]), viewer_identity(13), &store).expect("viewer auth");
    let mut config = NetConfig::new(relay.origin.clone(), Codec::H264, auth);
    config.transport = TransportChoice::Wss;
    let mut client = Client::start(config);
    let mut pc = handshake(&mut host, &relay.origin, true).await;
    assert!(!pc.pairing);
    viewer_caps(&mut host, &mut pc).await;
    pc_caps(&mut host, &mut pc).await;
    pc.recv_text(&mut host, "media-control").await;
    client.finish().await.expect("a clean stop");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Test 4: a viewer the PC did not admit, without a pairing code: the relay closes
/// /ws with 1008 and the client ends with a fatal error, without retrying.
#[tokio::test(flavor = "current_thread")]
async fn an_unlisted_viewer_is_closed_with_1008_and_that_is_fatal() {
    let relay = Relay::start().await;
    let (mut host, _) = register_host(&relay, &[]).await;
    let mut client = Client::start(wss_config(
        &relay,
        viewer_identity(14),
        vec![test_pc::fixture_pc()],
    ));
    let error = client.ended().await.expect_err("the client gives up");
    assert!(matches!(error, NetError::NotAdmitted), "{error}");
    assert_eq!(client.stats.snapshot().attempts, 1);
    let seen = quiet(&mut host, Duration::from_millis(200)).await;
    assert!(
        seen.as_ref()
            .is_none_or(|value| value["type"] != "peer-joined" && value["type"] != "auth-hello"),
        "the PC never saw the unlisted viewer: {seen:?}"
    );
}

/// The client-log-upload contract vectors by name (fixtures/client-log-upload-contract.json).
fn contract(name: &str) -> Value {
    let doc: Value = serde_json::from_str(include_str!(
        "../../../fixtures/client-log-upload-contract.json"
    ))
    .expect("contract fixture");
    doc["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["name"] == name)
        .unwrap_or_else(|| panic!("fixture message {name}"))["json"]
        .clone()
}

/// The next event of the hook's channel (async wait; the net thread produces them).
async fn hook_event(events: &mpsc::Receiver<CtlLogEvent>) -> CtlLogEvent {
    until("a log hook event", || events.try_recv().ok()).await
}

fn message_json(event: CtlLogEvent) -> Value {
    let CtlLogEvent::Message(text) = event else {
        panic!("expected a Message event, got {event:?}");
    };
    serde_json::from_str(&text).expect("routed JSON")
}

/// The PC's `caps` reply advertising `client-logs-v1`.
async fn pc_caps_with_logs(host: &mut Ws, pc: &mut Viewer) {
    let seq = pc
        .send(
            host,
            json!({ "type": "caps", "caps": [CAP_CONTROL_AEAD, CAP_DTLS_BIND, CAP_CLIENT_LOGS] }),
        )
        .await;
    assert_eq!(seq, 0, "the PC's first ctl is seq 0");
}

/// A configured hook advertises `client-logs-v1` in the auth-hello and the ctl caps, emits
/// Opened with the PC's caps, routes the host's contract messages and carries the client's
/// batch and feedback messages to the PC in order, sealed as their own ctl kinds.
#[tokio::test(flavor = "current_thread")]
async fn the_log_hook_carries_the_upload_contract_both_ways() {
    let relay = Relay::start().await;
    let viewer = viewer_identity(21);
    let (mut host, _) = register_host(&relay, &[viewer.id().to_string()]).await;
    let (hook, events) = CtlLogHook::channel();
    let mut config = wss_config(&relay, viewer, vec![test_pc::fixture_pc()]);
    config.log_hook = Some(hook.clone());
    let mut client = Client::start(config);

    let mut pc = handshake(&mut host, &relay.origin, true).await;
    assert!(
        pc.hello["caps"]
            .as_array()
            .expect("auth-hello caps")
            .contains(&json!(CAP_CLIENT_LOGS)),
        "auth-hello advertises the capability: {}",
        pc.hello
    );
    let caps = viewer_caps(&mut host, &mut pc).await;
    assert!(caps["caps"]
        .as_array()
        .unwrap()
        .contains(&json!(CAP_CLIENT_LOGS)));
    assert!(
        !hook.send(contract("logs-batch").to_string()),
        "refused before Opened"
    );
    pc_caps_with_logs(&mut host, &mut pc).await;

    let CtlLogEvent::Opened { pc_id, peer_caps } = hook_event(&events).await else {
        panic!("the first event is Opened");
    };
    assert_eq!(pc_id, test_pc::fixture_pc().0.to_string());
    assert!(peer_caps.iter().any(|cap| cap == CAP_CLIENT_LOGS));
    assert!(hook.is_connected());

    // Host -> client: logs-state.
    let state = contract("logs-state");
    pc.send(&mut host, state.clone()).await;
    assert_eq!(message_json(hook_event(&events).await), state);

    // Client -> host: a batch; the PC acks; the ack comes back as a Message.
    let batch = contract("logs-batch");
    assert!(hook.send(batch.to_string()));
    assert_eq!(pc.recv_text(&mut host, "logs-batch").await, batch);
    assert_eq!(hook.buffered_bytes(), 0, "the session took it");
    let ack = contract("logs-ack-ok");
    pc.send(&mut host, ack.clone()).await;
    assert_eq!(message_json(hook_event(&events).await), ack);

    // Feedback: begin and chunk in order, then the ack.
    let (begin, chunk) = (contract("feedback-begin"), contract("feedback-chunk"));
    assert!(hook.send(begin.to_string()) && hook.send(chunk.to_string()));
    assert_eq!(pc.recv_text(&mut host, "feedback-begin").await, begin);
    assert_eq!(pc.recv_text(&mut host, "feedback-chunk").await, chunk);
    let feedback_ack = contract("feedback-ack-ok");
    pc.send(&mut host, feedback_ack.clone()).await;
    assert_eq!(message_json(hook_event(&events).await), feedback_ack);

    // Kinds the hook does not route stay with the session's own handling.
    let rejected = client.stats.snapshot().rejected_controls;
    pc.send(&mut host, json!({ "type": "logs-batch", "v": 1 }))
        .await;
    until("the unrouted kind to be rejected", || {
        (client.stats.snapshot().rejected_controls > rejected).then_some(())
    })
    .await;
    assert!(events.try_recv().is_err(), "nothing routed for it");

    client.finish().await.expect("a clean stop");
    assert_eq!(hook_event(&events).await, CtlLogEvent::Closed);
    assert!(!hook.is_connected());
    assert!(!hook.send(batch.to_string()), "refused after Closed");
}

/// A PC without the capability: the client reports Opened (with the PC's caps) but the
/// hook neither sends nor delivers.
#[tokio::test(flavor = "current_thread")]
async fn a_pc_without_the_capability_gets_no_log_messages() {
    let relay = Relay::start().await;
    let viewer = viewer_identity(22);
    let (mut host, _) = register_host(&relay, &[viewer.id().to_string()]).await;
    let (hook, events) = CtlLogHook::channel();
    let mut config = wss_config(&relay, viewer, vec![test_pc::fixture_pc()]);
    config.log_hook = Some(hook.clone());
    let mut client = Client::start(config);

    let mut pc = handshake(&mut host, &relay.origin, true).await;
    viewer_caps(&mut host, &mut pc).await;
    pc_caps(&mut host, &mut pc).await;
    let CtlLogEvent::Opened { peer_caps, .. } = hook_event(&events).await else {
        panic!("Opened");
    };
    assert!(!peer_caps.iter().any(|cap| cap == CAP_CLIENT_LOGS));
    assert!(!hook.send(contract("logs-batch").to_string()));
    pc.send(&mut host, contract("logs-state")).await;
    pc.recv_text(&mut host, "media-control").await;
    client.finish().await.expect("a clean stop");
    assert!(events
        .try_iter()
        .all(|event| !matches!(event, CtlLogEvent::Message(_))));
}

/// A reconnect is a new epoch: Closed for the old one, a new Opened for the next, and the
/// queue starts empty.
#[tokio::test(flavor = "current_thread")]
async fn the_log_hook_reopens_on_each_reconnect() {
    let relay = Relay::start().await;
    let viewer = viewer_identity(23);
    let viewer_id = viewer.id().to_string();
    let (mut host, _) = register_host(&relay, std::slice::from_ref(&viewer_id)).await;
    let (hook, events) = CtlLogHook::channel();
    let mut config = wss_config(&relay, viewer, vec![test_pc::fixture_pc()]);
    config.log_hook = Some(hook.clone());
    let mut client = Client::start(config);

    let mut pc = handshake(&mut host, &relay.origin, true).await;
    viewer_caps(&mut host, &mut pc).await;
    pc_caps_with_logs(&mut host, &mut pc).await;
    assert!(matches!(
        hook_event(&events).await,
        CtlLogEvent::Opened { .. }
    ));
    // The PC goes away and comes back: the client reconnects with a fresh handshake.
    drop(pc);
    drop(host);
    assert_eq!(hook_event(&events).await, CtlLogEvent::Closed);
    assert!(
        !hook.send(contract("logs-batch").to_string()),
        "closed in between"
    );
    let (mut host, _) = register_host(&relay, &[viewer_id]).await;
    let mut pc = handshake(&mut host, &relay.origin, true).await;
    viewer_caps(&mut host, &mut pc).await;
    pc_caps_with_logs(&mut host, &mut pc).await;
    assert!(matches!(
        hook_event(&events).await,
        CtlLogEvent::Opened { .. }
    ));
    let batch = contract("logs-batch");
    assert!(hook.send(batch.to_string()));
    assert_eq!(pc.recv_text(&mut host, "logs-batch").await, batch);
    client.finish().await.expect("a clean stop");
    assert!(client.stats.snapshot().attempts >= 2);
}

/// Without a hook the session neither advertises the capability nor routes the kinds.
#[tokio::test(flavor = "current_thread")]
async fn a_session_without_a_hook_does_not_advertise_or_route_log_messages() {
    let relay = Relay::start().await;
    let viewer = viewer_identity(24);
    let (mut host, _) = register_host(&relay, &[viewer.id().to_string()]).await;
    let mut client = Client::start(wss_config(&relay, viewer, vec![test_pc::fixture_pc()]));

    let mut pc = handshake(&mut host, &relay.origin, true).await;
    assert_eq!(
        pc.hello["caps"],
        json!([CAP_CONTROL_AEAD, CAP_DTLS_BIND]),
        "auth-hello caps are unchanged"
    );
    let caps = viewer_caps(&mut host, &mut pc).await;
    assert_eq!(caps["caps"], json!([CAP_CONTROL_AEAD, CAP_DTLS_BIND]));
    pc_caps_with_logs(&mut host, &mut pc).await;
    pc.send(&mut host, contract("logs-state")).await;
    until("the unrouted kind to be rejected", || {
        (client.stats.snapshot().rejected_controls > 0).then_some(())
    })
    .await;
    client.finish().await.expect("a clean stop");
    assert_eq!(client.stats.snapshot().attempts, 1, "no reconnect");
}
