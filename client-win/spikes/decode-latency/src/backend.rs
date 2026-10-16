use anyhow::Result;
use std::any::Any;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

#[derive(Clone, Copy, Debug)]
pub enum Codec {
    H264,
    Hevc,
}

pub struct DecodedFrame {
    pub pts_100ns: i64,
    pub texture: ID3D11Texture2D,
    pub array_slice: u32,
    // Keep the decoder's surface pool lease alive through GPU video processing.
    // Backend-specific ownership (e.g. IMFSample now, AVFrame later).
    pub _lease: Box<dyn Any>,
}

pub trait DecodeBackend {
    // false means backpressure; caller retains this AU and retries after polling.
    fn submit(&mut self, access_unit: &[u8], pts_100ns: i64) -> Result<bool>;
    // Never waits indefinitely. The coordinator pumps HWND messages between calls.
    fn poll(&mut self) -> Result<Option<DecodedFrame>>;
    fn drain(&mut self) -> Result<()>;
    fn is_drained(&self) -> bool;
    fn description(&self) -> &str;
}
