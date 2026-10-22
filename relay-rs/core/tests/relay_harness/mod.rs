//! Shared harness for the relay_* tests. It drives the sans-IO `Relay` the way
//! an adapter does: a deterministic RNG, a manual clock, and helpers that
//! register hosts, open viewers and data sockets, and read the actions back.

#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bp_device_key::{DeviceIdentity, Role as KeyRole};
use bp_relay_core::{
    Action, ConnId, CoreConfig, PcTable, Reject, Relay, RelayRng, Route, UpgradeRequest,
};
use serde_json::{Value, json};

pub const ORIGIN: &str = "https://relay.example.test";
pub const TOKEN: &str = "relay-token-for-tests-0001";
pub const START_MS: u64 = 1_000_000;

/// The seed of the viewer identity every test host admits by default.
pub const VIEWER_SEED: u8 = 200;

/// The client address a harness upgrade comes from unless a test changes it.
pub const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 20));

/// Deterministic RNG (splitmix64). Every run with the same seed yields the same bytes.
pub struct TestRng(u64);

impl TestRng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl RelayRng for TestRng {
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

/// A relay plus its clock and RNG. `now` moves only when a test advances it.
pub struct Harness {
    pub relay: Relay,
    pub rng: TestRng,
    pub now: u64,
    /// The peer address for every upgrade. Its prefix keys the pairing limits.
    pub client: IpAddr,
}

/// A registered host: its signalling socket, PC id and ticket.
pub struct Host {
    pub id: ConnId,
    pub pc: String,
    pub ticket: String,
}

/// A viewer: its signalling socket, PC id, media epoch, ticket and iceServers.
pub struct Viewer {
    pub id: ConnId,
    pub pc: String,
    pub epoch: String,
    pub ticket: String,
    pub ice: Value,
}

impl Harness {
    pub fn new() -> Self {
        Self::with(|_| {})
    }

    /// A harness whose config is tweaked by `tweak` before the relay is built.
    pub fn with(tweak: impl FnOnce(&mut CoreConfig)) -> Self {
        let mut config = CoreConfig::new(ORIGIN, TOKEN);
        tweak(&mut config);
        Self {
            relay: Relay::new(config, PcTable::new()),
            rng: TestRng::new(7),
            now: START_MS,
            client: CLIENT,
        }
    }

    pub fn try_upgrade(&mut self, req: UpgradeRequest) -> Result<ConnId, Reject> {
        self.relay.upgrade(req, self.client, self.now)
    }

    pub fn upgrade(&mut self, req: UpgradeRequest) -> ConnId {
        self.try_upgrade(req).expect("upgrade accepted")
    }

    /// Sends a JSON text frame.
    pub fn text(&mut self, id: ConnId, value: &Value) -> Vec<Action> {
        self.raw(id, &value.to_string())
    }

    /// Sends exact text.
    pub fn raw(&mut self, id: ConnId, text: &str) -> Vec<Action> {
        self.relay.on_text(id, text, self.now, &mut self.rng)
    }

    pub fn binary(&mut self, id: ConnId, bytes: &[u8]) -> Vec<Action> {
        self.relay.on_binary(id, bytes, self.now)
    }

    pub fn close(&mut self, id: ConnId) -> Vec<Action> {
        self.relay.on_close(id)
    }

    /// Moves the clock forward and fires the timeouts that are due.
    pub fn advance(&mut self, ms: u64) -> Vec<Action> {
        self.now += ms;
        self.relay.tick(self.now)
    }

    /// Opens a host signalling socket for `identity` and sends its hello.
    /// Returns the socket and the actions (the challenge, or a refusal).
    pub fn host_hello(
        &mut self,
        identity: &DeviceIdentity,
        token: Option<&str>,
    ) -> (ConnId, Vec<Action>) {
        let pc = identity.id().to_string();
        let id = self.upgrade(UpgradeRequest {
            route: Route::HostWs,
            pc: Some(pc.clone()),
            epoch: None,
            viewer_authorized: false,
        });
        let mut hello = json!({
            "type": "hello",
            "role": "host",
            "id": pc,
            "public": b64(&identity.public_key()),
        });
        if let Some(token) = token {
            hello["token"] = json!(token);
        }
        let actions = self.text(id, &hello);
        (id, actions)
    }

