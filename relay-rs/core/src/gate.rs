//! The ntfy site gate, as a sans-IO state machine.
//!
//! The gate sits in front of every browser-facing HTTP path and every WebSocket
//! upgrade that carries an `Origin`. Every gated HTTP path needs a valid
//! `__Host-gate` cookie. Without one, only a top-level navigation (a GET with
//! `Sec-Fetch-Dest: document`, or with no `Sec-Fetch-Dest`) may open an
//! approval. It gets no page: a pending approval is published to ntfy with
//! Approve and Deny actions and the adapter holds the request open for up to
//! [`GATE_HOLD_MS`] ([`Verdict::Hold`]), asking [`Gate::check_hold`] whether it
//! may go on. An approval inside the hold answers the held request with a 302
//! to the same URL and the gate cookie. An approval that lands after the hold
//! ended is claimed by the next navigation from the same client prefix, once.
//! Every other outcome (timeout, deny, cooldown, capacity, no approval path)
//! is [`Verdict::Drop`]: the adapter ends the connection with no content. Any
//! other gated request without a cookie gets a plain 403 and opens nothing;
//! HEAD is dropped and opens nothing.
//!
//! Nothing here touches sockets, clocks or randomness. The adapter passes
//! `now_ms` with every call, supplies randomness through [`RelayRng`], sends
//! the [`GateEffect::NtfyPublish`] requests, and stores the [`GateSnapshot`]
//! that each [`GateEffect::PersistGate`] carries. Pending approvals, cooldowns
//! and rate counters live in memory only.
//!
//! Without an `ntfy` section in the config there is no approval path. Gated
//! navigations are then dropped.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv6Addr};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::rng::RelayRng;

/// Name of the gate cookie. The `__Host-` prefix forces Secure and Path=/.
pub const COOKIE_NAME: &str = "__Host-gate";
const COOKIE_PREFIX: &str = "__Host-gate=";
/// Lifetime of the cookie and of its server record, fixed at approval.
pub const COOKIE_MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;
const COOKIE_LIFETIME_MS: u64 = COOKIE_MAX_AGE_SECS * 1000;
/// Approval TTL bounds. `Gate::new` refuses values outside them.
pub const MIN_APPROVAL_TTL_SECS: u64 = 30;
pub const MAX_APPROVAL_TTL_SECS: u64 = 120;
pub const DEFAULT_APPROVAL_TTL_SECS: u64 = 90;
/// How long the adapter holds an unapproved navigation open before dropping it.
pub const GATE_HOLD_MS: u64 = 30_000;
/// Pending approvals in total, across all prefixes.
pub const MAX_PENDING: usize = 10;
/// Cooldown per prefix after a pending approval ends.
pub const COOLDOWN_MS: u64 = 15_000;
/// Decision requests per prefix per window. Excess requests get 204 and are
/// not processed.
pub const DECISION_LIMIT: u32 = 60;
pub const DECISION_WINDOW_MS: u64 = 60_000;
/// Upper bound on each per-prefix map (decision rates, cooldowns) and on the
/// pending list. A new prefix arriving at a full map is answered 429 after
/// expired entries are pruned.
pub const MAX_TRACKED_PREFIXES: usize = 4096;
const DECISION_PATH: &str = "/__gate/decision";
const FORGET_PATH: &str = "/__gate/forget";
const GATE_PREFIX: &str = "/__gate/";
const MANAGE_PREFIX: &str = "/__gate/manage/";

/// The ntfy topic the approval messages go to.
#[derive(Clone, PartialEq, Eq)]
pub struct NtfyConfig {
    /// The publish URL, used exactly as configured.
    pub url: String,
    pub topic: String,
    /// Sent as `Authorization: Bearer <token>` when present.
    pub token: Option<String>,
}

