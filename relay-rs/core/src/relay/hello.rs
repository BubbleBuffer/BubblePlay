//! The first message on every socket: hello, host registration, tickets and
//! data-socket attachment (Go's Relay.hello, routing.go, server.go:473-612).

use bp_device_key::{DeviceId, Role as KeyRole, verify_relay_proof};
use serde_json::{Map, Value, json};

use super::pcs::MAX_PCS;
use super::state::{Kind, Phase, Role, key_for};
use super::types::{Action, ConnId};
use super::util;
use super::{Relay, reasons};
use crate::RelayRng;

impl Relay {
    /// The hello on a socket that has none yet. Anything that fails closes the
    /// socket, with the reason Go uses for that case.
    pub(super) fn on_hello(
        &mut self,
        id: ConnId,
        text: &str,
        now_ms: u64,
        rng: &mut dyn RelayRng,
        out: &mut Vec<Action>,
    ) {
        let Some(conn) = self.open_conn(id) else {
            return;
        };
        let Some(m) = util::parse_object(text) else {
            self.hello_refused(id, "hello.malformed", out);
            return;
        };
        let role_text = util::text_of(&m, "role");
        let known_role = conn.role.hello_roles().contains(&role_text);
        if util::text_of(&m, "type") != "hello" || !known_role {
            self.hello_refused(id, "hello.bad-role", out);
            return;
        }
        match conn.role {
            Role::VersionProbe => self.version_probe(id, &m, out),
            Role::Host => self.host_hello(id, &m, now_ms, rng, out),
            Role::Viewer => self.viewer_hello(id, &m, now_ms, rng, out),
            Role::MediaHost | Role::MediaViewer | Role::FileHost | Role::FileViewer => {
                self.data_hello(id, &m, now_ms, out)
            }
        }
    }

    /// The register-proof that answers the challenge. It is single use: any
    /// answer, right or wrong, either registers the host or closes the socket.
    pub(super) fn on_proof(
        &mut self,
        id: ConnId,
        text: &str,
        now_ms: u64,
        rng: &mut dyn RelayRng,
        out: &mut Vec<Action>,
    ) {
        let Some(conn) = self.open_conn(id) else {
            return;
        };
        let Phase::Proof {
            nonce,
            id: claimed,
            public,
            token_valid,
        } = conn.phase.clone()
        else {
            return;
        };
        let Some(m) = util::parse_object(text) else {
            self.hello_refused(id, "proof.malformed", out);
            return;
        };
        if util::text_of(&m, "type") != "register-proof" {
            self.hello_refused(id, "proof.malformed", out);
            return;
        }
        let Some(signature) = util::decode_b64url(util::text_of(&m, "signature"), 64) else {
            self.hello_refused(id, "proof.malformed", out);
            return;
        };
        let origin = self.config.public_origin.clone();
        let claimed_text = claimed.to_string();
        let proof_ok = verify_relay_proof(
            KeyRole::Host,
            &origin,
            &nonce,
            &claimed_text,
            &public,
            &signature,
        )
        .is_ok();
        if !proof_ok {
            self.hello_refused(id, "proof.bad-signature", out);
            return;
        }
        if !self.can_register(&conn.pc, &public, token_valid) {
            self.hello_refused(id, "proof.cannot-register", out);
            return;
        }
        self.register_host(id, &conn.pc, public, now_ms, rng, out);
    }

