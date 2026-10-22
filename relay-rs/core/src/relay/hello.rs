//! The first message on every socket: hello, host registration, tickets and
//! data-socket attachment (Go's Relay.hello, routing.go, server.go:473-612).

use bp_device_key::{DeviceId, Role as KeyRole, verify_relay_proof};
use hmac::{Hmac, Mac};
use serde_json::{Map, Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use super::Relay;
use super::pcs::{MAX_PCS, PcRecord};
use super::state::{Conn, Kind, Phase, Role, Ticket, key_for};
use super::types::{Action, ConnId};
use super::util;
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
            self.hello_refused(id, out);
            return;
        };
        let role_text = util::text_of(&m, "role");
        let known_role = conn.role.hello_roles().iter().any(|r| *r == role_text);
        if util::text_of(&m, "type") != "hello" || !known_role {
            self.hello_refused(id, out);
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
            self.hello_refused(id, out);
            return;
        };
        if util::text_of(&m, "type") != "register-proof" {
            self.hello_refused(id, out);
            return;
        }
        let Some(signature) = util::decode_b64url(util::text_of(&m, "signature"), 64) else {
            self.hello_refused(id, out);
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
        if !proof_ok || !self.can_register(&conn.pc, &public, token_valid) {
            self.hello_refused(id, out);
            return;
        }
        self.register_host(id, &conn.pc, public, now_ms, rng, out);
    }

    /// Refuses a hello for a signalling socket, or any structural failure.
    pub(super) fn hello_refused(&mut self, id: ConnId, out: &mut Vec<Action>) {
        self.close(id, 1008, "invalid hello", out);
    }

    /// Refuses a data-socket hello whose ticket or epoch does not check out.
    /// Those are auth failures, so the reason is empty (D8). Signalling sockets
    /// keep Go's "invalid hello".
    fn auth_refused(&mut self, id: ConnId, out: &mut Vec<Action>) {
        let reason = match self.conns.get(&id).map(|c| c.role.kind()) {
            Some(Kind::Media | Kind::File) => "",
            _ => "invalid hello",
        };
        self.close(id, 1008, reason, out);
    }

    fn token_matches(&self, supplied: &str) -> bool {
        util::ct_eq(supplied.as_bytes(), self.config.token.as_bytes())
    }

    /// Go's canRegisterPC: a known PC needs the same key; an unknown PC needs a
    /// valid TOKEN and room in the table. Refused while the store is unhealthy.
    fn can_register(&self, pc: &str, public: &[u8; 32], token_valid: bool) -> bool {
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
            self.hello_refused(id, out);
            return;
        }
        self.send_text(id, json!({"type": "hello-ok"}).to_string(), out);
        self.close(id, 1000, "version-probe-complete", out);
    }

    /// The host's signalling hello: checks the identity, then sends the
    /// register-challenge (Go server.go:475-504).
    fn host_hello(
        &mut self,
        id: ConnId,
        m: &Map<String, Value>,
        now_ms: u64,
        rng: &mut dyn RelayRng,
        out: &mut Vec<Action>,
    ) {
        let Some(pc) = self.conns.get(&id).map(|c| c.pc.clone()) else {
            return;
        };
        let Ok(claimed) = DeviceId::parse(util::text_of(m, "id")) else {
            self.hello_refused(id, out);
            return;
        };
        let public = match util::decode_b64url(util::text_of(m, "public"), 32) {
            Some(bytes) => {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                key
            }
            None => {
                self.hello_refused(id, out);
                return;
            }
        };
        if claimed.to_string() != pc || DeviceId::from_public_key(&public) != claimed {
            self.hello_refused(id, out);
            return;
        }
        // A supplied token must match TOKEN, even for a known PC.
        let token = util::text_of(m, "token");
        let token_valid = !token.is_empty() && self.token_matches(token);
        if !token.is_empty() && !token_valid {
            self.hello_refused(id, out);
            return;
        }
        if !self.can_register(&pc, &public, token_valid) {
            self.hello_refused(id, out);
            return;
        }
        let nonce: [u8; 32] = util::random(rng);
        let deadline = now_ms + self.config.timings.challenge_ms;
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.phase = Phase::Proof {
                nonce,
                id: claimed,
                public,
                token_valid,
            };
            conn.deadline_ms = deadline;
        }
        self.send_text(
            id,
            json!({"type": "register-challenge", "nonce": util::b64url(&nonce)}).to_string(),
            out,
        );
    }

    /// Registration after a valid proof: persists the record first, then makes
    /// the host the current signalling peer and sends hello-ok with a ticket.
    fn register_host(
        &mut self,
        id: ConnId,
        pc: &str,
        public: [u8; 32],
        now_ms: u64,
        rng: &mut dyn RelayRng,
        out: &mut Vec<Action>,
    ) {
        let first_seen = self.pcs.records.get(pc).map_or(now_ms, |r| r.first_seen);
        self.pcs.records.insert(
            pc.to_owned(),
            PcRecord {
                public,
                first_seen,
                last_seen: now_ms,
            },
        );
        // The adapter must make this durable before it sends the hello-ok below.
        // If the write fails it calls set_store_healthy(false) and closes.
        out.push(Action::PersistPcs(self.pcs.clone()));

        let key = key_for(Role::Host, "", pc);
        if let Some(old) = self.peers.get(&key).copied().filter(|old| *old != id) {
            self.evict(old, 1008, "peer replaced", out);
            if let Some(viewer) = self.peers.get(&key_for(Role::Viewer, "", pc)).copied() {
                self.send_text(viewer, json!({"type": "peer-left"}).to_string(), out);
            }
        }
        self.peers.insert(key, id);
        let room = self.rooms.entry(pc.to_owned()).or_default();
        room.cached.clear();
        let pending = std::mem::take(&mut room.pending);
        self.make_live(id, now_ms);
        let ticket = self.issue_ticket(id, now_ms, rng);
        self.send_text(
            id,
            json!({"type": "hello-ok", "ticket": ticket, "pc": pc}).to_string(),
            out,
        );
        for buffered in pending {
            self.send_text(id, buffered.raw, out);
        }
    }

    /// The viewer's signalling hello: rotates the epoch, drops the old viewer's
    /// stale data sockets, replays the cached host capabilities (Go
    /// server.go:553-600).
    fn viewer_hello(
        &mut self,
        id: ConnId,
        m: &Map<String, Value>,
        now_ms: u64,
        rng: &mut dyn RelayRng,
        out: &mut Vec<Action>,
    ) {
        let Some(pc) = self.conns.get(&id).map(|c| c.pc.clone()) else {
            return;
        };
        let turn = util::bool_of(m, "turn");
        let key = key_for(Role::Viewer, "", &pc);
        if let Some(old) = self.peers.get(&key).copied().filter(|old| *old != id) {
            self.evict(old, 1008, "peer replaced", out);
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
            self.evict(old, 1001, "stale epoch", out);
        }

        if let Some(conn) = self.conns.get_mut(&id) {
            conn.epoch = epoch.clone();
        }
        self.peers.insert(key, id);
        self.make_live(id, now_ms);
        let ticket = self.issue_ticket(id, now_ms, rng);
        let ice = self.ice_servers(turn, now_ms, rng);
        self.send_text(
            id,
            json!({
                "type": "hello-ok",
                "ticket": ticket,
                "pc": pc,
                "mediaEpoch": epoch,
                "iceServers": ice,
            })
            .to_string(),
            out,
        );
        let cached: Vec<String> = self
            .rooms
            .get(&pc)
            .map(|r| r.cached.values().cloned().collect())
            .unwrap_or_default();
        for raw in cached {
            self.send_text(id, raw, out);
        }
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
            self.auth_refused(id, out);
            return;
        };
        // A viewer ticket is bound to the epoch its issuer holds now.
        if issuer_role == Role::Viewer && issuer_epoch != conn.epoch {
            self.auth_refused(id, out);
            return;
        }
        if self.current_epoch(&conn.pc) != Some(conn.epoch.as_str()) {
            self.auth_refused(id, out);
            return;
        }
        let key = key_for(conn.role, &conn.epoch, &conn.pc);
        if let Some(old) = self.peers.get(&key).copied().filter(|old| *old != id) {
            self.evict(old, 1008, "peer replaced", out);
        }
        self.peers.insert(key, id);
        self.make_live(id, now_ms);

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

    /// Checks a ticket against the signalling peer of `issuer_role` for `pc`.
    /// Returns that peer's epoch when the ticket is good. A ticket is good while
    /// its issuer is open and before it expires (12 h).
    fn valid_ticket(&self, issuer_role: Role, pc: &str, text: &str, now_ms: u64) -> Option<String> {
        util::decode_b64url(text, 32)?;
        let issuer = self.peers.get(&key_for(issuer_role, "", pc)).copied()?;
        let conn: Conn = self.open_conn(issuer)?;
        let Ticket { hash, expires_ms } = conn.ticket.clone()?;
        if now_ms >= expires_ms || !util::ct_eq(&sha256(text.as_bytes()), &hash) {
            return None;
        }
        Some(conn.epoch)
    }

    /// Mints a ticket for a signalling socket: 32 random bytes, base64url. Only
    /// the SHA-256 of the text is kept (Go hashes the text, server.go:572).
    fn issue_ticket(&mut self, id: ConnId, now_ms: u64, rng: &mut dyn RelayRng) -> String {
        let raw: [u8; 32] = util::random(rng);
        let text = util::b64url(&raw);
        let hash = sha256(text.as_bytes());
        let ttl = self.config.timings.ticket_ttl_ms;
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.ticket = Some(Ticket {
                hash,
                expires_ms: now_ms + ttl,
            });
        }
        text
    }

    /// The `iceServers` list: the STUN server always, and TURN credentials when
    /// the viewer asked for them and TURN is configured (Go server.go:766-794).
    fn ice_servers(&self, turn: bool, now_ms: u64, rng: &mut dyn RelayRng) -> Value {
        let mut servers = vec![json!({"urls": "stun:stun.l.google.com:19302"})];
        let Some(cfg) = self.config.turn.as_ref() else {
            return Value::Array(servers);
        };
        if !turn || cfg.secret.is_empty() {
            return Value::Array(servers);
        }
        let Some(authority) = util::turn_authority(&cfg.host) else {
            return Value::Array(servers);
        };
        // Go's randomCredential(12): 12 random bytes, unpadded base64url.
        let suffix: [u8; 12] = util::random(rng);
        let expiry = now_ms / 1000 + cfg.ttl_s;
        let username = format!("{expiry}:{}", util::b64url(&suffix));
        let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(cfg.secret.as_bytes()) else {
            return Value::Array(servers);
        };
        mac.update(username.as_bytes());
        let credential = util::b64_std(&mac.finalize().into_bytes());
        servers.push(json!({
            "urls": [
                format!("turn:{authority}?transport=udp"),
                format!("turn:{authority}?transport=tcp"),
            ],
            "username": username,
            "credential": credential,
        }));
        Value::Array(servers)
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    out
}