impl fmt::Debug for NtfyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NtfyConfig")
            .field("url", &self.url)
            .field("topic", &self.topic)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateConfig {
    /// Exact origin of the relay, for example `https://relay.example`: scheme,
    /// host and an optional non-default port, all lowercase, no path, query or
    /// fragment. Browser requests must send exactly this `Origin`, and the
    /// decision links point at it.
    pub public_origin: String,
    pub approval_ttl_secs: u64,
    pub ntfy: Option<NtfyConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    EmptyPublicOrigin,
    /// `public_origin` is not a canonical bare origin.
    NonCanonicalPublicOrigin,
    ApprovalTtlOutOfRange(u64),
}

/// One persisted cookie record. Only the SHA-256 of the cookie value is kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GateRecord {
    pub hash: [u8; 32],
    pub expires_ms: u64,
}

/// Everything that must survive a restart: the cookie records only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GateSnapshot {
    pub records: Vec<GateRecord>,
}

/// Work the adapter must do after a call returns.
#[derive(Clone, PartialEq, Eq)]
pub enum GateEffect {
    /// POST `body` to `url` with `headers`. The body is JSON.
    NtfyPublish {
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// Store this snapshot in place of the previous one.
    PersistGate(GateSnapshot),
}

/// The ntfy body carries the approve and deny bearers, and the headers carry
/// the ntfy token, so neither is printed.
impl fmt::Debug for GateEffect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateEffect::NtfyPublish { url, headers, body } => {
                let headers: Vec<(&str, &str)> = headers
                    .iter()
                    .map(|(name, value)| {
                        let value = if name.eq_ignore_ascii_case("authorization") {
                            "<redacted>"
                        } else {
                            value.as_str()
                        };
                        (name.as_str(), value)
                    })
                    .collect();
                f.debug_struct("NtfyPublish")
                    .field("url", url)
                    .field("headers", &headers)
                    .field("body", &format_args!("<redacted {} bytes>", body.len()))
                    .finish()
            }
            GateEffect::PersistGate(snapshot) => {
                f.debug_tuple("PersistGate").field(snapshot).finish()
            }
        }
    }
}

/// A response the adapter writes as is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// One HTTP or WebSocket-upgrade request, as the adapter sees it.
#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// The raw query string without the `?`, if any. Only used to send an
    /// admitted navigation back to the URL it asked for.
    pub query: Option<&'a str>,
    /// True for a WebSocket upgrade.
    pub websocket: bool,
    pub origin: Option<&'a str>,
    /// The raw `Cookie` header.
    pub cookie: Option<&'a str>,
    /// The raw `Authorization` header.
    pub authorization: Option<&'a str>,
    /// The raw `Sec-Fetch-Dest` header. Only `document` (or no header at all)
    /// on a GET is a top-level navigation that may open an approval.
    pub sec_fetch_dest: Option<&'a str>,
    pub body: &'a [u8],
    /// The client address, already resolved by the adapter.
    pub client_ip: IpAddr,
    pub now_ms: u64,
}

/// Cookies and bearers are redacted, and the body is shown by length only.
impl fmt::Debug for Request<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("query", &self.query)
            .field("websocket", &self.websocket)
            .field("origin", &self.origin)
            .field("cookie", &self.cookie.map(|_| "<redacted>"))
            .field("authorization", &self.authorization.map(|_| "<redacted>"))
            .field("sec_fetch_dest", &self.sec_fetch_dest)
            .field("body", &format_args!("<{} bytes>", self.body.len()))
            .field("client_ip", &self.client_ip)
            .field("now_ms", &self.now_ms)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Pass the request on to the relay.
    Allow,
    /// Answer 404 for HTTP. Refuse a WebSocket upgrade (the adapter closes
    /// with 1008 and an empty reason, or returns 404).
    Refuse,
    /// Run the effects (the ntfy publish), then hold the request open and
    /// call [`Gate::check_hold`] until it admits or drops it. Nothing is
    /// written to the client while it is held.
    Hold(Hold, Vec<GateEffect>),
    /// End the connection with no content: no status page, no body.
    Drop,
    /// Answer this response and run the effects. A gate endpoint, a plain 403
    /// for a gated request that cannot open an approval, or the 302 with the
    /// gate cookie for a navigation that claims an approval.
    Handled(Response, Vec<GateEffect>),
}