    /// Runs the whole host registration: hello, challenge, proof. Returns the
    /// host and the actions from the proof (persist, hello-ok, buffered frames).
    pub fn register_host(
        &mut self,
        identity: &DeviceIdentity,
        token: Option<&str>,
    ) -> (Host, Vec<Action>) {
        let (id, actions) = self.host_hello(identity, token);
        let nonce = challenge_nonce(&actions, id).expect("register-challenge sent");
        let actions = self.text(id, &proof_msg(identity, ORIGIN, &nonce));
        let ok = hello_ok(&actions, id);
        // Every test host admits the default viewer identity, as a real host
        // publishes its list. The admit's own actions are not part of the result.
        let admit = json!({
            "type": "admit",
            "viewers": [viewer_identity().id().to_string()],
            "remotePairing": false,
        });
        self.text(id, &admit);
        let host = Host {
            id,
            pc: identity.id().to_string(),
            ticket: ok["ticket"].as_str().expect("ticket").to_owned(),
        };
        (host, actions)
    }

    /// Runs the signed viewer hello for `identity` on `pc`: the hello, the
    /// register-challenge, then the register-proof. Returns the socket and the
    /// actions from the proof (hello-ok, or a refusal). `pair` is the pairing
    /// value sent in the hello, if any.
    pub fn viewer_join(
        &mut self,
        identity: &DeviceIdentity,
        pc: &str,
        turn: bool,
        pair: Option<&[u8; 32]>,
    ) -> (ConnId, Vec<Action>) {
        let id = self.upgrade(UpgradeRequest {
            route: Route::Ws,
            pc: Some(pc.to_owned()),
            epoch: None,
            viewer_authorized: true,
        });
        let mut hello = viewer_hello_msg(identity, turn);
        if let Some(pair) = pair {
            hello["pair"] = json!(b64(pair));
        }
        let actions = self.text(id, &hello);
        let nonce = challenge_nonce(&actions, id).expect("register-challenge sent");
        let actions = self.text(id, &viewer_proof_msg(identity, ORIGIN, &nonce));
        (id, actions)
    }

    /// Opens a viewer signalling socket on `pc` with the default viewer identity,
    /// which the test host admits. Returns the viewer and the proof's actions.
    pub fn viewer_hello(&mut self, pc: &str, turn: bool) -> (Viewer, Vec<Action>) {
        let (id, actions) = self.viewer_join(&viewer_identity(), pc, turn, None);
        let ok = hello_ok(&actions, id);
        let viewer = Viewer {
            id,
            pc: pc.to_owned(),
            epoch: ok["mediaEpoch"].as_str().expect("mediaEpoch").to_owned(),
            ticket: ok["ticket"].as_str().expect("ticket").to_owned(),
            ice: ok["iceServers"].clone(),
        };
        (viewer, actions)
    }

    pub fn open_viewer(&mut self, pc: &str) -> Viewer {
        self.viewer_hello(pc, false).0
    }

