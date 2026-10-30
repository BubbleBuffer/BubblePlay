//! The streaming session core shared by the egui client and the Flutter app: ordered decode,
//! the input capture adapter and the session statistics. Nothing here depends on a window
//! toolkit.

pub mod control;
pub mod decode;
pub mod live_input;
pub mod order;
pub mod player;
pub mod stats;
pub mod target;

pub use stats::{SessionInputState, SessionStats};
