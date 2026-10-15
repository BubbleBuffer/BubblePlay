//! Windows client transports: `/ws` signaling and input, str0m WebRTC over
//! owned UDP, WSS v4 media and auto fallback, epoch gates and reconnects.
//!
//! The entry point is [`run`], called on an owned worker thread. The API is
//! documented in `F:/build/win-client-step1/net-api.md`.

mod config;
mod cookies;
mod error;
mod events;
mod gate;
mod input;
#[cfg(any(test, feature = "dev-login"))]
mod login;
mod protocol;
mod session;
mod stats;
pub(crate) mod webrtc;

#[cfg(test)]
mod fake_relay;
#[cfg(test)]
mod integration_contract_tests;
#[cfg(test)]
mod session_tests;

pub use config::{Codec, NetConfig, TransportChoice, AUTO_ICE_CHECKING_TIMEOUT};
pub use cookies::CookieJar;
#[cfg(any(test, feature = "dev-login"))]
pub use cookies::{GATE_COOKIE, SESSION_COOKIE};
#[cfg(feature = "dev-login")]
pub use error::LoginError;
pub use error::NetError;
pub use events::{DecoderFeedback, NetEvent, VideoFrame};
pub use input::{InputLink, InputSnapshot};
#[cfg(feature = "dev-login")]
pub use login::login_blocking;
#[cfg(test)]
pub use login::login_with_cookies;
#[cfg(any(test, feature = "dev-login"))]
pub use login::LoginRequest;
pub use session::run;
pub use stats::{NetState, TransportStats};