    /// The viewer's register-proof (M2 section 1). A valid proof admits the
    /// viewer through the PC's admission list, or through a pairing window when
    /// the hello carried `pair`. Any other answer closes 1008 with an empty reason.
    pub(super) fn on_viewer_proof(
        &mut self,
        id: ConnId,
        text: &str,
        now_ms: u64,
        rng: &mut dyn RelayRng,
        out: &mut Vec<Action>,
    ) {
        let Some(conn) = self.open_conn(id) else {
            return;
        };
        let Phase::ViewerProof {
            nonce,
            id: claimed,
            public,
            turn,
            pair,
        } = conn.phase.clone()
        else {
            return;
        };
        let Some(m) = util::parse_object(text) else {
            self.hello_refused(id, "proof.malformed", out);
            return;
        };
        if m.keys().any(|key| key != "type" && key != "signature")
            || util::text_of(&m, "type") != "register-proof"
        {
            self.hello_refused(id, "proof.malformed", out);
            return;
        }
        let Some(signature) = util::decode_b64url(util::text_of(&m, "signature"), 64) else {
            self.hello_refused(id, "proof.malformed", out);
            return;
        };
        let origin = self.config.public_origin.clone();
        let device = claimed.to_string();
        let proof_ok = verify_relay_proof(
            KeyRole::Viewer,
            &origin,
            &nonce,
            &device,
            &public,
            &signature,
        )
        .is_ok();
        if !proof_ok {
            self.hello_refused(id, "proof.bad-signature", out);
            return;
        }
        let admitted = self
            .pcs
            .records
            .get(&conn.pc)
            .is_some_and(|record| record.admitted.contains(&device));
        if admitted {
            self.viewer_admit(id, device, turn, false, now_ms, rng, out);
        } else if let Some(pair) = pair {
            self.pair_attempt(id, device, pair, turn, now_ms, rng, out);
        } else {
            self.hello_refused(id, "viewer.not-admitted", out);
        }
    }

    /// Refuses a hello, a proof or any pre-auth failure, closing 1008. `class` is
    /// the log class and picks the reason a viewer is told (see `viewer_reason`).
    /// Hosts and the version probe keep Go's "invalid hello".
    pub(super) fn hello_refused(&mut self, id: ConnId, class: &'static str, out: &mut Vec<Action>) {
        let reason = match (self.conns.get(&id).map(|c| c.role), class) {
            (Some(Role::Viewer), _) => viewer_reason(class),
            (_, "timeout.hello") => reasons::TIMED_OUT,
            _ => reasons::INVALID_HELLO,
        };
        self.close(id, 1008, reason, class, out);
    }

    /// Refuses a data-socket hello whose ticket or epoch does not check out.
    /// Media and file sockets are told "not authorized"; signalling sockets keep
    /// Go's "invalid hello".
    fn auth_refused(&mut self, id: ConnId, class: &'static str, out: &mut Vec<Action>) {
        let reason = match self.conns.get(&id).map(|c| c.role.kind()) {
            Some(Kind::Media | Kind::File) => reasons::NOT_AUTHORIZED,
            _ => reasons::INVALID_HELLO,
        };
        self.close(id, 1008, reason, class, out);
    }

    pub(super) fn token_matches(&self, supplied: &str) -> bool {
        util::ct_eq(supplied.as_bytes(), self.config.token.as_bytes())
    }

    /// Go's canRegisterPC: a known PC needs the same key; an unknown PC needs a
    /// valid TOKEN and room in the table. Refused while the store is unhealthy.
    pub(super) fn can_register(&self, pc: &str, public: &[u8; 32], token_valid: bool) -> bool {
        if !self.healthy {
            return false;
        }
        match self.pcs.records.get(pc) {
            Some(record) => record.public == *public,
            None => token_valid && self.pcs.records.len() < MAX_PCS,
        }
    }

    /// `/host-version-ws`: TOKEN-gated. Answers hello-ok, then closes 1000.
    fn version_probe(&mut self, id: ConnId, m: &Map<String, Value>, out: &mut Vec<Action>) {
        if !self.token_matches(util::text_of(m, "token")) {
            self.hello_refused(id, "hello.bad-token", out);
            return;
        }
        self.send_text(id, json!({"type": "hello-ok"}).to_string(), out);
        self.close(id, 1000, "version-probe-complete", "probe.complete", out);
    }

