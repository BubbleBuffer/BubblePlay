//! D3D11 device, flip-model swap chain, back-buffer render target, frame capture.
//! Configuration follows client-win/spikes/decode-latency/src/presenter.rs.

use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;

use anyhow::{Context, Result};
use windows::core::{Interface, BOOL};
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

use super::{Rect, VideoLayer};

/// Clear colour of the canvas behind the video and the UI: #26282D.
const FIELD: [f32; 4] = [
    0x26 as f32 / 255.0,
    0x28 as f32 / 255.0,
    0x2D as f32 / 255.0,
    1.0,
];

/// Device, swap chain and egui renderer for one window.
pub(super) struct Gfx {
    pub(super) egui_ctx: egui::Context,
    renderer: egui_directx11::Renderer,
    rtv: Option<ID3D11RenderTargetView>,
    swap: IDXGISwapChain3,
    latency: HANDLE,
    /// Back buffer size in physical pixels.
    pub(super) size: (u32, u32),
    tearing: bool,
    swap_flags: u32,
    context: ID3D11DeviceContext,
    device: ID3D11Device,
}

impl Gfx {
    /// The shared device (multithread protection is on).
    pub(super) fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub(super) fn new(hwnd: HWND, w: u32, h: u32) -> Result<Self> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.context("D3D11 device")?;
        let context = context.context("D3D11 immediate context")?;
        // The media thread shares this device later.
        let multithread: ID3D11Multithread = context.cast()?;
        let _ = unsafe { multithread.SetMultithreadProtected(true) };

        let dxgi_device: IDXGIDevice = device.cast()?;
        let adapter = unsafe { dxgi_device.GetAdapter()? };
        let factory: IDXGIFactory5 = unsafe { adapter.GetParent()? };
        let mut allow_tearing = BOOL(0);
        let query = unsafe {
            factory.CheckFeatureSupport(
                DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                &mut allow_tearing as *mut _ as *mut c_void,
                size_of::<BOOL>() as u32,
            )
        };
        let tearing = query.is_ok() && allow_tearing.as_bool();

        let mut swap_flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
        if tearing {
            swap_flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
        }
        let sd = DXGI_SWAP_CHAIN_DESC1 {
            Width: w,
            Height: h,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: swap_flags,
            ..Default::default()
        };
        let swap: IDXGISwapChain3 = unsafe {
            factory
                .CreateSwapChainForHwnd(&device, hwnd, &sd, None, None)?
                .cast()?
        };
        unsafe {
            factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER)?;
            swap.SetMaximumFrameLatency(1)?;
        }
        let latency = unsafe { swap.GetFrameLatencyWaitableObject() };
        if latency.is_invalid() {
            anyhow::bail!("frame latency waitable object missing");
        }
        let renderer = egui_directx11::Renderer::new(&device)?;
        let rtv = Some(create_rtv(&device, &swap)?);
        Ok(Self {
            egui_ctx: egui::Context::default(),
            renderer,
            rtv,
            swap,
            latency,
            size: (w, h),
            tearing,
            swap_flags,
            context,
            device,
        })
    }

    /// The waitable object that signals when a new frame may be queued.
    pub(super) fn latency_handle(&self) -> HANDLE {
        self.latency
    }

    /// Recreates the back buffer and its render target view for a new client size.
    pub(super) fn resize(&mut self, w: u32, h: u32) -> Result<()> {
        if self.size == (w, h) {
            return Ok(());
        }
        self.rtv = None;
        unsafe {
            self.context.OMSetRenderTargets(None, None);
            // DXGI_FORMAT_UNKNOWN keeps the existing format; the flags must match creation.
            self.swap.ResizeBuffers(
                0,
                w,
                h,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(self.swap_flags as i32),
            )?;
        }
        self.size = (w, h);
        self.rtv = Some(create_rtv(&self.device, &self.swap)?);
        Ok(())
    }

    /// Clears to the field colour, draws video (if requested) and egui, saves a
    /// capture if asked, then presents.
    pub(super) fn draw(
        &mut self,
        egui_out: egui_directx11::RendererOutput,
        video_rect: Option<Rect>,
        video: &mut dyn VideoLayer,
        capture: Option<&Path>,
    ) -> Result<()> {
        let rtv = self.rtv.clone().context("render target view")?;
        unsafe { self.context.ClearRenderTargetView(&rtv, &FIELD) };
        if let Some(rect) = video_rect {
            video.draw(&self.context, &rtv, rect);
        }
        self.renderer
            .render(&self.context, &rtv, &self.egui_ctx, egui_out)?;
        if let Some(path) = capture {
            if let Err(e) = self.capture(path) {
                eprintln!("capture to {} failed: {e:#}", path.display());
            }
        }
        let presented = unsafe {
            if self.tearing {
                self.swap.Present(0, DXGI_PRESENT_ALLOW_TEARING)
            } else {
                self.swap.Present(1, DXGI_PRESENT(0))
            }
        };
        presented.ok()?;
        Ok(())
    }

    /// Copies our own back buffer to a staging texture and writes it as PNG.
    /// Only the window's back buffer is read; the desktop is never captured.
    fn capture(&self, path: &Path) -> Result<()> {
        let back: ID3D11Texture2D = unsafe { self.swap.GetBuffer(0)? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { back.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))?
        };
        let staging: ID3D11Texture2D = staging.context("staging texture")?;
        unsafe { self.context.CopyResource(&staging, &back) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?
        };
        let (w, h) = (desc.Width as usize, desc.Height as usize);
        let pitch = mapped.RowPitch as usize;
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            let row = unsafe {
                std::slice::from_raw_parts((mapped.pData as *const u8).add(y * pitch), w * 4)
            };
            for x in 0..w {
                let s = &row[x * 4..x * 4 + 4];
                let d = &mut rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 255;
            }
        }
        unsafe { self.context.Unmap(&staging, 0) };
        write_png(path, w as u32, h as u32, &rgba)
    }
}

fn create_rtv(device: &ID3D11Device, swap: &IDXGISwapChain3) -> Result<ID3D11RenderTargetView> {
    let back: ID3D11Texture2D = unsafe { swap.GetBuffer(0)? };
    // No explicit desc: the view inherits the UNORM (non-sRGB) back buffer format.
    let mut rtv = None;
    unsafe { device.CreateRenderTargetView(&back, None, Some(&mut rtv))? };
    rtv.context("render target view")
}

fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    Ok(())
}