/// A navigation held open while its approval is pending. The adapter keeps it
/// and passes it to [`Gate::check_hold`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hold {
    pending: String,
    location: String,
    until_ms: u64,
}

impl Hold {
    /// When the hold ends. [`Gate::check_hold`] drops the request at this time
    /// unless the approval landed.
    pub fn until_ms(&self) -> u64 {
        self.until_ms
    }
}

/// What [`Gate::check_hold`] decided for a held navigation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldCheck {
    /// Still waiting. Check again a little later.
    Waiting,
    /// Approved. Answer this 302 (same URL, gate cookie) and run the effects.
    Admit(Response, Vec<GateEffect>),
    /// Denied, timed out, or claimed by another request. End the connection
    /// with no content.
    Drop,
}

/// Outcome of counting one decision request against its prefix.
enum Counted {
    Ok,
    /// Over the per-prefix limit: 204, not processed.
    Limited,
    /// A new prefix at a full map: 429.
    Full,
}

/// The client prefix an approval is keyed by: IPv4 /24 or IPv6 /56. An
/// IPv4-mapped IPv6 address counts as IPv4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Prefix {
    V4([u8; 3]),
    V6([u8; 7]),
}

impl Prefix {
    pub fn of(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => {
                let o = v4.octets();
                Prefix::V4([o[0], o[1], o[2]])
            }
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => Prefix::of(IpAddr::V4(v4)),
                None => {
                    let o = v6.octets();
                    Prefix::V6([o[0], o[1], o[2], o[3], o[4], o[5], o[6]])
                }
            },
        }
    }
}

impl fmt::Display for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Prefix::V4(p) => write!(f, "{}.{}.{}.0/24", p[0], p[1], p[2]),
            Prefix::V6(p) => {
                let mut full = [0u8; 16];
                full[..7].copy_from_slice(p);
                write!(f, "{}/56", Ipv6Addr::from(full))
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Waiting,
    /// Approved and not yet claimed. Claimable until `decided_ms + ttl_ms`.
    Approved,
}

struct Pending {
    id: String,
    prefix: Prefix,
    approve_hash: Option<[u8; 32]>,
    deny_hash: Option<[u8; 32]>,
    expires_ms: u64,
    decided_ms: u64,
    state: State,
}

/// The gate. Create one per relay with [`Gate::new`] and call it for every
/// browser-facing request.
pub struct Gate {
    config: GateConfig,
    ttl_ms: u64,
    hold_ms: u64,
    cookies: BTreeMap<[u8; 32], u64>,
    pending: Vec<Pending>,
    cooldowns: BTreeMap<Prefix, u64>,
    decision_rates: BTreeMap<Prefix, (u64, u32)>,
}

