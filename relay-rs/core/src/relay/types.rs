//! The types the adapter sees: configuration, requests, actions and errors.

use serde::{Deserialize, Serialize};

use super::pcs::PcTable;
use super::preauth::{HELLO_TIMEOUT_MS, PROOF_TIMEOUT_MS};

/// Identifies one upgraded socket. Ids are never reused within a Relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
    /// Replace the socket's attachment with this snapshot. Emitted after each
    /// change the snapshot records and before the sends that depend on it. An
    /// adapter without hibernation ignores it. See `ConnSnapshot`.
    Snapshot(ConnId, ConnSnapshot),
}

/// The seven WebSocket routes. The adapter maps the request path to one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// An accepted upgrade.
#[derive(Clone, Debug, PartialEq)]
pub struct Upgraded {
    /// The new socket.
    pub id: ConnId,
    /// `Close` actions for pending sockets the relay dropped to make room. The adapter carries
    /// them out like any other close.
    pub evicted: Vec<Action>,
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

/// The default for `CoreConfig::max_connections`: sockets per PC object.
pub const DEFAULT_MAX_CONNECTIONS: usize = 32;

/// Protocol timings. Defaults are the settled values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timings {
    /// Deadline for the first hello on any socket (`HELLO_TIMEOUT_MS`).
    pub hello_ms: u64,
    /// Deadline for register-proof after register-challenge (`PROOF_TIMEOUT_MS`, single use).
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
            hello_ms: HELLO_TIMEOUT_MS,
            challenge_ms: PROOF_TIMEOUT_MS,
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
    /// Maximum sockets in the relay (default `DEFAULT_MAX_CONNECTIONS`).
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
            max_connections: DEFAULT_MAX_CONNECTIONS,
            turn: None,
            timings: Timings::default(),
        }
    }
}

/// The handshake phase a snapshot records. Pre-auth sockets are closed on
/// rehydrate, so only `Live` survives; the others are kept to say what was closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnPhase {
    /// Waiting for the first hello.
    Hello,
    /// Host registration: the challenge is out and the register-proof is awaited.
    HostProof,
    /// Viewer signalling: the challenge is out and the register-proof is awaited.
    ViewerProof,
    /// Hello done: the socket forwards messages.
    Live,
}

/// A signalling ticket as a snapshot holds it: only the SHA-256 of its text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotTicket {
    /// SHA-256 of the ticket text. Base64url in the serialized form.
    #[serde(with = "b64::fixed")]
    pub hash: [u8; 32],
    /// Unix ms at which the ticket expires.
    pub expires_ms: u64,
}

/// A host's open pairing window as a snapshot holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotPairWindow {
    /// The value a viewer must present. Base64url in the serialized form.
    #[serde(with = "b64::fixed")]
    pub admit: [u8; 32],
    /// Unix ms at which the window expires.
    pub expires_ms: u64,
    /// Failed attempts against the window so far (below 20 while it is open).
    pub failures: u32,
}

/// What a socket needs to come back after the Durable Object hibernates. The
/// adapter stores one per socket in its attachment (serialized as JSON) and
/// passes them all to `Relay::rehydrate`. Pending viewer messages, the
/// per-prefix pairing counts and challenge nonces are not part of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnSnapshot {
    /// The socket's id, so the adapter can match it to the live socket.
    pub id: ConnId,
    /// The route the socket came in on.
    pub route: Route,
    /// The canonical PC id. Empty only for a version probe.
    pub pc: String,
    /// The socket's epoch: set for data sockets and admitted viewers, else empty.
    pub epoch: String,
    /// Where the socket is in its handshake.
    pub phase: ConnPhase,
    /// The admitted viewer's device id. Empty for hosts and pre-auth sockets.
    pub device: String,
    /// True while the viewer is in pairing mode.
    pub pairing: bool,
    /// The last activity recorded, in Unix ms. It can lag the real last
    /// activity by less than the step (idle / 4).
    pub activity_ms: u64,
    /// The signalling ticket, if one is held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<SnapshotTicket>,
    /// The host's open pairing window. Host sockets only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pair_window: Option<SnapshotPairWindow>,
}

/// Serde helpers: byte values as unpadded base64url strings.
mod b64 {
    /// Any byte container, and the reader for it.
    pub mod any {
        use base64::Engine as _;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use serde::{Deserialize, Deserializer, Serializer};

        pub fn serialize<T: AsRef<[u8]>, S: Serializer>(
            bytes: &T,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            serializer.serialize_str(&URL_SAFE_NO_PAD.encode(bytes.as_ref()))
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Vec<u8>, D::Error> {
            let text = String::deserialize(deserializer)?;
            URL_SAFE_NO_PAD
                .decode(text)
                .map_err(<D::Error as serde::de::Error>::custom)
        }
    }

    /// Exactly 32 bytes.
    pub mod fixed {
        pub fn serialize<S: serde::Serializer>(
            bytes: &[u8; 32],
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            super::any::serialize(bytes, serializer)
        }

        pub fn deserialize<'de, D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> Result<[u8; 32], D::Error> {
            let bytes = super::any::deserialize(deserializer)?;
            let array: [u8; 32] = bytes
                .try_into()
                .map_err(|_| <D::Error as serde::de::Error>::custom("expected 32 bytes"))?;
            Ok(array)
        }
    }
}
