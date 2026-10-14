//! Transport-independent decode contract: Annex-B access units in, decoded
//! NV12 textures on the shared D3D11 device out. Nothing here knows about
//! WSS, WebRTC or the relay.

use anyhow::Result;
use windows::Win32::{
    Foundation::RECT, Graphics::Direct3D11::ID3D11Texture2D, Media::MediaFoundation::IMFSample,
};

/// Codec of an access unit. [`Codec::wire_id`] is the WSS v4 header byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    /// WSS v4 header codec byte: 1 = HEVC, 2 = H.264.
    pub fn wire_id(self) -> u8 {
        match self {
            Codec::H264 => 2,
            Codec::Hevc => 1,
        }
    }

    /// Inverse of [`Codec::wire_id`]; `None` for any other value.
    pub fn from_wire_id(id: u8) -> Option<Self> {
        match id {
            1 => Some(Codec::Hevc),
            2 => Some(Codec::H264),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
        }
    }
}

/// Keeps a decoder output sample alive. Dropping it returns the surface to the
/// decoder pool, so the video layer holds it until the GPU has finished with
/// the texture (see `video::FrameSink`).
pub struct FrameLease {
    _sample: IMFSample,
}

impl FrameLease {
    pub(super) fn new(sample: IMFSample) -> Self {
        Self { _sample: sample }
    }
}

// SAFETY: the sample comes from an MTA Media Foundation runtime, where its
// reference count is thread-safe. It is only ever dropped, never used, on a
// thread other than the one that created it.
unsafe impl Send for FrameLease {}

/// One decoded picture. Moves once, decode thread -> mailbox -> UI thread.
pub struct DecodedFrame {
    pub codec: Codec,
    /// Presentation timestamp in 100 ns units, unique per access unit.
    pub pts_100ns: i64,
    /// NV12 surface on the device passed to the backend. Bound as a decoder surface.
    pub texture: ID3D11Texture2D,
    /// Slice of `texture` holding this picture (texture arrays).
    pub array_slice: u32,
    /// Visible picture size, not the padded surface size.
    pub width: u32,
    pub height: u32,
    /// Visible image region within the coded NV12 surface, in source pixels.
    pub source_rect: RECT,
    /// Keeps the surface out of the decoder pool until it is safe to reuse.
    pub lease: FrameLease,
}

// SAFETY: `texture` and `lease` are free-threaded COM objects created on a
// device with multithread protection (see `Shell::device`). Access is serialized
// by that device's lock, and each frame is owned by one thread at a time.
unsafe impl Send for DecodedFrame {}

/// A hardware decoder that accepts Annex-B access units and returns GPU surfaces.
/// Implementations never copy output to CPU memory.
pub trait DecodeBackend {
    /// Queues one access unit. `Ok(false)` is backpressure: the caller keeps the
    /// access unit, polls, and retries. `pts_100ns` must be unique among units
    /// not yet returned by [`DecodeBackend::poll`].
    fn submit(&mut self, access_unit: &[u8], pts_100ns: i64) -> Result<bool>;

    /// Returns the next decoded picture, or `None` when nothing is ready.
    /// Async MFT events are polled without waiting. Synchronous MFT calls can
    /// wait for a decoder surface; the UI must keep retiring its GPU leases.
    fn poll(&mut self) -> Result<Option<DecodedFrame>>;

    /// Starts end-of-stream. Keep polling until [`DecodeBackend::is_drained`].
    fn drain(&mut self) -> Result<()>;

    fn is_drained(&self) -> bool;

    fn description(&self) -> &str;
}