impl Gate {
    /// Build the gate from its config and the last persisted snapshot. Records
    /// already expired at `now_ms` are dropped.
    pub fn new(
        config: GateConfig,
        persisted: GateSnapshot,
        now_ms: u64,
    ) -> Result<Self, ConfigError> {
        if !(MIN_APPROVAL_TTL_SECS..=MAX_APPROVAL_TTL_SECS).contains(&config.approval_ttl_secs) {
            return Err(ConfigError::ApprovalTtlOutOfRange(config.approval_ttl_secs));
        }
        if config.public_origin.is_empty() {
            return Err(ConfigError::EmptyPublicOrigin);
        }
        // One trailing slash is canonicalised away; anything else must already be
        // a canonical bare origin.
        let stripped = config
            .public_origin
            .strip_suffix('/')
            .unwrap_or(&config.public_origin);
        if stripped.is_empty() {
            return Err(ConfigError::EmptyPublicOrigin);
        }
        if !is_canonical_origin(stripped) {
            return Err(ConfigError::NonCanonicalPublicOrigin);
        }
        let public_origin = stripped.to_string();
        let config = GateConfig {
            public_origin,
            ..config
        };
        let ttl_ms = config.approval_ttl_secs.saturating_mul(1000);
        let cookies: BTreeMap<[u8; 32], u64> = persisted
            .records
            .into_iter()
            .filter(|record| record.expires_ms > now_ms)
            .map(|record| (record.hash, record.expires_ms))
            .collect();
        Ok(Gate {
            config,
            ttl_ms,
            hold_ms: GATE_HOLD_MS,
            cookies,
            pending: Vec::new(),
            cooldowns: BTreeMap::new(),
            decision_rates: BTreeMap::new(),
        })
    }

    /// Shorten (or restore) the hold, for tests. The value is clamped to
    /// `1..=GATE_HOLD_MS`, so the hold can never be made longer.
    pub fn set_hold_ms(&mut self, hold_ms: u64) {
        self.hold_ms = hold_ms.clamp(1, GATE_HOLD_MS);
    }

    /// The cookie records to persist. Pending approvals are not included.
    pub fn snapshot(&self) -> GateSnapshot {
        GateSnapshot {
            records: self
                .cookies
                .iter()
                .map(|(hash, expires_ms)| GateRecord {
                    hash: *hash,
                    expires_ms: *expires_ms,
                })
                .collect(),
        }
    }

    /// Drop expired state. Returns a `PersistGate` effect if any cookie record
    /// was removed.
    pub fn tick(&mut self, now_ms: u64) -> Vec<GateEffect> {
        self.sweep(now_ms);
        let before = self.cookies.len();
        self.cookies.retain(|_, expires_ms| *expires_ms > now_ms);
        if self.cookies.len() == before {
            Vec::new()
        } else {
            vec![GateEffect::PersistGate(self.snapshot())]
        }
    }

    /// The earliest time at which `tick` has work: an approval's expiry, the end
    /// of an unclaimed approval's claim window, a cookie's expiry, a cooldown's
    /// end or a decision window's end. None when nothing is tracked. `tick` at this
    /// time (and not before) does what the sweep would.
    pub fn next_deadline(&self) -> Option<u64> {
        let pending = self.pending.iter().map(|pending| match pending.state {
            State::Waiting => pending.expires_ms,
            State::Approved => pending.decided_ms.saturating_add(self.ttl_ms),
        });
        let cookies = self.cookies.values().copied();
        let cooldowns = self.cooldowns.values().copied();
        let rates = self
            .decision_rates
            .values()
            .map(|(start, _)| start.saturating_add(DECISION_WINDOW_MS));
        pending.chain(cookies).chain(cooldowns).chain(rates).min()
    }

    /// The ids of the approvals still waiting for a decision.
    pub fn pending_ids(&mut self, now_ms: u64) -> Vec<String> {
        self.sweep(now_ms);
        self.pending
            .iter()
            .filter(|pending| pending.state == State::Waiting)
            .map(|pending| pending.id.clone())
            .collect()
    }