    /// Opens a data socket and sends its hello with `role` (for example
    /// "media-client") and `ticket`. `route` picks the socket family.
    pub fn data_hello(
        &mut self,
        route: Route,
        pc: &str,
        epoch: &str,
        role: &str,
        ticket: &str,
    ) -> (ConnId, Vec<Action>) {
        let id = self.upgrade(UpgradeRequest {
            route,
            pc: Some(pc.to_owned()),
            epoch: Some(epoch.to_owned()),
            viewer_authorized: true,
        });
        let actions = self.text(
            id,
            &json!({"type": "hello", "role": role, "ticket": ticket}),
        );
        (id, actions)
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

pub fn req(route: Route, pc: Option<&str>, epoch: Option<&str>, viewer: bool) -> UpgradeRequest {
    UpgradeRequest {
        route,
        pc: pc.map(str::to_owned),
        epoch: epoch.map(str::to_owned),
        viewer_authorized: viewer,
    }
}

pub fn identity(seed: u8) -> DeviceIdentity {
    DeviceIdentity::from_seed(&[seed; 32])
}

pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The register-proof a host sends for `nonce`, signed for `origin`.
pub fn proof_msg(identity: &DeviceIdentity, origin: &str, nonce: &[u8; 32]) -> Value {
    let signature = identity.sign_relay_proof(KeyRole::Host, origin, nonce);
    json!({"type": "register-proof", "signature": b64(&signature)})
}

/// The register-proof a viewer sends for `nonce`, signed for `origin`.
pub fn viewer_proof_msg(identity: &DeviceIdentity, origin: &str, nonce: &[u8; 32]) -> Value {
    let signature = identity.sign_relay_proof(KeyRole::Viewer, origin, nonce);
    json!({"type": "register-proof", "signature": b64(&signature)})
}

/// The signed viewer hello for `identity`, without the optional `pair`.
pub fn viewer_hello_msg(identity: &DeviceIdentity, turn: bool) -> Value {
    json!({
        "type": "hello",
        "role": "viewer",
        "id": identity.id().to_string(),
        "public": b64(&identity.public_key()),
        "turn": turn,
    })
}

/// The viewer identity every test host admits.
pub fn viewer_identity() -> DeviceIdentity {
    identity(VIEWER_SEED)
}

/// Every text frame sent to `id`, as exact strings.
pub fn sent_text(actions: &[Action], id: ConnId) -> Vec<String> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::SendText(to, text) if *to == id => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Every text frame sent to `id`, parsed as JSON.
pub fn sent_json(actions: &[Action], id: ConnId) -> Vec<Value> {
    sent_text(actions, id)
        .iter()
        .map(|text| serde_json::from_str(text).expect("relay sends JSON"))
        .collect()
}

/// The `type` of every text frame sent to `id`.
pub fn sent_types(actions: &[Action], id: ConnId) -> Vec<String> {
    sent_json(actions, id)
        .iter()
        .map(|m| m["type"].as_str().unwrap_or_default().to_owned())
        .collect()
}

pub fn sent_binary(actions: &[Action], id: ConnId) -> Vec<Vec<u8>> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::SendBinary(to, bytes) if *to == id => Some(bytes.clone()),
            _ => None,
        })
        .collect()
}

/// Every close sent to `id`, as (code, reason).
pub fn closes(actions: &[Action], id: ConnId) -> Vec<(u16, String)> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Close(to, code, reason) if *to == id => Some((*code, reason.clone())),
            _ => None,
        })
        .collect()
}

/// The first close sent to `id`, if any.
pub fn closed(actions: &[Action], id: ConnId) -> Option<(u16, String)> {
    closes(actions, id).into_iter().next()
}

/// The index of the first PersistPcs action.
pub fn persisted_at(actions: &[Action]) -> Option<usize> {
    actions
        .iter()
        .position(|action| matches!(action, Action::PersistPcs(_)))
}

/// The hello-ok sent to `id`. Panics when there is none.
pub fn hello_ok(actions: &[Action], id: ConnId) -> Value {
    sent_json(actions, id)
        .into_iter()
        .find(|m| m["type"] == "hello-ok")
        .expect("hello-ok sent")
}

/// The register challenge nonce sent to `id`, if any.
pub fn challenge_nonce(actions: &[Action], id: ConnId) -> Option<[u8; 32]> {
    let message = sent_json(actions, id)
        .into_iter()
        .find(|m| m["type"] == "register-challenge")?;
    let bytes = URL_SAFE_NO_PAD.decode(message["nonce"].as_str()?).ok()?;
    bytes.try_into().ok()
}

/// The PC ids in a PersistPcs action, if the action is one.
pub fn persisted_pcs(actions: &[Action]) -> Option<Vec<String>> {
    actions.iter().find_map(|action| match action {
        Action::PersistPcs(table) => Some(table.records.keys().cloned().collect()),
        _ => None,
    })
}
