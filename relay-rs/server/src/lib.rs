//! Native shell of the BubblePlay relay: config, state files, subcommands and the static asset
//! handler. The relay and gate state machines are wired in later.

pub mod app;
pub mod assets;
pub mod config;
pub mod health;
pub mod import;
pub mod init;
#[cfg(feature = "lab")]
pub mod lab;
pub mod net;
pub mod server;
pub mod store;
