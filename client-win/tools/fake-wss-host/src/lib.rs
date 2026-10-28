//! Loopback fake relay + host for WSS v4. Test tool only: it is not shipped and
//! shares no code with the lead's client crate. The protocol and stream
//! modules are a portable copy of the wire rules in `host/src/wss_v4.rs`, the
//! worker's `media-v4.js` and the recorded spike streams.

pub mod audio;
pub mod cli;
pub mod cursor;
pub mod evidence;
pub mod input_record;
pub mod model;
pub mod monitors;
pub mod phase2;
pub mod protocol;
pub mod report;
pub mod rtc;
pub mod server;
pub mod signal;
pub mod stream;
