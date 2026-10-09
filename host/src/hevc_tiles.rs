use crate::encode::{EncodedFrame, EncoderOptions, Nvenc, Preset, ResourceMode, VideoCodec};
use crate::frame_damage::DamageRect;
use anyhow::{bail, ensure, Context, Result};
use std::time::Instant;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BOX,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

pub const HEVC_TILE_DIMENSION: u32 = 128;
const HEVC_TILE_BITRATE_BPS: u32 = 1_000_000;
const HEVC_TILE_FPS: u32 = 30;

pub fn is_supported_output_rect(rect: DamageRect) -> bool {
    rect.right.checked_sub(rect.left) == Some(HEVC_TILE_DIMENSION)
        && rect.bottom.checked_sub(rect.top) == Some(HEVC_TILE_DIMENSION)
}

pub struct HevcTileEncoded {
    pub frame: EncodedFrame,
    pub copy_us: u64,
}

pub struct HevcTileEncoder {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    texture: ID3D11Texture2D,
    encoder: Nvenc,
}

impl HevcTileEncoder {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let context =
            unsafe { device.GetImmediateContext() }.context("HEVC tile GetImmediateContext")?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: HEVC_TILE_DIMENSION,
            Height: HEVC_TILE_DIMENSION,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .context("CreateTexture2D(HEVC tile target)")?;
        let texture = texture.context("CreateTexture2D(HEVC tile target) returned no texture")?;
        let mut options = EncoderOptions::production(Preset::P3);
        options.gop_seconds = 1;
        options.resource_mode = ResourceMode::Persistent;
        let encoder = Nvenc::new_with_options_native_codec(
            device.as_raw(),
            HEVC_TILE_DIMENSION,
            HEVC_TILE_DIMENSION,
            HEVC_TILE_DIMENSION,
            HEVC_TILE_DIMENSION,
            HEVC_TILE_FPS,
            HEVC_TILE_BITRATE_BPS,
            options,
            VideoCodec::Hevc,
        )
        .context("open persistent 128x128 HEVC tile encoder")?;
        Ok(Self {
            device: device.clone(),
            context,
            texture,
            encoder,
        })
    }

    pub fn is_for_device(&self, device: &ID3D11Device) -> bool {
        self.device.as_raw() == device.as_raw()
    }

    pub fn copy_and_encode(
        &mut self,
        source: &ID3D11Texture2D,
        rect: DamageRect,
    ) -> Result<HevcTileEncoded> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        validate_tile_rect(rect, desc.Width, desc.Height)?;
        ensure!(
            desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM && desc.SampleDesc.Count == 1,
            "HEVC tile source must be a single-sample BGRA texture"
        );
        let source_device = unsafe { source.GetDevice() }.context("HEVC tile source GetDevice")?;
        ensure!(
            source_device.as_raw() == self.device.as_raw(),
            "HEVC tile source is on a different D3D11 device"
        );
        let source_box = D3D11_BOX {
            left: rect.left,
            top: rect.top,
            front: 0,
            right: rect.right,
            bottom: rect.bottom,
            back: 1,
        };
        let copy_started = Instant::now();
        unsafe {
            self.context.CopySubresourceRegion(
                &self.texture,
                0,
                0,
                0,
                0,
                source,
                0,
                Some(&source_box),
            );
        }
        let copy_us = copy_started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        self.encoder.force_idr();
        let frame = self
            .encoder
            .encode_texture(self.texture.as_raw())
            .context("encode 128x128 HEVC tile IDR")?;
        if !frame.is_idr {
            bail!("HEVC tile encoder did not return an IDR");
        }
        Ok(HevcTileEncoded { frame, copy_us })
    }
}

fn validate_tile_rect(rect: DamageRect, source_width: u32, source_height: u32) -> Result<()> {
    ensure!(
        is_supported_output_rect(rect),
        "HEVC tile crop must be exactly 128x128"
    );
    ensure!(
        rect.right <= source_width && rect.bottom <= source_height,
        "HEVC tile crop exceeds the scaled encoder texture"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_supported_output_rect, validate_tile_rect, HEVC_TILE_DIMENSION};
    use crate::frame_damage::DamageRect;

    #[test]
    fn exact_tile_copy_geometry_is_bounded_to_the_scaled_texture() {
        assert_eq!(HEVC_TILE_DIMENSION, 128);
        assert!(validate_tile_rect(
            DamageRect {
                left: 64,
                top: 96,
                right: 192,
                bottom: 224,
            },
            1920,
            1080,
        )
        .is_ok());
        assert!(validate_tile_rect(
            DamageRect {
                left: 64,
                top: 96,
                right: 191,
                bottom: 224,
            },
            1920,
            1080,
        )
        .is_err());
        assert!(validate_tile_rect(
            DamageRect {
                left: 1856,
                top: 1000,
                right: 1984,
                bottom: 1128,
            },
            1920,
            1080,
        )
        .is_err());
    }

    #[test]
    fn zoom_mapped_geometry_is_a_safe_capability_bypass() {
        assert!(is_supported_output_rect(DamageRect {
            left: 64,
            top: 96,
            right: 192,
            bottom: 224,
        }));
        assert!(!is_supported_output_rect(DamageRect {
            left: 128,
            top: 192,
            right: 384,
            bottom: 448,
        }));
        assert!(!is_supported_output_rect(DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 127,
        }));
    }
}
