//! Values passed between the net worker thread and the decoder.

use super::config::Codec;
use std::time::Instant;

/// One access unit that passed the receive gate, in arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    /// The local epoch of the connection that received it (see [`NetEvent::Reset`]).
    pub local_epoch: u64,
    pub codec: Codec,
    pub key: bool,
    pub refine: bool,
    pub generation: u32,
    pub seq: u32,
    pub capture_ts_us: u64,
    pub width: u16,
    pub height: u16,
    /// Annex-B access unit, without the 28-byte BPV4 header.
    pub data: Vec<u8>,
    /// Socket receipt time before ACK, receive gating and decoder queuing.
    pub received_at: Instant,
}

/// Decoder state must be discarded. Sent at every connection start, after a host
/// join, and on every gate invalidation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetEvent {
    pub local_epoch: u64,
    pub reason: String,
}

/// Events delivered over the bounded channel to the decoder, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetEvent {
    Frame(VideoFrame),
    Reset(ResetEvent),
}

/// Decoder feedback. Each message carries the `local_epoch` it belongs to, and
/// the net ignores any message from an older epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecoderFeedback {
    /// The frame decoded; updates decodedSeq in subsequent ACKs.
    Decoded {
        local_epoch: u64,
        generation: u32,
        seq: u32,
    },
    /// The frame was first drawn successfully; completes first-frame proof.
    Presented {
        local_epoch: u64,
        generation: u32,
        seq: u32,
    },
    /// The decoder failed. The gate is invalidated and a keyframe requested.
    DecodeError { local_epoch: u64, reason: String },
    /// The decoder needs a keyframe. Handled like `DecodeError`.
    KeyframeNeeded { local_epoch: u64, reason: String },
}

impl DecoderFeedback {
    pub fn local_epoch(&self) -> u64 {
        match self {
            DecoderFeedback::Decoded { local_epoch, .. }
            | DecoderFeedback::Presented { local_epoch, .. }
            | DecoderFeedback::DecodeError { local_epoch, .. }
            | DecoderFeedback::KeyframeNeeded { local_epoch, .. } => *local_epoch,
        }
    }
}
