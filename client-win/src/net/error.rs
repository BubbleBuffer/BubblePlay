//! Error types for the net module.

use std::fmt;

/// Why a login request did not produce session cookies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginError {
    /// Wrong password or TOTP (HTTP 401).
    Rejected,
    /// Too many attempts (HTTP 429); `retry_after_secs` comes from `Retry-After`.
    RateLimited { retry_after_secs: Option<u64> },
    /// The relay is not healthy yet (HTTP 503).
    Unavailable,
    /// Any other HTTP status.
    Status(u16),
    /// HTTP 204 without both `__Host-gate` and `__Host-bp-session`.
    MissingCookies,
    /// Connection, TLS or timeout failure.
    Transport(String),
}

/// Errors from the net module. Transient errors never leave [`crate::net::run`];
/// they reconnect. Only the fatal ones are returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetError {
    /// The relay URL is malformed or not an origin (fatal).
    InvalidRelayUrl(String),
    /// `http://` or `ws://` to a host other than `127.0.0.1` (fatal).
    PlaintextRefused(String),
    /// Login failed (returned by `login`, never by `run`).
    Login(LoginError),
    /// A WebSocket handshake answered 401 or 403; cookies are missing or stale (fatal).
    Auth(u16),
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
                | NetError::RelayError(_)
        )
    }
}

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoginError::Rejected => write!(f, "login rejected (401)"),
            LoginError::RateLimited {
                retry_after_secs: Some(secs),
            } => {
                write!(f, "login rate limited (429), retry after {secs} s")
            }
            LoginError::RateLimited {
                retry_after_secs: None,
            } => write!(f, "login rate limited (429)"),
            LoginError::Unavailable => write!(f, "relay unavailable (503)"),
            LoginError::Status(status) => write!(f, "login failed (HTTP {status})"),
            LoginError::MissingCookies => write!(f, "login succeeded without session cookies"),
            LoginError::Transport(message) => write!(f, "login transport error: {message}"),
        }
    }
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::InvalidRelayUrl(url) => write!(f, "invalid relay URL: {url}"),
            NetError::PlaintextRefused(url) => {
                write!(
                    f,
                    "plaintext relay refused (only https, or http to 127.0.0.1): {url}"
                )
            }
            NetError::Login(error) => write!(f, "{error}"),
            NetError::Auth(status) => write!(f, "relay refused the session (HTTP {status})"),
            NetError::RelayError(message) => write!(f, "relay error: {message}"),
            NetError::Handshake(message) => write!(f, "handshake failed: {message}"),
            NetError::Protocol(message) => write!(f, "protocol error: {message}"),
            NetError::Socket(message) => write!(f, "socket error: {message}"),
            NetError::Stalled(name) => write!(f, "{name} socket silent past the dead interval"),
            NetError::FirstFrameTimeout => write!(f, "no first frame after retry"),
        }
    }
}

impl std::error::Error for LoginError {}
impl std::error::Error for NetError {}
