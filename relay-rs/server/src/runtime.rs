//! The runtime state shared by every request and socket: the sans-IO relay and gate, the outbound
//! queues, persistence and ntfy. Locks are std Mutexes and are never held across an await.
//!
//! Actions from the core are applied in order by `Shared::apply`. A `PersistPcs` is written
//! before the actions after it run. Writes take the current table under a tokio mutex, so the
//! last write always holds the newest state.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bp_relay_core::RelayRng;
use bp_relay_core::gate::{
    COOKIE_NAME, Gate, GateConfig, GateEffect, GateRecord, GateSnapshot, NtfyConfig,
    Response as GateResponse,
};
use bp_relay_core::limits::{QUEUE_BYTE_LIMIT, QUEUE_FRAMES};
use bp_relay_core::presence;
use bp_relay_core::relay::{Action, ConnId, CoreConfig, PcRecord, PcTable, Relay, TurnConfig};
use ipnet::IpNet;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, mpsc, watch};
use tokio::task::JoinHandle;

use crate::store::{self, PcsFile};
use crate::{config::Config, limits::Limits, server::ServeError};

/// Ntfy publishes give up after this long.
const NTFY_TIMEOUT: Duration = Duration::from_secs(10);
/// A single frame that does not reach the peer within this time ends the socket.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// The close frame gets this long to go out.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// The gate cookie is stored hashed. This is the value the cookie carries, hashed.
pub fn cookie_hash(value: &str) -> [u8; 32] {
    let digest = Sha256::digest(value.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Milliseconds since the Unix epoch. The adapter is the only place that reads the clock.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Randomness from the OS.
pub struct OsRandom;

impl RelayRng for OsRandom {
    fn fill(&mut self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("the OS random source is unavailable");
    }
}

/// One message queued for a socket.
pub enum Out {
    Text(String),
    Binary(Vec<u8>),
}

impl Out {
    pub fn len(&self) -> usize {
        match self {
            Out::Text(text) => text.len(),
            Out::Binary(bytes) => bytes.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `PersistPcs` could not be written. The batch was dropped and the store marked unhealthy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreFailed;

/// A close the writer must send: code and reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Close {
    pub code: u16,
    pub reason: String,
}

/// The adapter side of one upgraded socket, kept in the registry while it is open.
pub struct Conn {
    out: mpsc::Sender<Out>,
    bytes: Arc<AtomicUsize>,
    stop: watch::Sender<Option<Close>>,
}

/// The receiving halves handed to the socket's tasks.
pub struct Wiring {
    pub out: mpsc::Receiver<Out>,
    pub bytes: Arc<AtomicUsize>,
    pub stop: watch::Receiver<Option<Close>>,
}

enum Enqueue {
    Sent,
    Missing,
    Overflow,
}

/// Everything shared between the HTTP handlers, the socket tasks and the ticker.
pub struct Shared {
    pub admin_token: String,
    pub assets_dir: PathBuf,
    /// The host of the public origin, for the Host check on `/healthz`.
    pub public_host: String,
    /// The canonical public origin. A browser socket that names it without a valid cookie is
    /// accepted and closed 1008 (see `app::refused_socket`).
    pub public_origin: String,
    /// The listen address's host literal, for the Host check on `/healthz` (the Docker probe).
    pub listen_host: String,
    /// Proxies whose `X-Forwarded-For` is believed.
    pub trusted_proxies: Vec<IpNet>,
    relay: Mutex<Relay>,
    gate: Mutex<Gate>,
    /// Per-client counters for `GET /presence` and held navigations.
    pub limits: Limits,
    conns: Mutex<HashMap<ConnId, Conn>>,
    pcs_write: AsyncMutex<()>,
    gate_write: AsyncMutex<()>,
    state_dir: PathBuf,
    pcs_healthy: AtomicBool,
    http: reqwest::Client,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// Loads the state files and builds the relay and the gate. A corrupt state file is an error.
    pub fn new(cfg: &Config) -> Result<Shared, ServeError> {
        let table = load_pcs_table(&cfg.state_dir)?;
        let snapshot = match store::load_gate(&cfg.state_dir)? {
            Some(value) => gate_from_json(&value)?,
            None => GateSnapshot::default(),
        };
        let mut core = CoreConfig::new(cfg.public_origin.clone(), cfg.token.expose());
        core.max_connections = cfg.max_connections as usize;
        core.turn = cfg.turn.as_ref().map(|turn| TurnConfig {
            host: turn.host.clone(),
            secret: turn.secret.expose().to_string(),
            ttl_s: u64::from(turn.ttl_s),
        });
        let gate_config = GateConfig {
            public_origin: cfg.public_origin.clone(),
            approval_ttl_secs: u64::from(cfg.approval_ttl_s),
            ntfy: cfg.ntfy.as_ref().map(|ntfy| NtfyConfig {
                url: ntfy.url.clone(),
                topic: ntfy.topic.clone(),
                token: ntfy.token.as_ref().map(|t| t.expose().to_string()),
            }),
        };
        let mut gate = Gate::new(gate_config, snapshot, now_ms())
            .map_err(|e| format!("gate configuration: {e:?}"))?;
        gate.set_hold_ms(cfg.gate_hold_ms);
        let http = reqwest::Client::builder()
            .timeout(NTFY_TIMEOUT)
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let public_host = url::Url::parse(&cfg.public_origin)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_default();
        let listen_host = match cfg.listen.ip() {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
        };
        Ok(Shared {
            admin_token: cfg.admin_token.expose().to_string(),
            assets_dir: cfg.assets_dir.clone(),
            public_origin: cfg.public_origin.clone(),
            public_host,
            listen_host,
            trusted_proxies: cfg.trusted_proxy_cidrs.clone(),
            relay: Mutex::new(Relay::new(core, table)),
            gate: Mutex::new(gate),
            limits: Limits::default(),
            conns: Mutex::new(HashMap::new()),
            pcs_write: AsyncMutex::new(()),
            gate_write: AsyncMutex::new(()),
            state_dir: cfg.state_dir.clone(),
            pcs_healthy: AtomicBool::new(true),
            http,
        })
    }

    /// The relay, locked for one synchronous call.
    pub fn relay(&self) -> MutexGuard<'_, Relay> {
        lock(&self.relay)
    }

    /// The gate, locked for one synchronous call.
    pub fn gate(&self) -> MutexGuard<'_, Gate> {
        lock(&self.gate)
    }

    /// Answers `GET /presence` for `client`: the limiter, then a read of host presence. Nothing
    /// else takes the limiter's lock, so holding it while the relay is read cannot deadlock.
    pub fn presence(&self, client: IpAddr, ids: Option<&str>, now: u64) -> GateResponse {
        let mut limiter = self.limits.presence();
        let relay = lock(&self.relay);
        presence::answer(&mut limiter, &relay, client, ids, now)
    }

    /// True when a valid, unexpired gate cookie is among the request's cookies.
    pub fn cookie_valid(&self, cookie: Option<&str>, now: u64) -> bool {
        let prefix = format!("{COOKIE_NAME}=");
        let hashes: Vec<[u8; 32]> = cookie
            .unwrap_or("")
            .split(';')
            .filter_map(|part| part.trim().strip_prefix(prefix.as_str()))
            .map(cookie_hash)
            .collect();
        if hashes.is_empty() {
            return false;
        }
        let snapshot = self.gate().snapshot();
        snapshot
            .records
            .iter()
            .any(|record| record.expires_ms > now && hashes.contains(&record.hash))
    }

    /// Registers the outbound side of a new socket.
    pub fn register(&self, id: ConnId) -> Wiring {
        let (out_tx, out_rx) = mpsc::channel(QUEUE_FRAMES);
        let (stop_tx, stop_rx) = watch::channel(None);
        let bytes = Arc::new(AtomicUsize::new(0));
        let conn = Conn {
            out: out_tx,
            bytes: bytes.clone(),
            stop: stop_tx,
        };
        lock(&self.conns).insert(id, conn);
        Wiring {
            out: out_rx,
            bytes,
            stop: stop_rx,
        }
    }

    /// Applies a batch of actions in order. Returns `Err` when a `PersistPcs` failed: the rest of
    /// the batch is dropped, the store is marked unhealthy and `origin` (if any) is closed 1011.
    pub async fn apply(
        &self,
        origin: Option<ConnId>,
        actions: Vec<Action>,
    ) -> Result<(), StoreFailed> {
        let mut queue: VecDeque<Action> = actions.into();
        while let Some(action) = queue.pop_front() {
            match action {
                Action::PersistPcs(_) => {
                    if !self.persist_pcs().await {
                        self.relay().set_store_healthy(false);
                        self.pcs_healthy.store(false, Ordering::SeqCst);
                        if let Some(id) = origin {
                            self.close_conn(id, 1011, String::new());
                            let nested = self.forget(id);
                            self.run_sync(nested);
                        }
                        return Err(StoreFailed);
                    }
                }
                Action::Snapshot(_, _) => {}
                Action::SendText(id, text) => self.deliver(id, Out::Text(text), &mut queue),
                Action::SendBinary(id, bytes) => self.deliver(id, Out::Binary(bytes), &mut queue),
                Action::Close(id, code, reason) => {
                    self.close_conn(id, code, reason);
                    let nested = self.forget(id);
                    push_front(&mut queue, nested);
                }
            }
        }
        Ok(())
    }

    /// Runs actions that cannot persist (the close paths only). A PersistPcs here is logged and
    /// skipped; the next persist writes the whole table anyway.
    fn run_sync(&self, actions: Vec<Action>) {
        let mut queue: VecDeque<Action> = actions.into();
        while let Some(action) = queue.pop_front() {
            match action {
                Action::PersistPcs(_) => {
                    tracing::warn!("skipped a PersistPcs on a close path");
                }
                Action::Snapshot(_, _) => {}
                Action::SendText(id, text) => self.deliver(id, Out::Text(text), &mut queue),
                Action::SendBinary(id, bytes) => self.deliver(id, Out::Binary(bytes), &mut queue),
                Action::Close(id, code, reason) => {
                    self.close_conn(id, code, reason);
                    let nested = self.forget(id);
                    push_front(&mut queue, nested);
                }
            }
        }
    }

    /// Queues a frame for a socket. A socket whose queue overflows is closed 1013 "slow peer".
    fn deliver(&self, id: ConnId, out: Out, queue: &mut VecDeque<Action>) {
        match self.enqueue(id, out) {
            Enqueue::Sent | Enqueue::Missing => {}
            Enqueue::Overflow => {
                self.close_conn(id, 1013, "slow peer".to_string());
                let nested = self.forget(id);
                push_front(queue, nested);
            }
        }
    }

    fn enqueue(&self, id: ConnId, out: Out) -> Enqueue {
        let conns = lock(&self.conns);
        let Some(conn) = conns.get(&id) else {
            return Enqueue::Missing;
        };
        let len = out.len();
        let before = conn.bytes.fetch_add(len, Ordering::SeqCst);
        if before + len > QUEUE_BYTE_LIMIT {
            conn.bytes.fetch_sub(len, Ordering::SeqCst);
            return Enqueue::Overflow;
        }
        match conn.out.try_send(out) {
            Ok(()) => Enqueue::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => {
                conn.bytes.fetch_sub(len, Ordering::SeqCst);
                Enqueue::Overflow
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                conn.bytes.fetch_sub(len, Ordering::SeqCst);
                Enqueue::Missing
            }
        }
    }

    /// Asks the socket's writer to close it with this code and reason.
    fn close_conn(&self, id: ConnId, code: u16, reason: String) {
        if let Some(conn) = lock(&self.conns).get(&id) {
            conn.stop.send_replace(Some(Close { code, reason }));
        }
    }

    /// Drops the socket from the registry and tells the relay it is gone. Idempotent.
    pub fn forget(&self, id: ConnId) -> Vec<Action> {
        lock(&self.conns).remove(&id);
        self.relay().on_close(id)
    }

    /// Called when a socket's reader ends.
    pub async fn close_socket(&self, id: ConnId) {
        let actions = self.forget(id);
        let _ = self.apply(None, actions).await;
    }

    /// Writes the whole PC table. The table is read under the write lock, so the last write holds
    /// the newest state. Returns false when the write failed.
    async fn persist_pcs(&self) -> bool {
        let _write = self.pcs_write.lock().await;
        let table = self.relay().pcs().clone();
        let file = pcs_file(&table);
        let dir = self.state_dir.clone();
        match tokio::task::spawn_blocking(move || store::save_pcs(&dir, &file)).await {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                tracing::error!(%error, "could not save the PC table");
                false
            }
            Err(error) => {
                tracing::error!(%error, "the PC table write task failed");
                false
            }
        }
    }

    async fn persist_gate(&self) {
        let _write = self.gate_write.lock().await;
        let value = gate_to_json(&self.gate().snapshot());
        let dir = self.state_dir.clone();
        match tokio::task::spawn_blocking(move || store::save_gate(&dir, &value)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::error!(%error, "could not save the gate state"),
            Err(error) => tracing::error!(%error, "the gate state write task failed"),
        }
    }

    /// Carries out the effects a gate call returned.
    pub async fn run_gate_effects(&self, effects: Vec<GateEffect>) {
        for effect in effects {
            match effect {
                GateEffect::NtfyPublish { url, headers, body } => {
                    let http = self.http.clone();
                    tokio::spawn(async move {
                        let mut request = http.post(url).body(body);
                        for (name, value) in &headers {
                            request = request.header(name.as_str(), value.as_str());
                        }
                        match request.send().await {
                            Ok(response) if response.status().is_success() => {}
                            Ok(response) => {
                                tracing::warn!(status = %response.status(), "ntfy publish was refused");
                            }
                            Err(error) => {
                                // The error text can carry the URL, which is not secret, but never the body or headers.
                                tracing::warn!(error = %error.without_url(), "ntfy publish failed");
                            }
                        }
                    });
                }
                GateEffect::PersistGate(_) => self.persist_gate().await,
            }
        }
    }

    /// One timer pass: relay and gate timeouts, and a retry of a failed PC write.
    pub async fn tick(&self) {
        let now = now_ms();
        let actions = self.relay().tick(now);
        let _ = self.apply(None, actions).await;
        let effects = self.gate().tick(now);
        self.run_gate_effects(effects).await;
        if !self.pcs_healthy.load(Ordering::SeqCst) && self.persist_pcs().await {
            self.relay().set_store_healthy(true);
            self.pcs_healthy.store(true, Ordering::SeqCst);
        }
    }
}

/// Runs `tick` once a second until the handle is aborted.
pub fn spawn_ticker(shared: Arc<Shared>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            shared.tick().await;
        }
    })
}

