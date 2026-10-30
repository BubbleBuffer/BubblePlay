//! BubblePlay Flutter app, Windows core.
//!
//! [`backend::AppBackend`] is the real `Backend` behind the app's snapshot / act / notify
//! contract; [`contract::Core`] carries `AppState` and `Action` as JSON, and [`api`] exposes
//! that to Dart through flutter_rust_bridge.

pub mod api;
pub mod backend;
pub mod contract;
mod frb_generated;
mod link;
pub mod stream_host;
