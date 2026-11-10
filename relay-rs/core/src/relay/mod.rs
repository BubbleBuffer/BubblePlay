//! The sans-IO routing state machine: a port of relay/server.go and
//! relay/routing.go.
//!
//! The adapter owns sockets, queues, backpressure, the HTTP side (Origin, gate
//! cookie, Host header) and the clock. It calls `upgrade` for each accepted
//! WebSocket, `on_text` / `on_binary` for each message, `on_close` once a
//! socket is gone, and `tick` on a timer. Every call returns the `Action`s the
//! adapter must carry out. A `Close` action is always followed by `on_close`
//! for that socket, which is idempotent.

mod forward;
mod hello;
mod pairing;
mod pcs;
mod snapshot;
mod state;
mod types;
mod util;

use std::collections::BTreeMap;
use std::net::IpAddr;

use crate::RelayRng;
use crate::gate::{Gate, MAX_TRACKED_PREFIXES, Prefix};
use crate::limits::{HELLO_LIMIT, PREAUTH_READ_LIMIT};
use crate::protocol::validate_epoch;
use pairing::{PairFailures, PairWindow};
use state::{Conn, Kind, Phase, Role, Room, key_for, read_limit};

pub use pcs::{MAX_PCS, PcRecord, PcTable};
pub use types::{
    Action, AdminError, ConnId, ConnPhase, ConnSnapshot, CoreConfig, PcListing, Reject, Route,
    SnapshotPairWindow, SnapshotTicket, Timings, TurnConfig, UpgradeRequest,
};

/// The routing state for one relay.
pub struct Relay {
    config: CoreConfig,
    pcs: PcTable,
    healthy: bool,
    conns: BTreeMap<ConnId, Conn>,
    /// The current socket for each peer key (Go's s.peers).
    peers: BTreeMap<state::PeerKey, ConnId>,
    /// Per-PC state while a socket for the PC is open (Go's rooms).
    rooms: BTreeMap<String, Room>,
    /// The open pairing window for each PC, in memory only (M2 section 3).
    pair_windows: BTreeMap<String, PairWindow>,
    /// Failed pairing attempts per client prefix (M2 section 3).
    pair_failures: BTreeMap<Prefix, PairFailures>,
    /// Upgrade attempt windows, separate from authenticated connection capacity.
    upgrade_attempts: BTreeMap<Prefix, (u64, u32)>,
    /// Default token budget for standalone callers without a deployment gate.
    auth_failures: BTreeMap<Prefix, (u64, u32)>,
    bad_token_failure: Option<ConnId>,
    next_id: u64,
    /// The latest `now_ms` an entry point saw: the timestamp of log lines.
    clock_ms: u64,
    /// Client-log messages dropped since the last debug line about them.
    client_log_dropped: u64,
    /// When that debug line was last written.
    client_log_logged_ms: Option<u64>,
}

/// The close reasons clients can show. No secrets, no detail an outsider could use.
pub mod reasons {
    /// Malformed or unexpected hello, proof or packet before the session is live.
    pub const INVALID_HELLO: &str = "invalid hello";
    /// A live socket sent a packet the relay does not accept.
    pub const INVALID_PACKET: &str = "invalid packet";
    /// The viewer's identity proof did not verify.
    pub const INVALID_PROOF: &str = "invalid proof";
    /// The viewer is not on the PC's admission list and offered no pairing code.
    pub const NOT_PAIRED: &str = "not paired";
    /// The pairing code was wrong, or no pairing window is open.
    pub const PAIRING_CODE: &str = "pairing code not accepted";
    /// Too many failed pairing attempts, from this network or against this PC.
    pub const RATE_LIMITED: &str = "rate limited";
    /// A media or file socket's ticket or epoch was not accepted.
    pub const NOT_AUTHORIZED: &str = "not authorized";
    /// The socket did not finish its hello or proof in time.
    pub const TIMED_OUT: &str = "timed out";
    /// The host removed this viewer from the admission list.
    pub const ACCESS_REMOVED: &str = "access removed";
    /// The site gate did not approve this browser socket.
    pub const NOT_APPROVED: &str = "not approved";
    /// The socket could not be restored after the relay woke from hibernation.
    pub const SESSION_NOT_RESTORED: &str = "session not restored";
    /// The PC was removed by the admin.
    pub const PEER_REMOVED: &str = "peer removed";
}

