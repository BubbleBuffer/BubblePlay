//! Admin mode: a small elevated input agent that injects the user-level
//! host's input so it reaches elevated windows. Only `protocol`, `session`
//! and `trust` are compiled into the agent binary; `client` is host/tray only.
pub mod client;
pub mod protocol;
pub mod session;
pub mod trust;