    /// The viewer's signalling hello (M2 section 1). It checks the signed
    /// identity and sends the register-challenge. Nothing else is sent, and the
    /// viewer is not admitted, until the register-proof passes.
    fn viewer_hello(
        &mut self,
        id: ConnId,
        m: &Map<String, Value>,
        now_ms: u64,
        rng: &mut dyn RelayRng,
        out: &mut Vec<Action>,
    ) {
        let Some(hello) = parse_viewer_hello(m) else {
            self.hello_refused(id, "hello.bad-identity", out);
            return;
        };
        let nonce: [u8; 32] = util::random(rng);
        let deadline = now_ms + self.config.timings.challenge_ms;
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.phase = Phase::ViewerProof {
                nonce,
                id: hello.id,
                public: hello.public,
                turn: hello.turn,
                pair: hello.pair,
            };
            conn.deadline_ms = deadline;
        }
        self.persist(id, out);
        self.send_text(
            id,
            json!({"type": "register-challenge", "nonce": util::b64url(&nonce)}).to_string(),
            out,
        );
    }

    /// Admits a viewer that has passed its proof and its admission check. It
    /// rotates the epoch, drops the old viewer's stale data sockets and sends
    /// hello-ok. A pairing-mode viewer gets `pairing: true` and no ticket. A
    /// host that is connected is told first, with `peer-joined` carrying the
    /// same epoch and the pairing flag.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn viewer_admit(
        &mut self,
        id: ConnId,
        device: String,
        turn: bool,
        pairing: bool,
        now_ms: u64,
        rng: &mut dyn RelayRng,
        out: &mut Vec<Action>,
    ) {
        let Some(pc) = self.conns.get(&id).map(|c| c.pc.clone()) else {
            return;
        };
        let key = key_for(Role::Viewer, "", &pc);
        if let Some(old) = self.peers.get(&key).copied().filter(|old| *old != id) {
            self.evict(old, 1008, "peer replaced", "peer.replaced", out);
        }
        let epoch: [u8; 16] = util::random(rng);
        let epoch = util::hex(&epoch);
        let room = self.rooms.entry(pc.clone()).or_default();
        room.pending.clear();
        room.current_epoch = Some(epoch.clone());

        let stale: Vec<ConnId> = self
            .peers
            .iter()
            .filter(|(k, _)| k.kind != Kind::Signal && k.pc == pc)
            .map(|(_, id)| *id)
            .collect();
        for old in stale {
            self.evict(old, 1001, "stale epoch", "epoch.rotated", out);
        }

        if let Some(conn) = self.conns.get_mut(&id) {
            conn.epoch = epoch.clone();
            conn.device = device;
            conn.pairing = pairing;
        }
        self.peers.insert(key, id);
        self.make_live(id, now_ms);
        let ticket = if pairing {
            None
        } else {
            Some(self.issue_ticket(id, now_ms, rng))
        };
        self.persist(id, out);
        // The host hears of the admission first, with the epoch the viewer is
        // about to hold. Its peer-joined is queued before the viewer's hello-ok.
        if let Some(host) = self.peers.get(&key_for(Role::Host, "", &pc)).copied() {
            self.send_text(
                host,
                json!({"type": "peer-joined", "mediaEpoch": epoch, "pairing": pairing}).to_string(),
                out,
            );
        }
        let ice = self.ice_servers(turn, now_ms, rng);
        let mut hello = json!({
            "type": "hello-ok",
            "pc": pc,
            "mediaEpoch": epoch,
            "iceServers": ice,
        });
        if let Some(ticket) = ticket {
            hello["ticket"] = json!(ticket);
        }
        if pairing {
            hello["pairing"] = json!(true);
        }
        self.send_text(id, hello.to_string(), out);
    }

    /// A media or file socket's hello: checks the ticket and the epoch, then
    /// announces presence to both sides (Go server.go:528-597).
    fn data_hello(
        &mut self,
        id: ConnId,
        m: &Map<String, Value>,
        now_ms: u64,
        out: &mut Vec<Action>,
    ) {
        let Some(conn) = self.open_conn(id) else {
            return;
        };
        let issuer_role = if conn.role.is_host_side() {
            Role::Host
        } else {
            Role::Viewer
        };
        let Some(issuer_epoch) =
            self.valid_ticket(issuer_role, &conn.pc, util::text_of(m, "ticket"), now_ms)
        else {
            self.auth_refused(id, "ticket.invalid", out);
            return;
        };
        // A viewer ticket is bound to the epoch its issuer holds now.
        if issuer_role == Role::Viewer && issuer_epoch != conn.epoch {
            self.auth_refused(id, "ticket.epoch-mismatch", out);
            return;
        }
        if self.current_epoch(&conn.pc) != Some(conn.epoch.as_str()) {
            self.auth_refused(id, "epoch.stale", out);
            return;
        }
        let key = key_for(conn.role, &conn.epoch, &conn.pc);
        if let Some(old) = self.peers.get(&key).copied().filter(|old| *old != id) {
            self.evict(old, 1008, "peer replaced", "peer.replaced", out);
        }
        self.peers.insert(key, id);
        self.make_live(id, now_ms);
        self.persist(id, out);

        let mut hello = json!({"type": "hello-ok"});
        if conn.role == Role::MediaViewer {
            hello["registeredAtMs"] = json!(now_ms);
        }
        self.send_text(id, hello.to_string(), out);

        let Some(other_role) = conn.role.other() else {
            return;
        };
        let event = Role::peer_event(conn.role.kind());
        let other = self
            .peers
            .get(&key_for(other_role, &conn.epoch, &conn.pc))
            .copied();
        self.send_text(
            id,
            json!({"type": event, "present": other.is_some()}).to_string(),
            out,
        );
        if let Some(other) = other {
            self.send_text(
                other,
                json!({"type": event, "present": true}).to_string(),
                out,
            );
        }
    }
}

