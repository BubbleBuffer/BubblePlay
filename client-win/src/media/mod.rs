//! Hardware video decode and presentation for the Windows client (WSS v4).
//!
//! Data flow: the session thread passes Annex-B access units to a
//! [`DecodeBackend`] (usually [`MfBackend`]), which returns NV12 textures on the
//! shared shell device. Decoded frames go to a [`FrameSink`] (the mailbox), and
//! [`VideoRenderer`] draws the newest one as the shell's `VideoLayer`. Nothing
//! here depends on the transport, and nothing copies frames to the CPU.
//!
//! Threads: `MediaRuntime` per thread, decode on one thread, drawing on the UI
//! thread. The session module owns the threads and connects their channels.

mod backend;
mod mf;
mod runtime;
mod stats;
mod video;

#[cfg(test)]
mod tests;

pub use backend::{Codec, DecodeBackend, DecodedFrame, FrameLease};
pub use mf::{hardware_decoder_available, resolve_codec, CodecRequest, MfBackend};
pub use runtime::MediaRuntime;
pub use stats::{MediaStats, StatsSnapshot, Totals};
pub use video::{FrameSink, TraceFn, VideoRenderer, WakeFn};
