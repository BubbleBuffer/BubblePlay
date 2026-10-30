//! The shared texture the video processor draws into for the Flutter texture bridge.
//!
//! The session owns its own D3D11 device. Flutter's engine opens the DXGI shared handle on its
//! own device (ANGLE), so the picture crosses devices without a copy through the CPU. The
//! engine does no synchronisation with the producer, so two textures alternate: the engine is
//! pointed at the one that was finished last, while the next frame is drawn into the other.
//! Every finished frame is followed by a `Flush`, which makes the writes visible to the other
//! device.

use anyhow::{Context, Result};
use windows::{
    core::Interface,
    Win32::{
        Foundation::HANDLE,
        Graphics::{
            Direct3D11::{
                ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
                D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
                D3D11_RESOURCE_MISC_SHARED, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            },
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                IDXGIResource,
            },
        },
    },
};

use crate::surface::Rect;

/// One BGRA texture with a shared handle and a render target view.
pub struct SharedSurface {
    texture: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    handle: HANDLE,
    width: u32,
    height: u32,
}

// The handle is a plain kernel handle value and the COM objects are free-threaded
// (the device has multithread protection on).
unsafe impl Send for SharedSurface {}

impl SharedSurface {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .context("create the shared video texture")?;
        let texture = texture.context("no shared video texture")?;
        let mut rtv = None;
        unsafe { device.CreateRenderTargetView(&texture, None, Some(&mut rtv)) }
            .context("create the render target view of the shared texture")?;
        let rtv = rtv.context("no render target view")?;
        let resource: IDXGIResource = texture.cast()?;
        let handle = unsafe { resource.GetSharedHandle() }.context("shared handle")?;
        Ok(Self {
            texture,
            rtv,
            handle,
            width,
            height,
        })
    }

    pub fn handle(&self) -> HANDLE {
        self.handle
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn rtv(&self) -> &ID3D11RenderTargetView {
        &self.rtv
    }

    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    pub fn viewport(&self) -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            w: self.width as f32,
            h: self.height as f32,
        }
    }
}

/// Two alternating surfaces of one size.
pub struct SharedPair {
    surfaces: [SharedSurface; 2],
    /// Index of the surface the next frame is drawn into.
    next: usize,
    /// Index of the last finished surface, if any.
    ready: Option<usize>,
}

impl SharedPair {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        Ok(Self {
            surfaces: [
                SharedSurface::new(device, width, height)?,
                SharedSurface::new(device, width, height)?,
            ],
            next: 0,
            ready: None,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        self.surfaces[0].size()
    }

    /// The surface to draw the next frame into.
    pub fn back(&self) -> &SharedSurface {
        &self.surfaces[self.next]
    }

    /// Marks the back surface as finished: flushes the device context so the other device
    /// sees the writes, then swaps.
    pub fn finish(&mut self, ctx: &ID3D11DeviceContext) {
        unsafe { ctx.Flush() };
        self.ready = Some(self.next);
        self.next ^= 1;
    }

    /// The last finished surface, or None before the first frame.
    pub fn front(&self) -> Option<&SharedSurface> {
        self.ready.map(|index| &self.surfaces[index])
    }
}
