//! The types the adapter sees: configuration, requests, actions and errors.

use super::pcs::PcTable;

/// Identifies one upgraded socket. Ids are never reused within a Relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u64);

/// What the adapter must do with a socket. Actions in one returned Vec run in
/// order. A `PersistPcs` must be durable before the actions after it are sent;
/// see `Relay::set_store_healthy`.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Send one text frame.
    SendText(ConnId, String),
    /// Send one binary frame.
    SendBinary(ConnId, Vec<u8>),
    /// Close the socket with this code and reason. The adapter then calls
    /// `Relay::on_close` for it.
    Close(ConnId, u16, String),
    /// Write the whole PC table to durable storage.
    PersistPcs(PcTable),
}

/// The seven WebSocket routes. The adapter maps the request path to one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Route {
    /// /host-ws?pc=ID: the PC's signalling socket.
    HostWs,
    /// /ws?pc=ID: the viewer's signalling socket.
    Ws,
    /// /host-version-ws: TOKEN-gated version probe; pc is optional.
    HostVersionWs,
    /// /host-media-ws?pc=ID&epoch=E.
    HostMediaWs,
    /// /media-ws?pc=ID&epoch=E.
    MediaWs,
    /// /host-file-ws?pc=ID&epoch=E.
    HostFileWs,
    /// /file-ws?pc=ID&epoch=E.
    FileWs,
}

/// An upgrade the adapter has already checked for Origin and gate cookie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeRequest {
    /// Which route was requested.
    pub route: Route,
    /// The raw `pc` query value, if the request had one. Must be present on every
    /// route except `HostVersionWs`. Parsed and canonicalised here.
    pub pc: Option<String>,
    /// The raw `epoch` query value (data sockets only).
    pub epoch: Option<String>,
    /// True when the site gate passed for this viewer upgrade: no Origin header,
    /// or an Origin with a valid gate cookie. Required by `Ws`, `MediaWs` and
    /// `FileWs`. It does not admit the viewer; that takes the device-key proof.
    pub viewer_authorized: bool,
}

/// The upgrade is refused. The adapter answers HTTP 404.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reject;

/// An admin call that could not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminError {
    /// The PC store is unhealthy. Map to HTTP 503.
    Unavailable,
    /// The id is not a valid device id. Map to HTTP 404.
    NotFound,
}

/// One row of the admin listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcListing {
    /// Canonical device id, `XXXX-XXXX-XXXX-XXXX`.
    pub id: String,
    /// Unpadded base64url of the 32-byte Ed25519 public key.
    pub public: String,
    /// Unix milliseconds of the first registration.
    pub first_seen: u64,
    /// Unix milliseconds of the most recent registration.
    pub last_seen: u64,
    /// True when the PC's host signalling socket is open.
    pub online: bool,
    /// How many viewer IDs the PC's host has admitted. The IDs are never listed.
    pub admitted: usize,
}

/// Protocol timings. Defaults are the settled values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timings {
    /// Deadline for the first hello on any socket (10 s).
    pub hello_ms: u64,
    /// Deadline for register-proof after register-challenge (30 s, single use).
    pub challenge_ms: u64,
    /// Idle timeout, refreshed by every processed message (90 s).
    pub idle_ms: u64,
    /// Lifetime of a signalling ticket (12 h).
    pub ticket_ttl_ms: u64,
    /// Lifetime of a pairing window, from its pair-open (300 s).
    pub pair_window_ms: u64,
    /// The window over which pairing failures are counted per client prefix (300 s).
    pub pair_fail_window_ms: u64,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            hello_ms: 10_000,
            challenge_ms: 30_000,
            idle_ms: 90_000,
            ticket_ttl_ms: 12 * 60 * 60 * 1000,
            pair_window_ms: 300_000,
            pair_fail_window_ms: 300_000,
        }
    }
}

/// TURN settings. The core builds `iceServers` from these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnConfig {
    /// `host` or `host:port`; the port defaults to 3478.
    pub host: String,
    /// The shared HMAC-SHA1 secret (at least 32 bytes; the adapter checks this).
    pub secret: String,
    /// Credential lifetime in seconds.
    pub ttl_s: u64,
}

/// Everything the core needs to know about the deployment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreConfig {
    /// The public origin, already canonical (see bp_device_key::relay_origin).
    pub public_origin: String,
    /// TOKEN: the first-registration ticket and version-probe secret.
    pub token: String,
    /// Maximum sockets in the relay (default 32).
    pub max_connections: usize,
    /// Optional TURN settings.
    pub turn: Option<TurnConfig>,
    /// Protocol timings.
    pub timings: Timings,
}

impl CoreConfig {
    /// A config with the default connection cap, no TURN and the default timings.
    pub fn new(public_origin: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            public_origin: public_origin.into(),
            token: token.into(),
            max_connections: 32,
            turn: None,
            timings: Timings::default(),
        }
    }
}
