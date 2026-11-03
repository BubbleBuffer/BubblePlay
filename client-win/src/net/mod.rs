//! Windows client transports: `/ws` signaling and input, str0m WebRTC over
//! owned UDP, WSS v4 media and auto fallback, epoch gates and reconnects.
//!
//! The entry point is [`run`], called on an owned worker thread. The API is
//! documented in `F:/build/win-client-step1/net-api.md`.

mod config;
mod controls;
mod ctl_hook;
mod error;
mod events;
mod gate;
mod input;
pub(crate) mod phase2;
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

pub use config::{Codec, NetConfig, TransportChoice, ViewerAuth, AUTO_ICE_CHECKING_TIMEOUT};
/// Only the session tests build a monitor error by name.
#[cfg(test)]
pub use controls::MonitorError;
pub use controls::{ModeOutcome, MonitorEntry, PickOutcome, SessionControls};
pub use ctl_hook::{CtlLogEvent, CtlLogHook, CAP_CLIENT_LOGS};
pub use error::NetError;
pub use events::{DecoderFeedback, NetEvent, VideoFrame};
pub use input::{InputLink, InputSnapshot};
pub use session::{pair, run};
pub use stats::{NetState, TransportStats};