    /// Classify one request and act on it.
    pub fn handle(&mut self, rng: &mut dyn RelayRng, req: &Request<'_>) -> Verdict {
        self.sweep(req.now_ms);
        let origin_ok = req
            .origin
            .is_none_or(|origin| origin == self.config.public_origin);
        if !origin_ok {
            return Verdict::Refuse;
        }
        let cookie_ok = self.has_valid_cookie(req.cookie, req.now_ms);
        let gate_path = req.path.starts_with(GATE_PREFIX);
        if req.websocket {
            if gate_path {
                return Verdict::Refuse;
            }
            return if req.origin.is_none() || cookie_ok {
                Verdict::Allow
            } else {
                Verdict::Refuse
            };
        }
        if req.path.starts_with(MANAGE_PREFIX) {
            return Verdict::Allow;
        }
        if gate_path {
            return self.gate_endpoint(req, cookie_ok);
        }
        if cookie_ok {
            return Verdict::Allow;
        }
        // Without a cookie, only a top-level navigation may open an approval.
        // HEAD is dropped and never creates a pending. Anything else is a
        // plain 403.
        if req.method == "HEAD" {
            return Verdict::Drop;
        }
        if req.method == "GET" && is_navigation(req.sec_fetch_dest) {
            self.navigate(rng, req)
        } else {
            Verdict::Handled(forbidden(), Vec::new())
        }
    }

    /// Decide a held navigation at `now_ms`: admit it if its approval landed,
    /// drop it if the approval is gone or the hold ended, else keep waiting.
    /// An approval is claimed once: the first held request or navigation to
    /// see it gets the cookie, and the entry is removed.
    pub fn check_hold(&mut self, rng: &mut dyn RelayRng, hold: &Hold, now_ms: u64) -> HoldCheck {
        self.sweep(now_ms);
        let Some(index) = self
            .pending
            .iter()
            .position(|pending| pending.id == hold.pending)
        else {
            return HoldCheck::Drop;
        };
        match self.pending[index].state {
            State::Approved => {
                self.pending.remove(index);
                let (response, effects) = self.admit(rng, &hold.location, now_ms);
                HoldCheck::Admit(response, effects)
            }
            State::Waiting if now_ms >= hold.until_ms => HoldCheck::Drop,
            State::Waiting => HoldCheck::Waiting,
        }
    }

    /// Mint a cookie and answer a 302 back to `location` that sets it.
    fn admit(
        &mut self,
        rng: &mut dyn RelayRng,
        location: &str,
        now_ms: u64,
    ) -> (Response, Vec<GateEffect>) {
        let value = random_token(rng);
        self.cookies
            .insert(sha256(&value), now_ms.saturating_add(COOKIE_LIFETIME_MS));
        let mut headers = common_headers();
        headers.push(("Location".into(), location.to_string()));
        headers.push(("Set-Cookie".into(), set_cookie(&value)));
        let response = Response {
            status: 302,
            headers,
            body: Vec::new(),
        };
        (response, vec![GateEffect::PersistGate(self.snapshot())])
    }

    fn has_valid_cookie(&self, cookie: Option<&str>, now_ms: u64) -> bool {
        cookie_hashes(cookie).any(|hash| {
            self.cookies
                .get(&hash)
                .is_some_and(|expires_ms| *expires_ms > now_ms)
        })
    }

    fn gate_endpoint(&mut self, req: &Request<'_>, cookie_ok: bool) -> Verdict {
        match req.path {
            DECISION_PATH if req.method == "POST" => self.decide(req),
            FORGET_PATH if req.method == "POST" && cookie_ok && req.origin.is_some() => {
                self.forget(req)
            }
            _ => Verdict::Refuse,
        }
    }

