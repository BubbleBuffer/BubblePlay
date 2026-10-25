//! Error types for the net module.

use std::fmt;

/// Errors from the net module. Transient errors never leave [`crate::net::run`];
/// they reconnect. Only the fatal ones are returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetError {
    /// The relay URL is malformed or not an origin (fatal).
    InvalidRelayUrl(String),
    /// `http://` or `ws://` to a host that is not loopback (fatal).
    PlaintextRefused(String),
    /// A WebSocket upgrade answered 401 or 403 (fatal).
    Auth(u16),
    /// The relay closed `/ws` with 1008 before `hello-ok`: the signed hello or proof was
    /// refused, or this viewer is not admitted to the PC (fatal).
    NotAdmitted,
    /// The relay sent `{"type":"error"}` on `/ws` (fatal).
    RelayError(String),
    /// Handshake failed or timed out (transient).
    Handshake(String),
    /// A relay message broke the contract (transient).
    Protocol(String),
    /// Socket I/O or WebSocket framing failed (transient).
    Socket(String),
    /// A socket heard nothing for the dead interval (transient).
    Stalled(&'static str),
    /// No decoded frame after the first-frame retry (transient).
    FirstFrameTimeout,
}

impl NetError {
    /// True when retrying cannot help, so `run` returns this error.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            NetError::InvalidRelayUrl(_)
                | NetError::PlaintextRefused(_)
                | NetError::Auth(_)
                | NetError::NotAdmitted
                | NetError::RelayError(_)
        )
    }
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::InvalidRelayUrl(url) => write!(f, "invalid relay URL: {url}"),
            NetError::PlaintextRefused(url) => {
                write!(
                    f,
                    "plaintext relay refused (only https, or http to a loopback host): {url}"
                )
            }
            NetError::Auth(status) => write!(f, "relay refused the session (HTTP {status})"),
            NetError::NotAdmitted => write!(
                f,
                "the relay refused this viewer key (not admitted to the PC, or a bad proof)"
            ),
            NetError::RelayError(message) => write!(f, "relay error: {message}"),
            NetError::Handshake(message) => write!(f, "handshake failed: {message}"),
            NetError::Protocol(message) => write!(f, "protocol error: {message}"),
            NetError::Socket(message) => write!(f, "socket error: {message}"),
            NetError::Stalled(name) => write!(f, "{name} socket silent past the dead interval"),
            NetError::FirstFrameTimeout => write!(f, "no first frame after retry"),
        }
    }
}

impl std::error::Error for NetError {}
