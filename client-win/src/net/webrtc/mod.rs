//! str0m 0.21 on the owned network thread. The caller drives UDP and deadlines;
//! no WebRTC runtime, device I/O, or detached worker is hidden in this module.
pub mod auto;
pub mod channels;
pub mod depacketize;
mod ice;
pub mod ingress;
mod offer;
mod peer;
pub mod stats;
pub use peer::Peer;
#[cfg(test)]
mod stats_tests;
