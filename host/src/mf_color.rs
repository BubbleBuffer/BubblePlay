//! Desktop BGRA -> NV12 for MF, matching measured current NVENC conversion.
//! Active dimensions at most 768x576 use BT.601; larger dimensions use BT.709.
//! Both outputs use studio range, from full-range capture RGB.

use crate::gpu_gate::{self, GpuGate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
}

/// Measured at both even boundaries and across retained-session reconfigure.
/// NVENC's native maximum dimensions do not affect this conversion choice.
pub fn matrix_for_nvenc(width: u32, height: u32) -> ColorMatrix {
    if width <= 768 && height <= 576 {
        ColorMatrix::Bt601
    } else {
        ColorMatrix::Bt709
    }
}
use anyhow::{ensure, Context, Result};
use std::{
    mem::ManuallyDrop,
    sync::Arc,
    time::{Duration, Instant},
};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

pub struct Nv12Converter {
    device: ID3D11Device,
    gate: Arc<GpuGate>,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    staging: Option<ID3D11Texture2D>,
    completion_query: Option<ID3D11Query>,
    width: u32,
    height: u32,
}

fn texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    staging: bool,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: if staging {
            D3D11_USAGE_STAGING
        } else {
            D3D11_USAGE_DEFAULT
        },
        BindFlags: if staging {
            0
        } else {
            (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32
        },
        CPUAccessFlags: if staging {
            D3D11_CPU_ACCESS_READ.0 as u32
        } else {
            0
        },
        MiscFlags: 0,
    };
    let mut out = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut out)) }
        .context("MF conversion texture")?;
    out.context("MF conversion texture missing")
}