impl Relay {
    /// A relay over the given PC table. The store is healthy.
    pub fn new(config: CoreConfig, pcs: PcTable) -> Self {
        // The configured secrets must never appear in a log line, whatever is logged.
        crate::log::register_secret(&config.token);
        if let Some(turn) = &config.turn {
            crate::log::register_secret(&turn.secret);
        }
        Self {
            config,
            pcs,
            healthy: true,
            conns: BTreeMap::new(),
            peers: BTreeMap::new(),
            rooms: BTreeMap::new(),
            pair_windows: BTreeMap::new(),
            pair_failures: BTreeMap::new(),
            upgrade_attempts: BTreeMap::new(),
            auth_failures: BTreeMap::new(),
            bad_token_failure: None,
            next_id: 1,
            clock_ms: 0,
            client_log_dropped: 0,
            client_log_logged_ms: None,
        }
    }

    /// Writes one relay log line (src "relay"). The PC id is cut to 8 characters.
    fn log(
        &self,
        level: crate::log::Level,
        msg: &str,
        pc: Option<&str>,
        fields: serde_json::Value,
    ) {
        crate::log::emit(level, self.clock_ms, "relay", msg, pc, fields);
    }

    /// The configuration this relay was built with.
    pub fn config(&self) -> &CoreConfig {
        &self.config
    }

    /// The current PC table.
    pub fn pcs(&self) -> &PcTable {
        &self.pcs
    }

    /// Records whether the PC store can be written. While false, registration
    /// is refused and the admin calls return `AdminError::Unavailable`. The
    /// adapter calls this with false when a `PersistPcs` fails.
    pub fn set_store_healthy(&mut self, healthy: bool) {
        self.healthy = healthy;
    }

    /// Accepts an upgrade. Origin and the gate cookie were checked by the
    /// adapter. `client` is the peer address; its prefix keys the pairing rate
    /// limits. Refusals are `Reject` (HTTP 404), including the connection cap.
    pub fn upgrade(
        &mut self,
        req: UpgradeRequest,
        client: IpAddr,
        now_ms: u64,
    ) -> Result<ConnId, Reject> {
        self.clock_ms = self.clock_ms.max(now_ms);
        let role = req.route.role();
        let kind = role.kind();
        let prefix = Prefix::of(client);
        if !self.allow_upgrade(prefix, now_ms) {
            return Err(self.upgrade_refused("upgrade.rate-limited", role, None));
        }
        if req.route.needs_viewer() && !req.viewer_authorized {
            return Err(self.upgrade_refused("upgrade.gate", role, None));
        }
        let pc = match req.pc.as_deref() {
            Some(raw) => match util::canonical_pc(raw) {
                Some(pc) => pc,
                None => return Err(self.upgrade_refused("upgrade.bad-pc", role, None)),
            },
            None if req.route == Route::HostVersionWs => String::new(),
            None => return Err(self.upgrade_refused("upgrade.no-pc", role, None)),
        };
        let epoch = if kind == Kind::Signal {
            String::new()
        } else {
            let Some(epoch) = req.epoch else {
                return Err(self.upgrade_refused("upgrade.no-epoch", role, Some(&pc)));
            };
            if !validate_epoch(&epoch) {
                return Err(self.upgrade_refused("upgrade.bad-epoch", role, Some(&pc)));
            }
            if self.current_epoch(&pc) != Some(epoch.as_str()) {
                return Err(self.upgrade_refused("upgrade.stale-epoch", role, Some(&pc)));
            }
            epoch
        };
        let unauthenticated = self
            .conns
            .values()
            .filter(|c| c.phase != Phase::Live && !c.closing);
        let (mut total, mut from_prefix) = (0, 0);
        for conn in unauthenticated {
            total += 1;
            from_prefix += usize::from(conn.prefix == prefix);
        }
        if total >= self.config.max_unauthenticated_connections
            || from_prefix >= self.config.max_unauthenticated_per_prefix
        {
            return Err(self.upgrade_refused("upgrade.unauthenticated-cap", role, Some(&pc)));
        }
        let id = ConnId(self.next_id);
        self.next_id += 1;
        self.conns.insert(
            id,
            Conn {
                route: req.route,
                role,
                pc,
                epoch,
                phase: Phase::Hello,
                deadline_ms: now_ms + self.config.timings.hello_ms,
                closing: false,
                ticket: None,
                prefix,
                device: String::new(),
                pairing: false,
                activity_ms: now_ms,
                saved_activity_ms: now_ms,
            },
        );
        Ok(id)
    }

