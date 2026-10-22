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
mod state;
mod types;
mod util;

use std::collections::BTreeMap;
use std::net::IpAddr;

use crate::RelayRng;
use crate::gate::Prefix;
use crate::limits::HELLO_LIMIT;
use crate::protocol::validate_epoch;
use pairing::{PairFailures, PairWindow};
use state::{Conn, Kind, Phase, Role, Room, key_for, read_limit};

pub use pcs::{MAX_PCS, PcRecord, PcTable};
pub use types::{
    Action, AdminError, ConnId, CoreConfig, PcListing, Reject, Route, Timings, TurnConfig,
    UpgradeRequest,
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
    next_id: u64,
}

impl Relay {
    /// A relay over the given PC table. The store is healthy.
    pub fn new(config: CoreConfig, pcs: PcTable) -> Self {
        Self {
            config,
            pcs,
            healthy: true,
            conns: BTreeMap::new(),
            peers: BTreeMap::new(),
            rooms: BTreeMap::new(),
            pair_windows: BTreeMap::new(),
            pair_failures: BTreeMap::new(),
            next_id: 1,
        }
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
        let role = req.route.role();
        let kind = role.kind();
        if req.route.needs_viewer() && !req.viewer_authorized {
            return Err(Reject);
        }
        let pc = match req.pc.as_deref() {
            Some(raw) => util::canonical_pc(raw).ok_or(Reject)?,
            None if req.route == Route::HostVersionWs => String::new(),
            None => return Err(Reject),
        };
        let epoch = if kind == Kind::Signal {
            String::new()
        } else {
            let epoch = req.epoch.ok_or(Reject)?;
            if !validate_epoch(&epoch) || self.current_epoch(&pc) != Some(epoch.as_str()) {
                return Err(Reject);
            }
            epoch
        };
        if self.conns.len() >= self.config.max_connections {
            return Err(Reject);
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
                prefix: Prefix::of(client),
                device: String::new(),
                pairing: false,
            },
        );
        Ok(id)
    }

    /// A text frame arrived on `id`. `rng` supplies the register challenge,
    /// tickets, the viewer epoch and TURN credential suffixes.
    pub fn on_text(
        &mut self,
        id: ConnId,
        text: &str,
        now_ms: u64,
        rng: &mut dyn RelayRng,
    ) -> Vec<Action> {
        let mut out = Vec::new();
        let Some(conn) = self.open_conn(id) else {
            return out;
        };
        let pre_auth = conn.phase != Phase::Live;
        if pre_auth && text.len() > HELLO_LIMIT {
            self.hello_refused(id, &mut out);
            return out;
        }
        if text.len() > read_limit(conn.role.kind()) {
            self.close(id, 1001, "peer disconnected", &mut out);
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
        let Some(conn) = self.open_conn(id) else {
            return out;
        };
        if conn.phase != Phase::Live {
            // Go refuses any non-text message before the hello and before the proof.
            self.hello_refused(id, &mut out);
            return out;
        }
        if bytes.len() > read_limit(conn.role.kind()) {
            self.close(id, 1001, "peer disconnected", &mut out);
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
    /// pairing windows and failure counts are dropped.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Action> {
        let mut out = Vec::new();
        self.expire_pairing(now_ms);
        let due: Vec<(ConnId, Option<&'static str>)> = self
            .conns
            .iter()
            .filter(|(_, c)| !c.closing && now_ms >= c.deadline_ms)
            .map(|(id, c)| {
                let challenge = match c.phase {
                    Phase::Proof { .. } => Some("invalid hello"),
                    Phase::ViewerProof { .. } => Some(""),
                    _ => None,
                };
                (*id, challenge)
            })
            .collect();
        for (id, challenge) in due {
            match challenge {
                Some(reason) => self.close(id, 1008, reason, &mut out),
                None => self.close(id, 1001, "peer disconnected", &mut out),
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
            self.close(host, 1008, "peer removed", &mut out);
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

    /// Moves a socket to the live phase and refreshes its idle deadline.
    fn make_live(&mut self, id: ConnId, now_ms: u64) {
        let idle = self.config.timings.idle_ms;
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.phase = Phase::Live;
            conn.deadline_ms = now_ms + idle;
        }
    }

    /// Refreshes the idle deadline after a processed message.
    fn touch(&mut self, id: ConnId, now_ms: u64) {
        let idle = self.config.timings.idle_ms;
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.deadline_ms = now_ms + idle;
        }
    }

    /// Marks a socket closing and queues the Close action. Idempotent.
    fn close(&mut self, id: ConnId, code: u16, reason: &str, out: &mut Vec<Action>) {
        if let Some(conn) = self.conns.get_mut(&id).filter(|c| !c.closing) {
            conn.closing = true;
            out.push(Action::Close(id, code, reason.to_owned()));
        }
    }

    /// Closes a socket and drops it from the peer table without telling its
    /// peer. Go does this when a socket is replaced or cleared as stale.
    fn evict(&mut self, id: ConnId, code: u16, reason: &str, out: &mut Vec<Action>) {
        self.close(id, code, reason, out);
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
            if let (Role::Host, Some(room)) = (conn.role, self.rooms.get_mut(&conn.pc)) {
                room.cached.clear();
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
            self.close(id, 1001, "viewer departed", out);
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
                self.close(other, 1001, "file-peer-left", out);
            }
        }
    }
}
