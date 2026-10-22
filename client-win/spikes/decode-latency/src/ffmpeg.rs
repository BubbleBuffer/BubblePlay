//! FFmpeg d3d11va on the presenter's existing device. A tiny C shim accesses the
//! installed FFmpeg headers, so no FFmpeg-owned struct ABI is duplicated here.
//! Decoder surfaces are handed directly to the presenter with an AVFrame lease.

use crate::backend::{Codec, DecodeBackend, DecodedFrame};
use anyhow::{anyhow, bail, Context, Result};
use std::{
    ffi::{c_char, c_int, c_void, CStr},
    ptr::{self, NonNull},
};
use windows::{
    core::{IUnknown, Interface},
    Win32::Graphics::{
        Direct3D11::{ID3D11Device, ID3D11Texture2D, D3D11_BIND_DECODER, D3D11_TEXTURE2D_DESC},
        Dxgi::Common::DXGI_FORMAT_NV12,
    },
};

// Only this small, locally defined plain-data struct crosses the language ABI.
#[repr(C)]
#[derive(Default)]
struct FrameInfo {
    texture: *mut c_void,
    array_slice: isize,
    pts_100ns: i64,
    width: c_int,
    height: c_int,
    flags: c_int,
    decode_error_flags: c_int,
}

extern "C" {
    fn bp_ffmpeg_create(
        codec: c_int,
        device: *mut c_void,
        width: c_int,
        height: c_int,
        context: *mut *mut c_void,
    ) -> c_int;
    fn bp_ffmpeg_free(context: *mut *mut c_void);
    fn bp_ffmpeg_send(context: *mut c_void, data: *const u8, size: c_int, pts: i64) -> c_int;
    fn bp_ffmpeg_send_eof(context: *mut c_void) -> c_int;
    fn bp_ffmpeg_frame_alloc() -> *mut c_void;
    fn bp_ffmpeg_frame_free(frame: *mut *mut c_void);
    fn bp_ffmpeg_receive(context: *mut c_void, frame: *mut c_void) -> c_int;
    fn bp_ffmpeg_frame_info(frame: *const c_void, info: *mut FrameInfo) -> c_int;
    fn bp_ffmpeg_error_kind(result: c_int) -> c_int;
    fn bp_ffmpeg_error_text(result: c_int, buffer: *mut c_char, capacity: usize);
    fn bp_ffmpeg_description(context: *const c_void, buffer: *mut c_char, capacity: usize);
}

fn ffmpeg_error(operation: &str, code: c_int) -> anyhow::Error {
    let mut buffer = [0 as c_char; 256];
    unsafe {
        bp_ffmpeg_error_text(code, buffer.as_mut_ptr(), buffer.len());
    }
    let message = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy();
    anyhow!("{operation}: {message} ({code})")
}

struct FrameLease(NonNull<c_void>);
impl FrameLease {
    fn new() -> Result<Self> {
        NonNull::new(unsafe { bp_ffmpeg_frame_alloc() })
            .map(Self)
            .context("av_frame_alloc returned null")
    }
}
impl Drop for FrameLease {
    fn drop(&mut self) {
        let mut frame = self.0.as_ptr();
        unsafe {
            bp_ffmpeg_frame_free(&mut frame);
        }
    }
}

pub struct FFmpegBackend {
    context: NonNull<c_void>,
    device_identity: IUnknown,
    width: u32,
    height: u32,
    // Reuse an empty AVFrame while polling EAGAIN. Once filled, ownership is
    // transferred to DecodedFrame; the next poll allocates a new receive slot.
    receive_slot: Option<FrameLease>,
    input_backpressured: bool,
    draining: bool,
    flush_sent: bool,
    drained: bool,
    description: String,
}

impl FFmpegBackend {
    pub fn new(codec: Codec, device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            bail!("Invalid FFmpeg stream dimensions");
        }
        let c_width: c_int = width.try_into().context("width exceeds i32")?;
        let c_height: c_int = height.try_into().context("height exceeds i32")?;
        let device_identity: IUnknown = device.cast()?;
        let receive_slot = Some(FrameLease::new()?);
        let mut raw = ptr::null_mut();
        let result = unsafe {
            bp_ffmpeg_create(
                match codec {
                    Codec::H264 => 0,
                    Codec::Hevc => 1,
                },
                device.as_raw(),
                c_width,
                c_height,
                &mut raw,
            )
        };
        if result < 0 {
            return Err(ffmpeg_error("FFmpeg d3d11va initialization", result));
        }
        let context = NonNull::new(raw).context("FFmpeg returned a null codec context")?;
        let mut buffer = [0 as c_char; 1024];
        unsafe {
            bp_ffmpeg_description(context.as_ptr(), buffer.as_mut_ptr(), buffer.len());
        }
        let description = unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        Ok(Self {
            context,
            device_identity,
            width,
            height,
            receive_slot,
            input_backpressured: false,
            draining: false,
            flush_sent: false,
            drained: false,
            description,
        })
    }

    // drain() cannot return backpressure in the shared trait. Defer an EAGAIN
    // flush to poll(), which retrieves buffered output and retries the NULL
    // packet on the next call without swallowing any decoded frame.
    fn try_send_flush(&mut self) -> Result<()> {
        if self.flush_sent || self.drained {
            return Ok(());
        }
        let result = unsafe { bp_ffmpeg_send_eof(self.context.as_ptr()) };
        if result >= 0 {
            self.flush_sent = true;
        } else {
            match unsafe { bp_ffmpeg_error_kind(result) } {
                1 => {}
                2 => self.flush_sent = true,
                _ => return Err(ffmpeg_error("avcodec_send_packet(NULL)", result)),
            }
        }
        Ok(())
    }
}