    /// Counts attempts before accepting the WebSocket, including refused upgrades.
    fn allow_upgrade(&mut self, prefix: Prefix, now_ms: u64) -> bool {
        self.upgrade_attempts
            .retain(|_, (start, _)| now_ms.saturating_sub(*start) < 60_000);
        if !self.upgrade_attempts.contains_key(&prefix)
            && self.upgrade_attempts.len() >= MAX_TRACKED_PREFIXES
        {
            return false;
        }
        let (_, attempts) = self.upgrade_attempts.entry(prefix).or_insert((now_ms, 0));
        *attempts = attempts.saturating_add(1);
        *attempts <= self.config.upgrade_rate_per_prefix
    }

    /// The current transport read limit. Adapters apply this before reassembly.
    pub fn read_limit(&self, id: ConnId) -> Option<usize> {
        let conn = self.conns.get(&id).filter(|c| !c.closing)?;
        Some(if conn.phase == Phase::Live {
            read_limit(conn.role.kind())
        } else {
            PREAUTH_READ_LIMIT
        })
    }

    /// Processes text using the deployment's shared gate authentication budget.
    /// Adapters must use this entry point so HTTP and WebSocket token failures
    /// consume the same per-prefix allowance.
    pub fn on_text_with_gate(
        &mut self,
        id: ConnId,
        text: &str,
        now_ms: u64,
        rng: &mut dyn RelayRng,
        gate: &mut Gate,
    ) -> Vec<Action> {
        let Some(conn) = self.open_conn(id) else {
            return Vec::new();
        };
        let token_attempt = Self::token_attempt(&conn, text);
        if token_attempt && !gate.auth_prefix_allowed(conn.prefix, now_ms) {
            self.clock_ms = self.clock_ms.max(now_ms);
            let mut out = Vec::new();
            self.hello_refused(id, "hello.auth-rate-limited", &mut out);
            return out;
        }
        let out = self.on_text_unchecked(id, text, now_ms, rng);
        if token_attempt && self.bad_token_failure == Some(id) {
            gate.record_auth_prefix_failure(conn.prefix, now_ms);
        }
        out
    }

    fn token_attempt(conn: &Conn, text: &str) -> bool {
        conn.phase == Phase::Hello
            && (conn.role == Role::VersionProbe
                || (conn.role == Role::Host
                    && text.len() <= HELLO_LIMIT
                    && util::parse_object(text)
                        .is_some_and(|m| !util::text_of(&m, "token").is_empty())))
    }

    /// Logs an upgrade the relay answers with HTTP 404.
    fn upgrade_refused(&self, class: &'static str, role: Role, pc: Option<&str>) -> Reject {
        self.log(
            crate::log::Level::Warn,
            "upgrade refused",
            pc,
            serde_json::json!({"class": class, "role": role.name(), "status": 404}),
        );
        Reject
    }

    /// A text frame arrived on `id`. `rng` supplies the register challenge,
    /// tickets, the viewer epoch and TURN credential suffixes.
    /// Standalone callers get the default token-failure budget; deployments use
    /// [`Self::on_text_with_gate`] to share it with HTTP authentication.
    pub fn on_text(
        &mut self,
        id: ConnId,
        text: &str,
        now_ms: u64,
        rng: &mut dyn RelayRng,
    ) -> Vec<Action> {
        let Some(conn) = self.open_conn(id) else {
            return Vec::new();
        };
        let token_attempt = Self::token_attempt(&conn, text);
        self.auth_failures
            .retain(|_, (start, _)| now_ms.saturating_sub(*start) < 600_000);
        let blocked = self
            .auth_failures
            .get(&conn.prefix)
            .is_some_and(|(_, n)| *n >= 5)
            || (!self.auth_failures.contains_key(&conn.prefix)
                && self.auth_failures.len() >= MAX_TRACKED_PREFIXES);
        if token_attempt && blocked {
            self.clock_ms = self.clock_ms.max(now_ms);
            let mut out = Vec::new();
            self.hello_refused(id, "hello.auth-rate-limited", &mut out);
            return out;
        }
        let out = self.on_text_unchecked(id, text, now_ms, rng);
        if token_attempt && self.bad_token_failure == Some(id) {
            let (_, failures) = self.auth_failures.entry(conn.prefix).or_insert((now_ms, 0));
            *failures = failures.saturating_add(1);
        }
        out
    }