impl Nv12Converter {
    pub fn new(device: &ID3D11Device, width: u32, height: u32, fps: u32) -> Result<Self> {
        let gate = gpu_gate::for_device(device);
        let gpu = gate.lock();
        let context = unsafe { device.GetImmediateContext() }?;
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext = context.cast()?;
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc) }?;
        ensure!(
            unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM) }?
                & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32
                != 0,
            "MF video processor has no BGRA input"
        );
        ensure!(
            unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12) }?
                & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32
                != 0,
            "MF video processor has no NV12 output"
        );
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }?;
        let color: ID3D11VideoContext1 = video_context
            .cast()
            .context("MF explicit color space requires ID3D11VideoContext1")?;
        unsafe {
            color.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            color.VideoProcessorSetOutputColorSpace1(
                &processor,
                match matrix_for_nvenc(width, height) {
                    ColorMatrix::Bt601 => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
                    ColorMatrix::Bt709 => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                },
            );
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        }
        let output = texture(device, width, height, DXGI_FORMAT_NV12, false)?;
        let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view = None;
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &output,
                &enumerator,
                &view_desc,
                Some(&mut output_view),
            )
        }?;
        let output_view = output_view.context("MF video processor output view missing")?;
        drop(gpu);
        Ok(Self {
            device: device.clone(),
            gate,
            context,
            video_device,
            video_context,
            enumerator,
            processor,
            output,
            output_view,
            staging: None,
            completion_query: None,
            width,
            height,
        })
    }

    pub fn convert(&self, input: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let _gpu = self.gate.lock();
        check_input(input, &self.device, self.width, self.height)?;
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut view = None;
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                input,
                &self.enumerator,
                &desc,
                Some(&mut view),
            )
        }?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(view.context("MF input view missing")?)),
            ..Default::default()
        };
        let result = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // Native structs contain ManuallyDrop COM fields, so explicitly release ownership.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.context("MF BGRA -> NV12 VideoProcessorBlt")?;
        Ok(self.output.clone())
    }

    /// Diagnostic barrier: attribute completed GPU conversion to its own stage.
    /// Production callers do not use this barrier. Polling never retains GpuGate.
    pub fn wait_for_completion(&mut self) -> Result<()> {
        {
            let _gpu = self.gate.lock();
            if self.completion_query.is_none() {
                let desc = D3D11_QUERY_DESC {
                    Query: D3D11_QUERY_EVENT,
                    MiscFlags: 0,
                };
                let mut query = None;
                unsafe { self.device.CreateQuery(&desc, Some(&mut query)) }?;
                self.completion_query =
                    Some(query.context("GPU conversion completion query missing")?);
            }
            unsafe {
                self.context.End(self.completion_query.as_ref().unwrap());
                self.context.Flush();
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let status = {
                let _gpu = self.gate.lock();
                unsafe {
                    // The binding's Result<()> erases S_FALSE. Query status
                    // directly, allowing the runtime to flush pending commands;
                    // DONOTFLUSH polling can otherwise remain pending forever.
                    (Interface::vtable(&self.context).GetData)(
                        self.context.as_raw(),
                        self.completion_query.as_ref().unwrap().as_raw(),
                        std::ptr::null_mut(),
                        0,
                        0,
                    )
                }
            };
            if status == windows::core::HRESULT(0) {
                return Ok(());
            }
            status.ok().context("GPU conversion completion GetData")?;
            ensure!(
                status == windows::core::HRESULT(1),
                "GPU conversion completion returned unexpected status {status:?}"
            );
            ensure!(
                Instant::now() < deadline,
                "GPU conversion completion timed out"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn readback(&mut self, dest: &mut Vec<u8>) -> Result<()> {
        let _gpu = self.gate.lock();
        if self.staging.is_none() {
            self.staging = Some(texture(
                &self.device,
                self.width,
                self.height,
                DXGI_FORMAT_NV12,
                true,
            )?);
        }
        let staging = self.staging.as_ref().unwrap();
        unsafe { self.context.CopyResource(staging, &self.output) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }?;
        let result = (|| {
            ensure!(
                !mapped.pData.is_null() && mapped.RowPitch >= self.width,
                "invalid NV12 staging pitch"
            );
            let w = self.width as usize;
            let rows = self.height as usize * 3 / 2;
            dest.resize(w * rows, 0);
            for row in 0..rows {
                let src = unsafe {
                    std::slice::from_raw_parts(
                        (mapped.pData as *const u8).add(row * mapped.RowPitch as usize),
                        w,
                    )
                };
                dest[row * w..(row + 1) * w].copy_from_slice(src);
            }
            Ok(())
        })();
        unsafe { self.context.Unmap(staging, 0) };
        result
    }
}

fn check_input(
    input: &ID3D11Texture2D,
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<()> {
    ensure!(
        unsafe { input.GetDevice() }?.as_raw() == device.as_raw(),
        "MF input is on another D3D11 device"
    );
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { input.GetDesc(&mut desc) };
    ensure!(
        desc.Width == width && desc.Height == height && desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM,
        "MF input must be {width}x{height} BGRA"
    );
    Ok(())
}

pub struct CpuConverter {
    device: ID3D11Device,
    gate: Arc<GpuGate>,
    context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl CpuConverter {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        let gate = gpu_gate::for_device(device);
        let gpu = gate.lock();
        let context = unsafe { device.GetImmediateContext() }?;
        let staging = texture(device, width, height, DXGI_FORMAT_B8G8R8A8_UNORM, true)?;
        drop(gpu);
        Ok(Self {
            device: device.clone(),
            gate,
            context,
            staging,
            width,
            height,
        })
    }
    pub fn convert(&self, input: &ID3D11Texture2D, dest: &mut Vec<u8>) -> Result<()> {
        let _gpu = self.gate.lock();
        check_input(input, &self.device, self.width, self.height)?;
        unsafe { self.context.CopyResource(&self.staging, input) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }?;
        let result = (|| {
            ensure!(
                !mapped.pData.is_null() && mapped.RowPitch as usize >= self.width as usize * 4,
                "invalid BGRA staging pitch"
            );
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    mapped.pData as *const u8,
                    mapped.RowPitch as usize * self.height as usize,
                )
            };
            bgra_to_nv12(
                bytes,
                mapped.RowPitch as usize,
                self.width as usize,
                self.height as usize,
                dest,
            )
        })();
        unsafe { self.context.Unmap(&self.staging, 0) };
        result
    }
}

/// Studio conversion with 16-bit coefficients. Chroma averages a 2x2 RGB box.
pub fn bgra_to_nv12(
    src: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    dst: &mut Vec<u8>,
) -> Result<()> {
    bgra_to_nv12_with_matrix(
        src,
        stride,
        width,
        height,
        dst,
        matrix_for_nvenc(width as u32, height as u32),
    )
}
fn bgra_to_nv12_with_matrix(
    src: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    dst: &mut Vec<u8>,
    matrix: ColorMatrix,
) -> Result<()> {
    ensure!(
        width > 0 && height > 0 && width.is_multiple_of(2) && height.is_multiple_of(2),
        "NV12 requires positive even dimensions"
    );
    ensure!(
        stride >= width * 4 && src.len() >= stride * height,
        "BGRA buffer is too short"
    );
    dst.resize(width * height * 3 / 2, 0);
    let (yc, uc, vc) = match matrix {
        ColorMatrix::Bt601 => (
            [16829, 33039, 6416],
            [-9714, -19071, 28784],
            [28784, -24103, -4681],
        ),
        ColorMatrix::Bt709 => (
            [11966, 40254, 4064],
            [-6596, -22189, 28784],
            [28784, -26145, -2639],
        ),
    };
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let (mut r, mut g, mut b) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = (y + dy) * stride + (x + dx) * 4;
                    let (pb, pg, pr) = (src[p] as i32, src[p + 1] as i32, src[p + 2] as i32);
                    dst[(y + dy) * width + x + dx] = (((yc[0] * pr
                        + yc[1] * pg
                        + yc[2] * pb
                        + 32768)
                        >> 16)
                        + 16)
                        .clamp(16, 235) as u8;
                    r += pr;
                    g += pg;
                    b += pb;
                }
            }
            let (r, g, b) = ((r + 2) / 4, (g + 2) / 4, (b + 2) / 4);
            let uv = width * height + (y / 2) * width + x;
            dst[uv] =
                (((uc[0] * r + uc[1] * g + uc[2] * b + 32768) >> 16) + 128).clamp(16, 240) as u8;
            dst[uv + 1] =
                (((vc[0] * r + vc[1] * g + vc[2] * b + 32768) >> 16) + 128).clamp(16, 240) as u8;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn studio_black_white_and_bgra_channel_order() {
        let mut out = Vec::new();
        for (bgra, expected) in [
            ([0, 0, 0, 255], [16, 128, 128]),
            ([255, 255, 255, 255], [235, 128, 128]),
            ([0, 0, 255, 255], [81, 90, 240]),
            ([255, 0, 0, 255], [41, 240, 110]),
        ] {
            bgra_to_nv12(&bgra.repeat(4), 8, 2, 2, &mut out).unwrap();
            assert_eq!(&out[..4], &[expected[0]; 4]);
            assert_eq!(&out[4..], &expected[1..]);
        }
    }
    #[test]
    fn padded_bgra_and_invalid_dimensions() {
        let mut out = Vec::new();
        let src = [vec![255; 8], vec![0; 8], vec![255; 8], vec![0; 8]].concat();
        bgra_to_nv12(&src, 16, 2, 2, &mut out).unwrap();
        assert_eq!(&out[..4], &[235; 4]);
        assert!(bgra_to_nv12(&src, 16, 3, 2, &mut out).is_err());
        assert!(bgra_to_nv12(&src[..8], 16, 2, 2, &mut out).is_err());
    }
    #[test]
    fn measured_nvenc_even_boundaries_select_matrix_from_active_dimensions() {
        assert_eq!(matrix_for_nvenc(768, 576), ColorMatrix::Bt601);
        assert_eq!(matrix_for_nvenc(770, 576), ColorMatrix::Bt709);
        assert_eq!(matrix_for_nvenc(640, 578), ColorMatrix::Bt709);
        assert_eq!(matrix_for_nvenc(848, 480), ColorMatrix::Bt709);
        assert_eq!(matrix_for_nvenc(960, 540), ColorMatrix::Bt709);
    }
    #[test]
    fn bt709_primary_values_match_measured_nvenc_studio_conversion() {
        let mut out = Vec::new();
        for (bgra, expected) in [
            ([0, 0, 255, 255], [63, 102, 240]),
            ([0, 255, 0, 255], [173, 42, 26]),
            ([255, 0, 0, 255], [32, 240, 118]),
            ([255, 255, 255, 255], [235, 128, 128]),
            ([0, 0, 0, 255], [16, 128, 128]),
        ] {
            bgra_to_nv12_with_matrix(&bgra.repeat(4), 8, 2, 2, &mut out, ColorMatrix::Bt709)
                .unwrap();
            assert_eq!(&out[..4], &[expected[0]; 4]);
            assert_eq!(&out[4..], &expected[1..]);
        }
    }
}
