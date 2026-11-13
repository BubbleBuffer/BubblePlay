//! Native shell of the BubblePlay relay: config, state files, subcommands and the static asset
//! handler, and the relay and gate wired to axum.

pub mod app;
pub mod assets;
pub mod config;
pub mod health;
pub mod import;
pub mod init;
pub mod limits;
pub mod log;
pub mod net;
pub mod runtime;
pub mod server;
pub mod socket;
pub mod store;