    fn on_text_unchecked(
        &mut self,
        id: ConnId,
        text: &str,
        now_ms: u64,
        rng: &mut dyn RelayRng,
    ) -> Vec<Action> {
        let mut out = Vec::new();
        self.bad_token_failure = None;
        self.clock_ms = self.clock_ms.max(now_ms);
        let Some(conn) = self.open_conn(id) else {
            return out;
        };
        let pre_auth = conn.phase != Phase::Live;
        if pre_auth && now_ms >= conn.deadline_ms {
            let class = if conn.phase == Phase::Hello {
                "timeout.hello"
            } else {
                "timeout.proof"
            };
            self.hello_refused(id, class, &mut out);
            return out;
        }
        if pre_auth && text.len() > HELLO_LIMIT {
            self.hello_refused(id, "hello.oversize", &mut out);
            return out;
        }
        if text.len() > read_limit(conn.role.kind()) {
            self.close(id, 1001, "peer disconnected", "limit.read", &mut out);
            return out;
        }
        match conn.phase {
            Phase::Hello => self.on_hello(id, text, now_ms, rng, &mut out),
            Phase::Proof { .. } => self.on_proof(id, text, now_ms, rng, &mut out),
            Phase::ViewerProof { .. } => self.on_viewer_proof(id, text, now_ms, rng, &mut out),
            Phase::Live => self.on_live_text(id, text, now_ms, &mut out),
        }
        out
    }

    /// A binary frame arrived on `id`.
    pub fn on_binary(&mut self, id: ConnId, bytes: &[u8], now_ms: u64) -> Vec<Action> {
        let mut out = Vec::new();
        self.clock_ms = self.clock_ms.max(now_ms);
        let Some(conn) = self.open_conn(id) else {
            return out;
        };
        if conn.phase != Phase::Live {
            // Go refuses any non-text message before the hello and before the proof.
            self.hello_refused(id, "hello.not-text", &mut out);
            return out;
        }
        if bytes.len() > read_limit(conn.role.kind()) {
            self.close(id, 1001, "peer disconnected", "limit.read", &mut out);
            return out;
        }
        self.on_live_binary(id, bytes, now_ms, &mut out);
        out
    }

    /// The socket for `id` is gone (the peer left, or the adapter closed it).
    /// Removes it and tells its peer. Idempotent.
    pub fn on_close(&mut self, id: ConnId) -> Vec<Action> {
        let mut out = Vec::new();
        self.forget(id, &mut out);
        out
    }

    /// Fires the timeouts due at `now_ms`. A hello not received in time, or a
    /// socket idle past the idle timeout, closes 1001 "peer disconnected". A
    /// host's register-proof not received in time closes 1008 "invalid hello";
    /// a viewer's register-proof closes 1008 with an empty reason. Expired
    /// pairing windows and tickets are dropped (with a snapshot for the host or
    /// the socket), and expired failure counts are dropped.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Action> {
        let mut out = Vec::new();
        self.clock_ms = self.clock_ms.max(now_ms);
        self.expire_pairing(now_ms, &mut out);
        self.expire_tickets(now_ms, &mut out);
        let due: Vec<(ConnId, &'static str)> = self
            .conns
            .iter()
            .filter(|(_, c)| !c.closing && now_ms >= c.deadline_ms)
            .map(|(id, c)| {
                let class = match c.phase {
                    Phase::Hello => "timeout.hello",
                    Phase::Proof { .. } | Phase::ViewerProof { .. } => "timeout.proof",
                    _ => "timeout.idle",
                };
                (*id, class)
            })
            .collect();
        for (id, class) in due {
            if class == "timeout.idle" {
                self.close(id, 1001, "peer disconnected", class, &mut out);
            } else {
                self.hello_refused(id, class, &mut out);
            }
        }
        out
    }

    /// The listing for the admin route: every remembered PC, sorted by id.
    pub fn list_pcs(&self) -> Result<Vec<PcListing>, AdminError> {
        if !self.healthy {
            return Err(AdminError::Unavailable);
        }
        Ok(self
            .pcs
            .records
            .iter()
            .map(|(id, record)| PcListing {
                id: id.clone(),
                public: util::b64url(&record.public),
                first_seen: record.first_seen,
                last_seen: record.last_seen,
                online: self.host_online(id),
                admitted: record.admitted.len(),
            })
            .collect())
    }