impl DecodeBackend for FFmpegBackend {
    fn submit(&mut self, access_unit: &[u8], pts_100ns: i64) -> Result<bool> {
        if self.draining {
            bail!("Cannot submit after FFmpeg drain started");
        }
        if access_unit.is_empty() {
            bail!("Empty FFmpeg access unit");
        }
        if pts_100ns == i64::MIN {
            bail!("PTS is FFmpeg's reserved AV_NOPTS_VALUE");
        }
        let size: c_int = access_unit.len().try_into().context("AU larger than i32")?;
        let result =
            unsafe { bp_ffmpeg_send(self.context.as_ptr(), access_unit.as_ptr(), size, pts_100ns) };
        if result >= 0 {
            self.input_backpressured = false;
            return Ok(true);
        }
        match unsafe { bp_ffmpeg_error_kind(result) } {
            1 => {
                self.input_backpressured = true;
                Ok(false)
            }
            _ => Err(ffmpeg_error("avcodec_send_packet", result)),
        }
    }

    fn poll(&mut self) -> Result<Option<DecodedFrame>> {
        if self.drained {
            return Ok(None);
        }
        if self.draining {
            self.try_send_flush()?;
        }
        if self.receive_slot.is_none() {
            self.receive_slot = Some(FrameLease::new()?);
        }
        let slot = self
            .receive_slot
            .as_ref()
            .expect("receive slot initialized");
        let result = unsafe { bp_ffmpeg_receive(self.context.as_ptr(), slot.0.as_ptr()) };
        if result < 0 {
            return match unsafe { bp_ffmpeg_error_kind(result) } {
                1 if self.draining => {
                    bail!("FFmpeg receive returned EAGAIN during drain (flush_sent={}); invalid send/receive state", self.flush_sent)
                }
                1 if self.input_backpressured => {
                    bail!(
                        "FFmpeg send and receive both returned EAGAIN; invalid send/receive state"
                    )
                }
                1 => Ok(None),
                2 if self.draining && self.flush_sent => {
                    self.drained = true;
                    Ok(None)
                }
                2 => bail!("FFmpeg returned EOF before requested drain"),
                _ => Err(ffmpeg_error("avcodec_receive_frame", result)),
            };
        }
        self.input_backpressured = false;
        let mut info = FrameInfo::default();
        let result = unsafe { bp_ffmpeg_frame_info(slot.0.as_ptr(), &mut info) };
        if result < 0 {
            return Err(ffmpeg_error(
                "FFmpeg output must be timestamped D3D11/NV12 hardware frame",
                result,
            ));
        }
        if info.flags != 0 || info.decode_error_flags != 0 {
            bail!(
                "FFmpeg returned a corrupt frame: flags={}, decode_error_flags={}",
                info.flags,
                info.decode_error_flags
            );
        }
        if info.width != self.width as i32 || info.height != self.height as i32 {
            bail!(
                "FFmpeg changed output dimensions to {}x{}; expected {}x{}",
                info.width,
                info.height,
                self.width,
                self.height
            );
        }
        let array_slice: u32 = info
            .array_slice
            .try_into()
            .context("Invalid FFmpeg texture array slice")?;
        // The pointer is borrowed from the retained AVFrame. Clone AddRefs the
        // texture independently; the AVFrame lease also prevents pool reuse.
        let texture = unsafe { ID3D11Texture2D::from_raw_borrowed(&info.texture) }
            .context("FFmpeg returned a null texture")?
            .clone();
        unsafe {
            let surface_device: IUnknown = texture.GetDevice()?.cast()?;
            if surface_device != self.device_identity {
                bail!("FFmpeg output belongs to a different canonical D3D11 device");
            }
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            if desc.Format != DXGI_FORMAT_NV12 {
                bail!("FFmpeg texture format is {:?}, expected NV12", desc.Format);
            }
            if desc.BindFlags & D3D11_BIND_DECODER.0 as u32 == 0 {
                bail!("FFmpeg texture lacks D3D11_BIND_DECODER");
            }
            if desc.MipLevels != 1
                || desc.SampleDesc.Count != 1
                || array_slice >= desc.ArraySize
                || desc.Width < self.width
                || desc.Height < self.height
            {
                bail!("Invalid FFmpeg surface layout: slice={array_slice}, desc={desc:?}");
            }
        }
        let lease = self.receive_slot.take().expect("received frame lease");
        Ok(Some(DecodedFrame {
            pts_100ns: info.pts_100ns,
            texture,
            array_slice,
            _lease: Box::new(lease),
        }))
    }

    fn drain(&mut self) -> Result<()> {
        self.draining = true;
        self.try_send_flush()
    }
    fn is_drained(&self) -> bool {
        self.drained
    }
    fn description(&self) -> &str {
        &self.description
    }
}

impl Drop for FFmpegBackend {
    fn drop(&mut self) {
        let mut context = self.context.as_ptr();
        unsafe {
            bp_ffmpeg_free(&mut context);
        }
    }
}