/// The fields of a viewer hello that are checked before the proof.
struct ViewerHello {
    id: DeviceId,
    public: [u8; 32],
    turn: bool,
    pair: Option<[u8; 32]>,
}

/// Parses a viewer hello (M2 section 1). Unknown keys are refused. The `id`
/// must be canonical and must match `public`, which is 32 base64url bytes.
/// `turn` is a boolean when present. `pair` is 32 base64url bytes when present.
fn parse_viewer_hello(m: &Map<String, Value>) -> Option<ViewerHello> {
    const KEYS: [&str; 6] = ["type", "role", "id", "public", "turn", "pair"];
    if m.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return None;
    }
    let text = util::text_of(m, "id");
    let id = DeviceId::parse(text).ok()?;
    if id.to_string() != text {
        return None;
    }
    let public = key32(util::decode_b64url(util::text_of(m, "public"), 32)?)?;
    if DeviceId::from_public_key(&public) != id {
        return None;
    }
    let turn = match m.get("turn") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return None,
    };
    let pair = match m.get("pair") {
        None => None,
        Some(Value::String(text)) => Some(key32(util::decode_b64url(text, 32)?)?),
        Some(_) => return None,
    };
    Some(ViewerHello {
        id,
        public,
        turn,
        pair,
    })
}

fn key32(bytes: Vec<u8>) -> Option<[u8; 32]> {
    bytes.try_into().ok()
}

/// The reason a viewer is told for a refusal of this class. Classes that say
/// nothing the viewer could not already know get the generic "invalid hello".
fn viewer_reason(class: &str) -> &'static str {
    match class {
        "pairing.wrong-code" | "pairing.no-window" => reasons::PAIRING_CODE,
        "pairing.rate-limited" | "pairing.tracking-full" => reasons::RATE_LIMITED,
        "viewer.not-admitted" => reasons::NOT_PAIRED,
        "proof.bad-signature" => reasons::INVALID_PROOF,
        "timeout.hello" | "timeout.proof" => reasons::TIMED_OUT,
        _ => reasons::INVALID_HELLO,
    }
}
