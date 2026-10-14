//! Windows client WSS v4 receive path (step 1): relay login, `/ws` signaling,
//! `/media-ws` BPV4 frames, the receive gate and whole-session reconnects.
//!
//! The entry point is [`run`], called on an owned worker thread. The API is
//! documented in `F:/build/win-client-step1/net-api.md`.

// The shell and decoder use parts of this module as they are integrated; until
// then some items have no caller yet.
#![allow(dead_code)]

mod config;
mod cookies;
mod error;
mod events;
mod gate;
mod input;
mod login;
mod protocol;
mod session;
mod stats;

#[cfg(test)]
mod fake_relay;
#[cfg(test)]
mod integration_contract_tests;
#[cfg(test)]
mod session_tests;

pub use config::{Codec, NetConfig};
pub use cookies::{CookieJar, GATE_COOKIE, SESSION_COOKIE};
pub use error::{LoginError, NetError};
pub use events::{DecoderFeedback, NetEvent, ResetEvent, VideoFrame};
pub use input::{CursorState, InputLink, InputSnapshot};
pub use login::{login, login_blocking, login_with_cookies, LoginRequest};
pub use session::run;
pub use stats::{NetState, TransportSnapshot, TransportStats};