fn push_front(queue: &mut VecDeque<Action>, actions: Vec<Action>) {
    for action in actions.into_iter().rev() {
        queue.push_front(action);
    }
}

/// The PC table from `pcs.json`. `registered_ms` is the first registration; `last_seen` is not
/// persisted and starts equal to it.
pub fn load_pcs_table(state_dir: &Path) -> Result<PcTable, ServeError> {
    let file = store::load_pcs(state_dir)?;
    let mut table = PcTable::new();
    for record in file.pcs {
        let public = store::decode_public_key(&record.public)
            .ok_or_else(|| format!("pc {}: bad public key", record.id))?;
        table.records.insert(
            record.id,
            PcRecord {
                public,
                first_seen: record.registered_ms,
                last_seen: record.registered_ms,
                admitted: record.admitted.into_iter().collect(),
                remote_pairing: record.remote_pairing,
            },
        );
    }
    Ok(table)
}

fn pcs_file(table: &PcTable) -> PcsFile {
    PcsFile {
        version: store::PCS_VERSION,
        pcs: table
            .records
            .iter()
            .map(|(id, record)| store::PcRecord {
                id: id.clone(),
                public: URL_SAFE_NO_PAD.encode(record.public),
                registered_ms: record.first_seen,
                admitted: record.admitted.iter().cloned().collect(),
                remote_pairing: record.remote_pairing,
            })
            .collect(),
    }
}

