//! Loopback fake relay + host for WSS v4. Test tool only: it is not shipped and
//! shares no code with the lead's client crate. The protocol and stream
//! modules are a portable copy of the wire rules in `host/src/wss_v4.rs`, the
//! worker's `media-v4.js` and the recorded spike streams.

pub mod cli;
pub mod evidence;
pub mod model;
pub mod protocol;
pub mod report;
pub mod server;
pub mod signal;
pub mod stream;
