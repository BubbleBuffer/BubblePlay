//! BubblePlay Windows client core.
//!
//! The network, media, audio, input and key/PC-store code, and the `model` boundary
//! (`Backend`, `AppState`, `Action`). With the default `egui-client` feature it also holds
//! the egui window client (`shell`, `ui`, `mock`, `session`). The Flutter app links this
//! crate with `default-features = false`.

// Without the egui client the windowed session does not exist, so some net helpers are unused.
#![cfg_attr(not(feature = "egui-client"), allow(dead_code))]

pub mod audio;
pub mod input;
pub mod media;
pub mod model;
pub mod net;
pub mod store;
pub mod stream;
pub mod surface;

#[cfg(feature = "egui-client")]
pub mod mock;
#[cfg(feature = "egui-client")]
pub mod session;
#[cfg(feature = "egui-client")]
pub mod shell;
#[cfg(feature = "egui-client")]
pub mod ui;