/// `gate.json`: `{"version":1,"cookies":[{"hash":"<base64url>","expires_ms":N}]}`.
pub fn gate_to_json(snapshot: &GateSnapshot) -> Value {
    let cookies: Vec<Value> = snapshot
        .records
        .iter()
        .map(|record| {
            json!({
                "hash": URL_SAFE_NO_PAD.encode(record.hash),
                "expires_ms": record.expires_ms,
            })
        })
        .collect();
    json!({ "version": 1, "cookies": cookies })
}

pub fn gate_from_json(value: &Value) -> Result<GateSnapshot, String> {
    if value.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("gate.json: unsupported version".to_string());
    }
    let cookies = value
        .get("cookies")
        .and_then(Value::as_array)
        .ok_or_else(|| "gate.json: cookies is not a list".to_string())?;
    let mut records = Vec::with_capacity(cookies.len());
    for cookie in cookies {
        let hash_text = cookie
            .get("hash")
            .and_then(Value::as_str)
            .ok_or_else(|| "gate.json: a cookie has no hash".to_string())?;
        let bytes = URL_SAFE_NO_PAD
            .decode(hash_text)
            .map_err(|_| "gate.json: a cookie hash is not base64url".to_string())?;
        let hash: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "gate.json: a cookie hash is not 32 bytes".to_string())?;
        let expires_ms = cookie
            .get("expires_ms")
            .and_then(Value::as_u64)
            .ok_or_else(|| "gate.json: a cookie has no expiry".to_string())?;
        records.push(GateRecord { hash, expires_ms });
    }
    Ok(GateSnapshot { records })
}