    fn decide(&mut self, req: &Request<'_>) -> Verdict {
        let prefix = Prefix::of(req.client_ip);
        match self.count_decision(prefix, req.now_ms) {
            Counted::Ok => {}
            Counted::Limited => return Verdict::Handled(no_content(), Vec::new()),
            Counted::Full => return Verdict::Handled(too_many_requests(), Vec::new()),
        }
        let bearer = req
            .authorization
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|token| {
                URL_SAFE_NO_PAD
                    .decode(*token)
                    .is_ok_and(|raw| raw.len() == 32)
            });
        let action = std::str::from_utf8(req.body)
            .ok()
            .map(|text| text.trim().to_ascii_lowercase());
        if let (Some(token), Some(action)) = (bearer, action.as_deref()) {
            let hash = sha256(token);
            let now = req.now_ms;
            let found = self.pending.iter().position(|pending| {
                pending.state == State::Waiting
                    && pending.expires_ms > now
                    && match action {
                        "approve" => pending.approve_hash == Some(hash),
                        "deny" => pending.deny_hash == Some(hash),
                        _ => false,
                    }
            });
            if let Some(index) = found {
                // A denied entry is removed at once, so its held requests drop.
                // An approved one waits to be claimed.
                let ended_prefix = self.pending[index].prefix;
                if action == "approve" {
                    let pending = &mut self.pending[index];
                    pending.state = State::Approved;
                    pending.decided_ms = now;
                    pending.approve_hash = None;
                    pending.deny_hash = None;
                } else {
                    self.pending.remove(index);
                }
                self.set_cooldown(ended_prefix, now);
            }
        }
        Verdict::Handled(no_content(), Vec::new())
    }

    fn forget(&mut self, req: &Request<'_>) -> Verdict {
        let mut removed = false;
        for hash in cookie_hashes(req.cookie) {
            removed |= self.cookies.remove(&hash).is_some();
        }
        let mut headers = common_headers();
        headers.push((
            "Set-Cookie".into(),
            format!("{COOKIE_NAME}=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Lax"),
        ));
        let effects = if removed {
            vec![GateEffect::PersistGate(self.snapshot())]
        } else {
            Vec::new()
        };
        Verdict::Handled(
            Response {
                status: 204,
                headers,
                body: Vec::new(),
            },
            effects,
        )
    }

    /// A top-level navigation without a cookie. It claims an approval its
    /// prefix already has, joins its prefix's waiting approval, or opens and
    /// publishes a new one. Anything else is dropped.
    fn navigate(&mut self, rng: &mut dyn RelayRng, req: &Request<'_>) -> Verdict {
        let Some(ntfy) = self.config.ntfy.clone() else {
            return Verdict::Drop;
        };
        let prefix = Prefix::of(req.client_ip);
        let location = self.location(req);
        // An approval that landed after its hold ended. `sweep` already
        // removed any past its claim window.
        if let Some(index) = self
            .pending
            .iter()
            .position(|pending| pending.state == State::Approved && pending.prefix == prefix)
        {
            self.pending.remove(index);
            let (response, effects) = self.admit(rng, &location, req.now_ms);
            return Verdict::Handled(response, effects);
        }
        let until_ms = req.now_ms.saturating_add(self.hold_ms);
        if let Some(pending) = self
            .pending
            .iter()
            .find(|pending| pending.state == State::Waiting && pending.prefix == prefix)
        {
            let hold = Hold {
                pending: pending.id.clone(),
                location,
                until_ms,
            };
            return Verdict::Hold(hold, Vec::new());
        }
        let waiting = self
            .pending
            .iter()
            .filter(|pending| pending.state == State::Waiting)
            .count();
        let cooling = self
            .cooldowns
            .get(&prefix)
            .is_some_and(|until| *until > req.now_ms);
        // A new prefix needs room in the cooldown map and the pending list.
        // Expired cooldowns and claim windows were pruned by `sweep`.
        let cooldowns_full =
            !self.cooldowns.contains_key(&prefix) && self.cooldowns.len() >= MAX_TRACKED_PREFIXES;
        let pending_full = self.pending.len() >= MAX_TRACKED_PREFIXES;
        if waiting >= MAX_PENDING || cooling || cooldowns_full || pending_full {
            return Verdict::Drop;
        }
        let id = random_string(rng, 16);
        let approve = random_token(rng);
        let deny = random_token(rng);
        self.pending.push(Pending {
            id: id.clone(),
            prefix,
            approve_hash: Some(sha256(&approve)),
            deny_hash: Some(sha256(&deny)),
            expires_ms: req.now_ms.saturating_add(self.ttl_ms),
            decided_ms: req.now_ms,
            state: State::Waiting,
        });
        let effect = ntfy_publish(&ntfy, &self.config.public_origin, prefix, &approve, &deny);
        let hold = Hold {
            pending: id,
            location,
            until_ms,
        };
        Verdict::Hold(hold, vec![effect])
    }

    /// The absolute URL a navigation asked for, on the public origin. It is
    /// built on the origin so a path like `//elsewhere` cannot redirect off the
    /// relay.
    fn location(&self, req: &Request<'_>) -> String {
        let mut location = format!("{}{}", self.config.public_origin, req.path);
        if let Some(query) = req.query.filter(|query| !query.is_empty()) {
            location.push('?');
            location.push_str(query);
        }
        location
    }

    /// Count one decision request against its prefix. Expired windows were
    /// pruned by `sweep`, so a full map holds only live prefixes.
    fn count_decision(&mut self, prefix: Prefix, now_ms: u64) -> Counted {
        if !self.decision_rates.contains_key(&prefix)
            && self.decision_rates.len() >= MAX_TRACKED_PREFIXES
        {
            return Counted::Full;
        }
        let entry = self.decision_rates.entry(prefix).or_insert((now_ms, 0));
        if now_ms.saturating_sub(entry.0) >= DECISION_WINDOW_MS {
            *entry = (now_ms, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        if entry.1 <= DECISION_LIMIT {
            Counted::Ok
        } else {
            Counted::Limited
        }
    }

    fn set_cooldown(&mut self, prefix: Prefix, from_ms: u64) {
        let until = from_ms.saturating_add(COOLDOWN_MS);
        if !self.cooldowns.contains_key(&prefix)
            && self.cooldowns.len() >= MAX_TRACKED_PREFIXES
            && let Some(soonest) = self
                .cooldowns
                .iter()
                .min_by_key(|(_, ends)| **ends)
                .map(|(prefix, _)| *prefix)
        {
            // Only reachable when the map filled after this prefix's pending
            // was admitted. Drop the cooldown that ends soonest to hold the cap.
            self.cooldowns.remove(&soonest);
        }
        let entry = self.cooldowns.entry(prefix).or_insert(0);
        *entry = (*entry).max(until);
    }

    /// Apply expiry: an approval that times out starts its prefix's cooldown
    /// and is removed. An approval nobody claimed is dropped when its claim
    /// window (the approval TTL from the decision) ends.
    fn sweep(&mut self, now_ms: u64) {
        let ttl_ms = self.ttl_ms;
        let mut expired = Vec::new();
        self.pending.retain(|pending| match pending.state {
            State::Waiting if pending.expires_ms <= now_ms => {
                expired.push((pending.prefix, pending.expires_ms));
                false
            }
            State::Waiting => true,
            State::Approved => pending.decided_ms.saturating_add(ttl_ms) > now_ms,
        });
        for (prefix, ended_at) in expired {
            self.set_cooldown(prefix, ended_at);
        }
        self.cooldowns.retain(|_, until| *until > now_ms);
        self.decision_rates
            .retain(|_, (start, _)| now_ms.saturating_sub(*start) < DECISION_WINDOW_MS);
    }
}

fn ntfy_publish(
    ntfy: &NtfyConfig,
    origin: &str,
    prefix: Prefix,
    approve: &str,
    deny: &str,
) -> GateEffect {
    let decision_url = format!("{origin}/__gate/decision");
    let body = serde_json::json!({
        "topic": ntfy.topic.as_str(),
        "title": "BubblePlay access request",
        "message": format!("Browser navigation awaiting approval from client prefix {prefix}"),
        "tags": ["lock"],
        "actions": [
            {
                "action": "http",
                "label": "Approve",
                "url": decision_url.as_str(),
                "method": "POST",
                "headers": { "Authorization": format!("Bearer {approve}") },
                "body": "approve",
                "clear": true
            },
            {
                "action": "http",
                "label": "Deny",
                "url": decision_url.as_str(),
                "method": "POST",
                "headers": { "Authorization": format!("Bearer {deny}") },
                "body": "deny",
                "clear": true
            }
        ]
    });
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if let Some(token) = &ntfy.token {
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    GateEffect::NtfyPublish {
        url: ntfy.url.clone(),
        headers,
        body: serde_json::to_vec(&body).expect("serializing a JSON object cannot fail"),
    }
}

fn cookie_hashes(cookie: Option<&str>) -> impl Iterator<Item = [u8; 32]> + '_ {
    cookie
        .unwrap_or("")
        .split(';')
        .filter_map(|part| part.trim().strip_prefix(COOKIE_PREFIX))
        .map(sha256)
}

fn set_cookie(value: &str) -> String {
    format!(
        "{COOKIE_NAME}={value}; Max-Age={COOKIE_MAX_AGE_SECS}; Path=/; Secure; HttpOnly; SameSite=Lax"
    )
}

fn no_content() -> Response {
    Response {
        status: 204,
        headers: common_headers(),
        body: Vec::new(),
    }
}

/// A plain 403 for a gated request that cannot open an approval.
fn forbidden() -> Response {
    plain(403, "Forbidden")
}

/// A plain 429 for a gate endpoint that is over its capacity.
fn too_many_requests() -> Response {
    plain(429, "Too many requests.")
}

fn plain(status: u16, text: &str) -> Response {
    let mut headers = common_headers();
    headers.push(("Content-Type".into(), "text/plain; charset=utf-8".into()));
    Response {
        status,
        headers,
        body: text.as_bytes().to_vec(),
    }
}

/// `Sec-Fetch-Dest` is `document` for a top-level navigation. A request with no
/// such header is treated as one, for clients that do not send it.
fn is_navigation(dest: Option<&str>) -> bool {
    dest.is_none_or(|value| value.trim().eq_ignore_ascii_case("document"))
}

/// True for `scheme://host[:port]` with a lowercase `http` or `https` scheme, a
/// lowercase host, and a port only when it is not the scheme's default. No
/// userinfo, path, query or fragment.
fn is_canonical_origin(origin: &str) -> bool {
    if origin != origin.to_ascii_lowercase() || origin.bytes().any(|b| b.is_ascii_whitespace()) {
        return false;
    }
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    let default_port = match scheme {
        "http" => "80",
        "https" => "443",
        _ => return false,
    };
    if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
        return false;
    }
    let port = if let Some(rest) = authority.strip_prefix('[') {
        // An IPv6 literal in brackets, then an optional port.
        let Some(end) = rest.find(']') else {
            return false;
        };
        let literal = &rest[..end];
        let literal_ok = !literal.is_empty()
            && literal
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.');
        if !literal_ok {
            return false;
        }
        match &rest[end + 1..] {
            "" => None,
            after => match after.strip_prefix(':') {
                Some(port) => Some(port),
                None => return false,
            },
        }
    } else {
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        let host_ok = !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
        if !host_ok {
            return false;
        }
        port
    };
    match port {
        None => true,
        Some(port) => {
            // Digits only, no leading zero (so no "0" either), and not the
            // scheme's default port, which the canonical form omits.
            !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit())
                && !port.starts_with('0')
                && port != default_port
                && port.parse::<u16>().is_ok()
        }
    }
}

fn common_headers() -> Vec<(String, String)> {
    vec![
        ("Cache-Control".into(), "no-store".into()),
        ("X-Content-Type-Options".into(), "nosniff".into()),
        ("X-Frame-Options".into(), "DENY".into()),
        ("Referrer-Policy".into(), "no-referrer".into()),
    ]
}

fn sha256(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn random_string(rng: &mut dyn RelayRng, len: usize) -> String {
    let mut buf = vec![0u8; len];
    rng.fill(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

/// 32 random bytes, base64url without padding.
fn random_token(rng: &mut dyn RelayRng) -> String {
    random_string(rng, 32)
}