    /// Forgets a PC. Persists the table first (the first action), then closes
    /// its host sockets 1008 "peer removed" and sends peer-left to its viewer.
    /// An id that is not a device id, or a valid id with no registered PC, is
    /// `AdminError::NotFound` and nothing is persisted.
    pub fn delete_pc(&mut self, id: &str) -> Result<Vec<Action>, AdminError> {
        if !self.healthy {
            return Err(AdminError::Unavailable);
        }
        let pc = util::canonical_pc(id).ok_or(AdminError::NotFound)?;
        if self.pcs.records.remove(&pc).is_none() {
            return Err(AdminError::NotFound);
        }
        let mut out = vec![Action::PersistPcs(self.pcs.clone())];
        let hosts: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.pc == pc && c.route.is_host_socket())
            .map(|(id, _)| *id)
            .collect();
        for host in hosts {
            self.close(
                host,
                1008,
                reasons::PEER_REMOVED,
                "admin.pc-removed",
                &mut out,
            );
            self.forget(host, &mut out);
        }
        Ok(out)
    }

    // -- shared bookkeeping, used by the hello and forward paths --------------

    /// The socket if it exists and is not closing.
    fn open_conn(&self, id: ConnId) -> Option<Conn> {
        self.conns.get(&id).filter(|c| !c.closing).cloned()
    }

    /// True while the socket exists and has not been closed.
    fn is_open(&self, id: ConnId) -> bool {
        self.conns.get(&id).is_some_and(|c| !c.closing)
    }

    /// True while a host socket for `pc` (a canonical device id) is open: the
    /// state behind `online` in [`Relay::list_pcs`]. Read-only, for
    /// `GET /presence`.
    pub fn pc_online(&self, pc: &str) -> bool {
        self.host_online(pc)
    }

    /// The host signalling socket for a PC is open.
    fn host_online(&self, pc: &str) -> bool {
        self.peers
            .get(&key_for(Role::Host, "", pc))
            .is_some_and(|id| self.is_open(*id))
    }

    /// The room's current epoch, if the room exists and has one.
    fn current_epoch(&self, pc: &str) -> Option<&str> {
        self.rooms.get(pc)?.current_epoch.as_deref()
    }

    /// Moves a socket to the live phase, records its activity and refreshes its
    /// idle deadline. The caller emits the snapshot.
    fn make_live(&mut self, id: ConnId, now_ms: u64) {
        let idle = self.config.timings.idle_ms;
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.phase = Phase::Live;
            conn.deadline_ms = now_ms + idle;
            conn.activity_ms = now_ms;
        }
    }

    /// Reserve authenticated capacity only after proof/ticket validation. A
    /// verified replacement may reuse its peer's slot, before evicting that peer.
    fn admit_capacity(&mut self, id: ConnId, out: &mut Vec<Action>) -> bool {
        let Some(conn) = self.open_conn(id) else {
            return false;
        };
        let key = key_for(conn.role, &conn.epoch, &conn.pc);
        let replacing = self.peers.get(&key).is_some_and(|old| self.is_open(*old));
        let live = self
            .conns
            .values()
            .filter(|c| c.phase == Phase::Live && !c.closing)
            .count();
        if !replacing && live >= self.config.max_connections {
            self.hello_refused(id, "hello.connection-cap", out);
            return false;
        }
        true
    }

    /// Marks a socket closing and queues the Close action. Idempotent.
    /// `class` names why, for the log line: a short dotted word such as
    /// `pairing.wrong-code`. Every close is logged with its status, reason and class.
    fn close(
        &mut self,
        id: ConnId,
        code: u16,
        reason: &str,
        class: &'static str,
        out: &mut Vec<Action>,
    ) {
        let seen = self.conns.get_mut(&id).filter(|c| !c.closing).map(|conn| {
            conn.closing = true;
            // A viewer that has not been admitted yet is known by the id it claimed.
            let device = match &conn.phase {
                Phase::ViewerProof { id, .. } if conn.device.is_empty() => id.to_string(),
                _ => conn.device.clone(),
            };
            (conn.role, device, conn.pc.clone())
        });
        if let Some((role, device, pc)) = seen {
            out.push(Action::Close(id, code, reason.to_owned()));
            let level = if code == 1008 {
                crate::log::Level::Warn
            } else {
                crate::log::Level::Info
            };
            let msg = if code == 1008 {
                "socket refused"
            } else {
                "socket closed"
            };
            let fields = serde_json::json!({
                "class": class,
                "status": code,
                "reason": reason,
                "role": role.name(),
                "peer": (!device.is_empty()).then(|| crate::log::short_id(&device)),
            });
            self.log(level, msg, (!pc.is_empty()).then_some(pc.as_str()), fields);
        }
    }

    /// Closes a socket and drops it from the peer table without telling its
    /// peer. Go does this when a socket is replaced or cleared as stale.
    fn evict(
        &mut self,
        id: ConnId,
        code: u16,
        reason: &str,
        class: &'static str,
        out: &mut Vec<Action>,
    ) {
        self.close(id, code, reason, class, out);
        if let Some(conn) = self.conns.remove(&id) {
            let key = key_for(conn.role, &conn.epoch, &conn.pc);
            if self.peers.get(&key) == Some(&id) {
                self.peers.remove(&key);
            }
        }
    }

    /// Queues a text frame unless the socket is gone.
    fn send_text(&self, id: ConnId, text: String, out: &mut Vec<Action>) {
        if self.is_open(id) {
            out.push(Action::SendText(id, text));
        }
    }

    /// Queues a binary frame unless the socket is gone.
    fn send_binary(&self, id: ConnId, bytes: &[u8], out: &mut Vec<Action>) {
        if self.is_open(id) {
            out.push(Action::SendBinary(id, bytes.to_vec()));
        }
    }

    /// Go's removePeerLocked: forgets the socket and tells its peer. Safe to
    /// call for a socket that was already forgotten.
    fn forget(&mut self, id: ConnId, out: &mut Vec<Action>) {
        let Some(conn) = self.conns.remove(&id) else {
            return;
        };
        let kind = conn.role.kind();
        let key = key_for(conn.role, &conn.epoch, &conn.pc);
        if self.peers.get(&key) == Some(&id) {
            self.peers.remove(&key);
            if conn.role == Role::Viewer {
                self.forget_viewer_state(&conn, out);
            }
            if conn.role == Role::Host {
                // The host's signalling socket closing removes its pairing window.
                self.pair_windows.remove(&conn.pc);
            }
            self.notify_peer_left(&conn, kind, out);
        }
        if !conn.pc.is_empty() && !self.conns.values().any(|c| c.pc == conn.pc) {
            self.rooms.remove(&conn.pc);
        }
    }

    /// A viewer left: its pending buffer goes, its epoch is cleared if current,
    /// and its data sockets for that epoch are closed.
    fn forget_viewer_state(&mut self, conn: &Conn, out: &mut Vec<Action>) {
        if let Some(room) = self.rooms.get_mut(&conn.pc) {
            room.pending.clear();
            if room.current_epoch.as_deref() == Some(conn.epoch.as_str()) {
                room.current_epoch = None;
            }
        }
        let departed: Vec<ConnId> = self
            .peers
            .iter()
            .filter(|(k, _)| k.kind != Kind::Signal && k.pc == conn.pc && k.epoch == conn.epoch)
            .map(|(_, id)| *id)
            .collect();
        for id in departed {
            self.close(id, 1001, "viewer departed", "viewer.left", out);
        }
    }

    /// Tells the other side of a departed socket: peer-left for signalling,
    /// media-peer present=false for media, and file-peer-left (a close) for files.
    fn notify_peer_left(&mut self, conn: &Conn, kind: Kind, out: &mut Vec<Action>) {
        let Some(other_role) = conn.role.other() else {
            return;
        };
        let other_key = key_for(other_role, &conn.epoch, &conn.pc);
        let Some(other) = self.peers.get(&other_key).copied() else {
            return;
        };
        match kind {
            Kind::Signal => self.send_text(
                other,
                serde_json::json!({"type": "peer-left"}).to_string(),
                out,
            ),
            Kind::Media => self.send_text(
                other,
                serde_json::json!({"type": Role::peer_event(kind), "present": false}).to_string(),
                out,
            ),
            Kind::File => {
                self.peers.remove(&other_key);
                self.close(other, 1001, "file-peer-left", "file.peer-left", out);
            }
        }
    }
}
