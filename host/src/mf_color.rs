//! Desktop BGRA -> NV12 for MF, matching measured current NVENC conversion.
//! Active dimensions at most 768x576 use BT.601; larger dimensions use BT.709.
//! Both outputs use studio range, from full-range capture RGB.

use crate::frame_damage::DamageRect;
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
    ops::Range,
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

/// Options for the CPU BGRA -> NV12 path. [`CpuConversionOptions::from_env`] reads
/// `BP_CPU_SIMD` (default 1), `BP_CPU_DAMAGE` (default 1) and `BP_CPU_CONVERT_THREADS`
/// (1 or 2, default 1; any other value means 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuConversionOptions {
    /// Use AVX2 kernels when the CPU has them; otherwise the scalar kernels run.
    pub simd: bool,
    /// Patch only the damaged output rectangles when `dest` still holds the previous output.
    pub damage: bool,
    /// Worker threads that split output rows: 1 or 2.
    pub threads: usize,
}

impl Default for CpuConversionOptions {
    fn default() -> Self {
        Self {
            simd: true,
            damage: true,
            threads: 1,
        }
    }
}

impl CpuConversionOptions {
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var("BP_CPU_SIMD").ok().as_deref(),
            std::env::var("BP_CPU_DAMAGE").ok().as_deref(),
            std::env::var("BP_CPU_CONVERT_THREADS").ok().as_deref(),
        )
    }

    fn from_values(simd: Option<&str>, damage: Option<&str>, threads: Option<&str>) -> Self {
        let defaults = Self::default();
        Self {
            simd: parse_flag(simd, defaults.simd),
            damage: parse_flag(damage, defaults.damage),
            threads: match threads.map(str::trim).map(str::parse::<usize>) {
                Some(Ok(2)) => 2,
                _ => 1,
            },
        }
    }

    /// The thread count clamped to the supported 1..=2 range.
    pub fn normalized(self) -> Self {
        Self {
            threads: self.threads.clamp(1, 2),
            ..self
        }
    }
}

fn parse_flag(value: Option<&str>, default: bool) -> bool {
    match value.map(str::trim) {
        Some("0" | "false" | "off") => false,
        Some("1" | "true" | "on") => true,
        _ => default,
    }
}

/// Wall time of one `convert_frame` call. `readback_us` covers the GPU copy, Map and
/// Unmap, with GpuGate held only around the D3D calls; `conversion_us` is the CPU work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuConversionTimings {
    pub readback_us: u64,
    pub conversion_us: u64,
}

/// The output that `dest` last received from this converter, identified by the buffer's
/// address and the output dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Retained {
    ptr: usize,
    output_width: usize,
    output_height: usize,
}

pub struct CpuConverter {
    device: ID3D11Device,
    gate: Arc<GpuGate>,
    context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
    options: CpuConversionOptions,
    retained: Option<Retained>,
}

impl CpuConverter {
    /// `width` and `height` are the source (capture) dimensions of every input texture.
    /// Options start from [`CpuConversionOptions::from_env`].
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
            options: CpuConversionOptions::from_env(),
            retained: None,
        })
    }

    pub fn options(&self) -> CpuConversionOptions {
        self.options
    }

    pub fn set_options(&mut self, options: CpuConversionOptions) {
        self.options = options.normalized();
    }

    /// Full-frame conversion at source size, the same contract as before.
    pub fn convert(&mut self, input: &ID3D11Texture2D, dest: &mut Vec<u8>) -> Result<()> {
        let (width, height) = (self.width, self.height);
        self.convert_frame(input, width, height, None, dest)
            .map(|_| ())
    }

    /// Converts the staged BGRA frame to NV12 at `output_width` x `output_height`.
    ///
    /// `damage` is in source coordinates and lists every region that changed since the
    /// previous call on this converter, including both ends of any move. `None`, invalid
    /// rectangles, a different output size, or a `dest` that is not the buffer this converter
    /// last wrote all cause a full refresh. `Some(&[])` means nothing changed and nothing is
    /// read back. `dest` must not be modified between calls when damage is used.
    pub fn convert_frame(
        &mut self,
        input: &ID3D11Texture2D,
        output_width: u32,
        output_height: u32,
        damage: Option<&[DamageRect]>,
        dest: &mut Vec<u8>,
    ) -> Result<CpuConversionTimings> {
        let plan = Plan::new(
            self.width as usize,
            self.height as usize,
            output_width as usize,
            output_height as usize,
        )?;
        let options = self.options;
        let patch = options.damage && can_patch(self.retained, dest, &plan);
        let rects = if patch {
            incremental_rects(&plan, damage)
        } else {
            None
        };
        if rects.as_ref().is_some_and(|rects| rects.is_empty()) {
            return Ok(CpuConversionTimings::default());
        }
        // The retained output stays invalid until this conversion completes.
        self.retained = None;
        dest.resize(plan.output_len(), 0);

        let started = Instant::now();
        let gpu = self.gate.lock();
        check_input(input, &self.device, self.width, self.height)?;
        unsafe { self.context.CopyResource(&self.staging, input) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }?;
        drop(gpu);
        let mapped_at = Instant::now();

        // The gate is released while the CPU reads the mapping. Unmap runs below even
        // when the conversion fails.
        let (src_width, src_height) = (plan.sw, plan.sh);
        let converted = (|| -> Result<Duration> {
            ensure!(
                !mapped.pData.is_null() && mapped.RowPitch as usize >= src_width * 4,
                "invalid BGRA staging pitch"
            );
            let pitch = mapped.RowPitch as usize;
            // SAFETY: the staging texture stays mapped until the Unmap below, and nothing else
            // reads or writes it while this slice exists.
            let bytes = unsafe {
                std::slice::from_raw_parts(mapped.pData as *const u8, pitch * src_height)
            };
            let conversion_started = Instant::now();
            convert_into(
                &plan,
                bytes,
                pitch,
                options.normalized(),
                rects.as_deref(),
                dest.as_mut_slice(),
            );
            Ok(conversion_started.elapsed())
        })();

        let unmap_started = Instant::now();
        let gpu = self.gate.lock();
        unsafe { self.context.Unmap(&self.staging, 0) };
        drop(gpu);
        let unmapped = unmap_started.elapsed();
        let conversion = converted?;
        self.retained = Some(Retained {
            ptr: dest.as_ptr() as usize,
            output_width: plan.ow,
            output_height: plan.oh,
        });
        Ok(CpuConversionTimings {
            readback_us: (mapped_at - started + unmapped).as_micros() as u64,
            conversion_us: conversion.as_micros() as u64,
        })
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

/// Fused BGRA -> NV12 at any even output size. Output pixel (ox, oy) samples source pixel
/// (ox * sw / ow, oy * sh / oh) by nearest neighbour; chroma averages the 2x2 output block.
/// Output dimensions choose the colour matrix, as `matrix_for_nvenc` does for the GPU path.
/// `dest` is resized to the NV12 length and fully rewritten.
#[allow(clippy::too_many_arguments)]
pub fn bgra_to_nv12_scaled(
    src: &[u8],
    stride: usize,
    src_width: usize,
    src_height: usize,
    output_width: usize,
    output_height: usize,
    options: CpuConversionOptions,
    dest: &mut Vec<u8>,
) -> Result<()> {
    let plan = Plan::new(src_width, src_height, output_width, output_height)?;
    check_source(src, stride, src_width, src_height)?;
    dest.resize(plan.output_len(), 0);
    convert_into(
        &plan,
        src,
        stride,
        options.normalized(),
        None,
        dest.as_mut_slice(),
    );
    Ok(())
}

fn check_source(src: &[u8], stride: usize, width: usize, height: usize) -> Result<()> {
    ensure!(
        stride >= width * 4 && src.len() >= stride * height,
        "BGRA buffer is too short"
    );
    Ok(())
}

/// Rows per worker below which a rectangle is not split across threads.
const MIN_PAIRS_PER_WORKER: usize = 16;

/// Output rectangle in NV12 luma coordinates. Edges are even, so every chroma sample it
/// touches is recomputed from its whole 2x2 block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutRect {
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
}

/// Studio coefficients with 16-bit precision, shared with the legacy `bgra_to_nv12`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Coef {
    y: [i32; 3],
    u: [i32; 3],
    v: [i32; 3],
}

fn coef(matrix: ColorMatrix) -> Coef {
    match matrix {
        ColorMatrix::Bt601 => Coef {
            y: [16829, 33039, 6416],
            u: [-9714, -19071, 28784],
            v: [28784, -24103, -4681],
        },
        ColorMatrix::Bt709 => Coef {
            y: [11966, 40254, 4064],
            u: [-6596, -22189, 28784],
            v: [28784, -26145, -2639],
        },
    }
}

struct Plan {
    sw: usize,
    sh: usize,
    ow: usize,
    oh: usize,
    identity_x: bool,
    identity_y: bool,
    /// Source column for each output column.
    col_map: Vec<u32>,
    coef: Coef,
}

impl Plan {
    fn new(
        src_width: usize,
        src_height: usize,
        output_width: usize,
        output_height: usize,
    ) -> Result<Self> {
        ensure!(
            src_width > 0 && src_height > 0 && src_width <= i32::MAX as usize,
            "BGRA source must be non-empty"
        );
        ensure!(
            output_width > 0
                && output_height > 0
                && output_width.is_multiple_of(2)
                && output_height.is_multiple_of(2),
            "NV12 requires positive even dimensions"
        );
        let (sw, sh, ow, oh) = (src_width, src_height, output_width, output_height);
        Ok(Self {
            sw,
            sh,
            ow,
            oh,
            identity_x: sw == ow,
            identity_y: sh == oh,
            col_map: (0..ow).map(|ox| (ox * sw / ow) as u32).collect(),
            coef: coef(matrix_for_nvenc(ow as u32, oh as u32)),
        })
    }

    fn output_len(&self) -> usize {
        self.ow * self.oh * 3 / 2
    }

    /// Returns the BGRA pixels of output row `oy`, valid at least in columns `x0..x1`. Without
    /// horizontal scaling this is the source row itself; otherwise the sampled columns are
    /// gathered into `scratch`.
    fn source_row<'a>(
        &self,
        src: &'a [u8],
        stride: usize,
        oy: usize,
        span: Range<usize>,
        scratch: &'a mut [u8],
        simd: bool,
    ) -> &'a [u8] {
        let (x0, x1) = (span.start, span.end);
        let sy = if self.identity_y {
            oy
        } else {
            oy * self.sh / self.oh
        };
        let row = &src[sy * stride..];
        if self.identity_x {
            return &row[..self.sw * 4];
        }
        let mut start = x0;
        #[cfg(target_arch = "x86_64")]
        if use_avx2(simd) {
            // The map contains non-negative pixel indices within this row;
            // each gather reads exactly one BGRA pixel from mapped staging.
            unsafe {
                start = avx2::gather(row, &self.col_map, scratch, x0, x1);
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = simd;
        for ox in start..x1 {
            let sx = self.col_map[ox] as usize * 4;
            scratch[ox * 4..ox * 4 + 4].copy_from_slice(&row[sx..sx + 4]);
        }
        scratch
    }
}

/// Output span `start..end` touched by source span `lo..hi`, rounded out to even edges.
/// `None` when no output sample reads the source span.
fn output_span(lo: usize, hi: usize, src: usize, dst: usize) -> Option<(usize, usize)> {
    // Output o samples source floor(o * src / dst), so o is affected iff
    // ceil(lo * dst / src) <= o < ceil(hi * dst / src).
    let start = (lo * dst).div_ceil(src);
    let end = (hi * dst).div_ceil(src).min(dst);
    if start >= end {
        return None;
    }
    Some((start & !1, ((end + 1) & !1).min(dst)))
}

/// Maps source damage to aligned output rectangles. `None` means full refresh: no damage
/// report, or any rectangle that is empty or outside the source. `Some(vec![])` means no
/// output sample changed.
fn incremental_rects(plan: &Plan, damage: Option<&[DamageRect]>) -> Option<Vec<OutRect>> {
    let damage = damage?;
    let mut rects = Vec::with_capacity(damage.len());
    for rect in damage {
        let (left, top) = (rect.left as usize, rect.top as usize);
        let (right, bottom) = (rect.right as usize, rect.bottom as usize);
        if left >= right || top >= bottom || right > plan.sw || bottom > plan.sh {
            return None;
        }
        if let (Some((x0, x1)), Some((y0, y1))) = (
            output_span(left, right, plan.sw, plan.ow),
            output_span(top, bottom, plan.sh, plan.oh),
        ) {
            rects.push(OutRect { x0, x1, y0, y1 });
        }
    }
    Some(rects)
}

/// Whether `dest` still holds the output this converter last wrote for `plan`.
fn can_patch(retained: Option<Retained>, dest: &[u8], plan: &Plan) -> bool {
    retained.is_some_and(|r| {
        r.ptr == dest.as_ptr() as usize
            && r.output_width == plan.ow
            && r.output_height == plan.oh
            && dest.len() == plan.output_len()
    })
}

/// Raw view of the NV12 output, shared by the row workers. Each worker writes only
/// its own row pairs, so the ranges never overlap.
#[derive(Clone, Copy)]
struct OutPtr {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the pointer addresses one buffer that outlives every worker. Workers write disjoint
// ranges, guaranteed by `convert_into`.
unsafe impl Send for OutPtr {}
unsafe impl Sync for OutPtr {}

impl OutPtr {
    /// SAFETY: the caller must not use the returned slice together with another slice that
    /// covers any of the same bytes.
    unsafe fn range_mut<'a>(self, start: usize, len: usize) -> &'a mut [u8] {
        assert!(start + len <= self.len);
        std::slice::from_raw_parts_mut(self.ptr.add(start), len)
    }
}

fn convert_into(
    plan: &Plan,
    src: &[u8],
    stride: usize,
    options: CpuConversionOptions,
    rects: Option<&[OutRect]>,
    dst: &mut [u8],
) {
    assert_eq!(dst.len(), plan.output_len());
    let full = [OutRect {
        x0: 0,
        x1: plan.ow,
        y0: 0,
        y1: plan.oh,
    }];
    let rects: &[OutRect] = match rects {
        Some(rects) => rects,
        None => &full,
    };
    let threads = options.threads.clamp(1, 2);
    let simd = options.simd;
    let out = OutPtr {
        ptr: dst.as_mut_ptr(),
        len: dst.len(),
    };
    for rect in rects {
        let rect = *rect;
        let pairs = rect.y0 / 2..rect.y1 / 2;
        if threads == 2 && pairs.len() >= 2 * MIN_PAIRS_PER_WORKER {
            let mid = pairs.start + pairs.len() / 2;
            let (first, second) = (pairs.start..mid, mid..pairs.end);
            std::thread::scope(|scope| {
                scope.spawn(move || convert_pairs(plan, src, stride, out, rect, first, simd));
                convert_pairs(plan, src, stride, out, rect, second, simd);
            });
        } else {
            convert_pairs(plan, src, stride, out, rect, pairs, simd);
        }
    }
}

/// Converts output row pairs `pairs` (each pair is two luma rows and one chroma row) over the
/// columns of `rect`.
fn convert_pairs(
    plan: &Plan,
    src: &[u8],
    stride: usize,
    out: OutPtr,
    rect: OutRect,
    pairs: Range<usize>,
    simd: bool,
) {
    let width = rect.x1 - rect.x0;
    let scratch_len = if plan.identity_x { 0 } else { plan.ow * 4 };
    let mut scratch_a = vec![0u8; scratch_len];
    let mut scratch_b = vec![0u8; scratch_len];
    for pair in pairs {
        let oy = 2 * pair;
        let row_a = plan.source_row(src, stride, oy, rect.x0..rect.x1, &mut scratch_a, simd);
        let row_b = plan.source_row(src, stride, oy + 1, rect.x0..rect.x1, &mut scratch_b, simd);
        // SAFETY: this pair owns luma rows oy and oy+1 and chroma row `pair`, restricted to
        // this rect's columns. Pair ranges handed to different workers never overlap, and
        // each range below is dropped at the end of its statement.
        unsafe {
            luma_row(
                row_a,
                out.range_mut(oy * plan.ow + rect.x0, width),
                rect.x0,
                &plan.coef,
                simd,
            );
            luma_row(
                row_b,
                out.range_mut((oy + 1) * plan.ow + rect.x0, width),
                rect.x0,
                &plan.coef,
                simd,
            );
            chroma_row(
                row_a,
                row_b,
                out.range_mut(plan.ow * plan.oh + pair * plan.ow + rect.x0, width),
                rect.x0,
                &plan.coef,
                simd,
            );
        }
    }
}

fn luma_value(r: i32, g: i32, b: i32, k: &[i32; 3]) -> u8 {
    ((((k[0] * r + k[1] * g + k[2] * b + 32768) >> 16) + 16).clamp(16, 235)) as u8
}

fn chroma_value(r: i32, g: i32, b: i32, k: &[i32; 3]) -> u8 {
    ((((k[0] * r + k[1] * g + k[2] * b + 32768) >> 16) + 128).clamp(16, 240)) as u8
}

/// Luma for columns `x0..x0 + dst.len()`. `src` is a full BGRA row.
fn luma_scalar(src: &[u8], dst: &mut [u8], x0: usize, c: &Coef) {
    for (i, out) in dst.iter_mut().enumerate() {
        let p = (x0 + i) * 4;
        *out = luma_value(src[p + 2] as i32, src[p + 1] as i32, src[p] as i32, &c.y);
    }
}

/// Interleaved UV for the chroma samples covering `x0..x0 + dst.len()` (even bounds).
/// `a` and `b` are the two luma rows of the pair.
fn chroma_scalar(a: &[u8], b: &[u8], dst: &mut [u8], x0: usize, c: &Coef) {
    for (j, pair) in dst.chunks_exact_mut(2).enumerate() {
        let p = (x0 + 2 * j) * 4;
        let (mut r, mut g, mut blue) = (0, 0, 0);
        for row in [a, b] {
            for q in [p, p + 4] {
                blue += row[q] as i32;
                g += row[q + 1] as i32;
                r += row[q + 2] as i32;
            }
        }
        let (r, g, blue) = ((r + 2) / 4, (g + 2) / 4, (blue + 2) / 4);
        pair[0] = chroma_value(r, g, blue, &c.u);
        pair[1] = chroma_value(r, g, blue, &c.v);
    }
}

#[cfg(target_arch = "x86_64")]
fn use_avx2(simd: bool) -> bool {
    simd && is_x86_feature_detected!("avx2")
}

#[cfg(not(target_arch = "x86_64"))]
fn use_avx2(_simd: bool) -> bool {
    false
}

fn luma_row(src: &[u8], dst: &mut [u8], x0: usize, c: &Coef, simd: bool) {
    assert!(src.len() >= (x0 + dst.len()) * 4);
    if use_avx2(simd) {
        // SAFETY: AVX2 was detected at runtime. The assert bounds every 32-byte load.
        unsafe { avx2::luma(src, dst, x0, c) }
    } else {
        luma_scalar(src, dst, x0, c)
    }
}

fn chroma_row(a: &[u8], b: &[u8], dst: &mut [u8], x0: usize, c: &Coef, simd: bool) {
    assert!(dst.len().is_multiple_of(2));
    assert!(a.len() >= (x0 + dst.len()) * 4 && b.len() >= (x0 + dst.len()) * 4);
    if use_avx2(simd) {
        // SAFETY: AVX2 was detected at runtime. The asserts bound every 32-byte load.
        unsafe { avx2::chroma(a, b, dst, x0, c) }
    } else {
        chroma_scalar(a, b, dst, x0, c)
    }
}

#[cfg(target_arch = "x86_64")]
mod avx2 {
    //! AVX2 kernels, bit-exact with the scalar kernels in the parent module. Each kernel
    //! finishes its tail with the scalar code.
    use super::{chroma_scalar, luma_scalar, Coef};
    use std::arch::x86_64::*;

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn gather(
        row: &[u8],
        map: &[u32],
        out: &mut [u8],
        start: usize,
        end: usize,
    ) -> usize {
        let mut x = start;
        while x + 8 <= end {
            let indices = _mm256_loadu_si256(map.as_ptr().add(x).cast());
            let bgra = _mm256_i32gather_epi32::<4>(row.as_ptr().cast(), indices);
            _mm256_storeu_si256(out.as_mut_ptr().add(x * 4).cast(), bgra);
            x += 8;
        }
        x
    }

    /// Shuffle mask that places byte `offset` of each BGRA pixel into the low byte of
    /// dword `k` within each 128-bit lane. Other bytes are zeroed. Lane 0 holds pixels
    /// 0..4 and lane 1 holds pixels 4..8, so the eight dwords are pixels 0..8 in order.
    const fn channel_mask(offset: usize) -> [i8; 32] {
        let mut mask = [-128i8; 32];
        let mut k = 0usize;
        while k < 4 {
            mask[4 * k] = (4 * k + offset) as i8;
            mask[16 + 4 * k] = (4 * k + offset) as i8;
            k += 1;
        }
        mask
    }

    const BLUE: [i8; 32] = channel_mask(0);
    const GREEN: [i8; 32] = channel_mask(1);
    const RED: [i8; 32] = channel_mask(2);

    #[target_feature(enable = "avx2")]
    unsafe fn channel(v: __m256i, mask: &[i8; 32]) -> __m256i {
        _mm256_shuffle_epi8(v, _mm256_loadu_si256(mask.as_ptr() as *const __m256i))
    }

    /// Studio value `clamp(((k·rgb + 32768) >> 16) + offset, 16, max)` in eight lanes.
    #[target_feature(enable = "avx2")]
    unsafe fn studio(
        r: __m256i,
        g: __m256i,
        b: __m256i,
        k: &[i32; 3],
        offset: i32,
        max: i32,
    ) -> __m256i {
        let sum = _mm256_add_epi32(
            _mm256_add_epi32(
                _mm256_mullo_epi32(r, _mm256_set1_epi32(k[0])),
                _mm256_mullo_epi32(g, _mm256_set1_epi32(k[1])),
            ),
            _mm256_add_epi32(
                _mm256_mullo_epi32(b, _mm256_set1_epi32(k[2])),
                _mm256_set1_epi32(32768),
            ),
        );
        let value = _mm256_add_epi32(_mm256_srai_epi32::<16>(sum), _mm256_set1_epi32(offset));
        _mm256_min_epi32(
            _mm256_max_epi32(value, _mm256_set1_epi32(16)),
            _mm256_set1_epi32(max),
        )
    }

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn luma(src: &[u8], dst: &mut [u8], x0: usize, c: &Coef) {
        let mut i = 0usize;
        while i + 8 <= dst.len() {
            let v = _mm256_loadu_si256(src.as_ptr().add((x0 + i) * 4) as *const __m256i);
            let y = studio(
                channel(v, &RED),
                channel(v, &GREEN),
                channel(v, &BLUE),
                &c.y,
                16,
                235,
            );
            // Dwords 0..8 hold pixels in order; pack their low bytes into 8 bytes.
            let packed = _mm256_permute4x64_epi64::<0x08>(_mm256_packus_epi32(y, y));
            let low = _mm256_castsi256_si128(packed);
            _mm_storel_epi64(
                dst.as_mut_ptr().add(i) as *mut __m128i,
                _mm_packus_epi16(low, low),
            );
            i += 8;
        }
        luma_scalar(src, &mut dst[i..], x0 + i, c);
    }

    /// Sums one channel over each 2x2 block: pixels 0..16 of rows `a` and `b` give
    /// eight sums, returned in order.
    #[target_feature(enable = "avx2")]
    unsafe fn pair_sums(
        a0: __m256i,
        a1: __m256i,
        b0: __m256i,
        b1: __m256i,
        mask: &[i8; 32],
    ) -> __m256i {
        let s0 = _mm256_add_epi32(channel(a0, mask), channel(b0, mask));
        let s1 = _mm256_add_epi32(channel(a1, mask), channel(b1, mask));
        // hadd yields qwords [m0 m1 | m4 m5 | m2 m3 | m6 m7]; 0xD8 reorders to pair order.
        _mm256_permute4x64_epi64::<0xD8>(_mm256_hadd_epi32(s0, s1))
    }

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn chroma(a: &[u8], b: &[u8], dst: &mut [u8], x0: usize, c: &Coef) {
        let mut j = 0usize;
        while j + 8 <= dst.len() / 2 {
            // Eight chroma samples cover sixteen pixels, which is 64 bytes per row.
            let p = (x0 + 2 * j) * 4;
            let a0 = _mm256_loadu_si256(a.as_ptr().add(p) as *const __m256i);
            let a1 = _mm256_loadu_si256(a.as_ptr().add(p + 32) as *const __m256i);
            let b0 = _mm256_loadu_si256(b.as_ptr().add(p) as *const __m256i);
            let b1 = _mm256_loadu_si256(b.as_ptr().add(p + 32) as *const __m256i);
            let two = _mm256_set1_epi32(2);
            // (sum + 2) / 4 on non-negative sums is a shift.
            let r = _mm256_srai_epi32::<2>(_mm256_add_epi32(pair_sums(a0, a1, b0, b1, &RED), two));
            let g =
                _mm256_srai_epi32::<2>(_mm256_add_epi32(pair_sums(a0, a1, b0, b1, &GREEN), two));
            let blue =
                _mm256_srai_epi32::<2>(_mm256_add_epi32(pair_sums(a0, a1, b0, b1, &BLUE), two));
            let u = studio(r, g, blue, &c.u, 128, 240);
            let v = studio(r, g, blue, &c.v, 128, 240);
            // Interleave U and V as 16-bit words, then pack to bytes in sample order.
            let uv = _mm256_unpacklo_epi16(_mm256_packus_epi32(u, u), _mm256_packus_epi32(v, v));
            let bytes = _mm256_permute4x64_epi64::<0x08>(_mm256_packus_epi16(uv, uv));
            _mm_storeu_si128(
                dst.as_mut_ptr().add(2 * j) as *mut __m128i,
                _mm256_castsi256_si128(bytes),
            );
            j += 8;
        }
        chroma_scalar(a, b, &mut dst[2 * j..], x0 + 2 * j, c);
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod avx2 {
    use super::{chroma_scalar, luma_scalar, Coef};

    pub(super) unsafe fn luma(src: &[u8], dst: &mut [u8], x0: usize, c: &Coef) {
        luma_scalar(src, dst, x0, c)
    }

    pub(super) unsafe fn chroma(a: &[u8], b: &[u8], dst: &mut [u8], x0: usize, c: &Coef) {
        chroma_scalar(a, b, dst, x0, c)
    }
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

    /// Source and output sizes: odd sources, even outputs, up- and downscale, both matrices.
    const SCALED: [(usize, usize, usize, usize); 6] = [
        (1001, 563, 640, 360),
        (333, 221, 166, 110),
        (1, 1, 2, 2),
        (33, 17, 64, 36),
        (1920, 1080, 1280, 720),
        (777, 555, 256, 190),
    ];

    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }

        fn fill(&mut self, buf: &mut [u8]) {
            for chunk in buf.chunks_mut(8) {
                let value = self.next_u64().to_le_bytes();
                chunk.copy_from_slice(&value[..chunk.len()]);
            }
        }
    }

    /// Straightforward fused oracle: per output sample, nearest source pixel, then the
    /// shared arithmetic. Shares no sampling code with the production path.
    fn reference_fused(
        src: &[u8],
        stride: usize,
        sw: usize,
        sh: usize,
        ow: usize,
        oh: usize,
    ) -> Vec<u8> {
        let c = coef(matrix_for_nvenc(ow as u32, oh as u32));
        let at = |ox: usize, oy: usize| {
            let p = (oy * sh / oh) * stride + (ox * sw / ow) * 4;
            (src[p + 2] as i32, src[p + 1] as i32, src[p] as i32)
        };
        let mut out = vec![0u8; ow * oh * 3 / 2];
        for oy in 0..oh {
            for ox in 0..ow {
                let (r, g, b) = at(ox, oy);
                out[oy * ow + ox] = luma_value(r, g, b, &c.y);
            }
        }
        for cy in 0..oh / 2 {
            for cx in 0..ow / 2 {
                let (mut r, mut g, mut b) = (0, 0, 0);
                for dy in 0..2 {
                    for dx in 0..2 {
                        let (pr, pg, pb) = at(2 * cx + dx, 2 * cy + dy);
                        r += pr;
                        g += pg;
                        b += pb;
                    }
                }
                let (r, g, b) = ((r + 2) / 4, (g + 2) / 4, (b + 2) / 4);
                let uv = ow * oh + cy * ow + 2 * cx;
                out[uv] = chroma_value(r, g, b, &c.u);
                out[uv + 1] = chroma_value(r, g, b, &c.v);
            }
        }
        out
    }

    fn assert_same(label: &str, got: &[u8], want: &[u8]) {
        assert_eq!(got.len(), want.len(), "{label}: length");
        if let Some(i) = got.iter().zip(want).position(|(a, b)| a != b) {
            panic!(
                "{label}: first mismatch at byte {i}: got {} want {}",
                got[i], want[i]
            );
        }
    }

    fn tight_copy(src: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
        (0..height)
            .flat_map(|y| src[y * stride..y * stride + width * 4].iter().copied())
            .collect()
    }

    fn random_rect(rng: &mut Rng, width: usize, height: usize) -> DamageRect {
        let rw = 1 + rng.below((width / 6).max(1));
        let rh = 1 + rng.below((height / 6).max(1));
        let left = rng.below(width - rw + 1);
        let top = rng.below(height - rh + 1);
        DamageRect {
            left: left as u32,
            top: top as u32,
            right: (left + rw) as u32,
            bottom: (top + rh) as u32,
        }
    }

    fn fill_rect(rng: &mut Rng, frame: &mut [u8], stride: usize, rect: DamageRect) {
        for y in rect.top as usize..rect.bottom as usize {
            let start = y * stride + rect.left as usize * 4;
            let end = y * stride + rect.right as usize * 4;
            rng.fill(&mut frame[start..end]);
        }
    }

    fn union_rect(rects: &[DamageRect]) -> DamageRect {
        DamageRect {
            left: rects.iter().map(|r| r.left).min().unwrap(),
            top: rects.iter().map(|r| r.top).min().unwrap(),
            right: rects.iter().map(|r| r.right).max().unwrap(),
            bottom: rects.iter().map(|r| r.bottom).max().unwrap(),
        }
    }

    #[test]
    fn fused_full_size_matches_legacy_baseline() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for (w, h) in [(640, 360), (768, 576), (770, 578), (1280, 720), (1366, 768)] {
            let stride = w * 4 + 28;
            let mut src = vec![0u8; stride * h];
            rng.fill(&mut src);
            let mut legacy = Vec::new();
            bgra_to_nv12(&src, stride, w, h, &mut legacy).unwrap();
            for simd in [false, true] {
                for threads in [1, 2] {
                    let options = CpuConversionOptions {
                        simd,
                        damage: true,
                        threads,
                    };
                    let mut fused = Vec::new();
                    bgra_to_nv12_scaled(&src, stride, w, h, w, h, options, &mut fused).unwrap();
                    assert_same(&format!("{w}x{h} simd={simd} t={threads}"), &fused, &legacy);
                }
            }
            assert_same(
                &format!("{w}x{h} reference"),
                &reference_fused(&src, stride, w, h, w, h),
                &legacy,
            );
        }
    }

    #[test]
    fn scaled_paths_match_fused_reference_with_padded_and_tight_rows() {
        let mut rng = Rng(0x0123_4567_89AB_CDEF);
        for (sw, sh, ow, oh) in SCALED {
            let stride = sw * 4 + 12;
            let mut src = vec![0u8; stride * sh];
            rng.fill(&mut src);
            let want = reference_fused(&src, stride, sw, sh, ow, oh);
            for simd in [false, true] {
                for threads in [1, 2] {
                    let options = CpuConversionOptions {
                        simd,
                        damage: true,
                        threads,
                    };
                    let mut got = Vec::new();
                    bgra_to_nv12_scaled(&src, stride, sw, sh, ow, oh, options, &mut got).unwrap();
                    assert_same(
                        &format!("{sw}x{sh}->{ow}x{oh} simd={simd} t={threads}"),
                        &got,
                        &want,
                    );
                }
            }
            let tight = tight_copy(&src, stride, sw, sh);
            let mut got = Vec::new();
            bgra_to_nv12_scaled(
                &tight,
                sw * 4,
                sw,
                sh,
                ow,
                oh,
                CpuConversionOptions::default(),
                &mut got,
            )
            .unwrap();
            assert_same(&format!("{sw}x{sh}->{ow}x{oh} tight"), &got, &want);
        }
    }

    #[test]
    fn damage_patches_on_persistent_buffer_match_full_reference() {
        let mut rng = Rng(0x5EED_0001);
        let mut cases = SCALED.to_vec();
        cases.push((640, 360, 640, 360));
        for (sw, sh, ow, oh) in cases {
            let stride = sw * 4 + 16;
            let plan = Plan::new(sw, sh, ow, oh).unwrap();
            let mut frame = vec![0u8; stride * sh];
            rng.fill(&mut frame);
            let mut dest = Vec::new();
            bgra_to_nv12_scaled(
                &frame,
                stride,
                sw,
                sh,
                ow,
                oh,
                CpuConversionOptions::default(),
                &mut dest,
            )
            .unwrap();
            for iter in 0..8 {
                let mut damage: Vec<DamageRect> = Vec::new();
                for _ in 0..1 + rng.below(3) {
                    let rect = random_rect(&mut rng, sw, sh);
                    fill_rect(&mut rng, &mut frame, stride, rect);
                    damage.push(rect);
                }
                if iter % 3 == 2 {
                    // Move-like: copy a block to a new place and report both ends.
                    let (w, h) = (
                        1 + rng.below((sw / 4).max(1)),
                        1 + rng.below((sh / 4).max(1)),
                    );
                    let from = (rng.below(sw - w + 1), rng.below(sh - h + 1));
                    let to = (rng.below(sw - w + 1), rng.below(sh - h + 1));
                    let mut block = Vec::with_capacity(w * h * 4);
                    for y in 0..h {
                        let p = (from.1 + y) * stride + from.0 * 4;
                        block.extend_from_slice(&frame[p..p + w * 4]);
                    }
                    for y in 0..h {
                        let p = (to.1 + y) * stride + to.0 * 4;
                        frame[p..p + w * 4].copy_from_slice(&block[y * w * 4..(y + 1) * w * 4]);
                    }
                    damage.push(DamageRect {
                        left: from.0 as u32,
                        top: from.1 as u32,
                        right: (from.0 + w) as u32,
                        bottom: (from.1 + h) as u32,
                    });
                    damage.push(DamageRect {
                        left: to.0 as u32,
                        top: to.1 as u32,
                        right: (to.0 + w) as u32,
                        bottom: (to.1 + h) as u32,
                    });
                } else if iter % 3 == 1 {
                    // Report the union of this frame's changes as one rectangle.
                    damage = vec![union_rect(&damage)];
                }
                let list = incremental_rects(&plan, Some(damage.as_slice())).expect("valid damage");
                let options = CpuConversionOptions {
                    simd: iter % 3 != 0,
                    damage: true,
                    threads: 1 + iter % 2,
                };
                convert_into(
                    &plan,
                    &frame,
                    stride,
                    options,
                    Some(list.as_slice()),
                    dest.as_mut_slice(),
                );
                assert_same(
                    &format!("{sw}x{sh}->{ow}x{oh} iter {iter}"),
                    &dest,
                    &reference_fused(&frame, stride, sw, sh, ow, oh),
                );
            }
        }
    }

    #[test]
    fn changes_to_unsampled_source_columns_leave_output_unchanged() {
        let (sw, sh, ow, oh) = (1001, 563, 640, 360);
        let stride = sw * 4 + 8;
        let plan = Plan::new(sw, sh, ow, oh).unwrap();
        let mut frame = vec![0u8; stride * sh];
        let mut rng = Rng(77);
        rng.fill(&mut frame);
        let mut sampled = vec![false; sw];
        for &x in &plan.col_map {
            sampled[x as usize] = true;
        }
        let column = (0..sw)
            .find(|&x| !sampled[x])
            .expect("downscale skips columns");
        let before = reference_fused(&frame, stride, sw, sh, ow, oh);
        let mut dest = before.clone();
        for y in 0..sh {
            rng.fill(&mut frame[y * stride + column * 4..y * stride + column * 4 + 4]);
        }
        let damage = [DamageRect {
            left: column as u32,
            top: 0,
            right: column as u32 + 1,
            bottom: sh as u32,
        }];
        let list = incremental_rects(&plan, Some(damage.as_slice())).unwrap();
        assert!(list.is_empty(), "unsampled column must not touch output");
        let options = CpuConversionOptions::default();
        convert_into(
            &plan,
            &frame,
            stride,
            options,
            Some(list.as_slice()),
            dest.as_mut_slice(),
        );
        assert_same(
            "unsampled",
            &dest,
            &reference_fused(&frame, stride, sw, sh, ow, oh),
        );
        assert_same("unsampled unchanged", &dest, &before);
    }

    #[test]
    fn output_rects_cover_every_changed_output_sample_and_are_even() {
        let mut rng = Rng(4242);
        for (sw, sh, ow, oh) in [
            (1001, 563, 640, 360),
            (33, 17, 64, 36),
            (101, 75, 46, 30),
            (7, 5, 20, 12),
            (1, 1, 2, 2),
        ] {
            let plan = Plan::new(sw, sh, ow, oh).unwrap();
            for _ in 0..20 {
                let rect = random_rect(&mut rng, sw, sh);
                let list = incremental_rects(&plan, Some(&[rect])).unwrap();
                for r in &list {
                    assert!(r.x0 % 2 == 0 && r.x1 % 2 == 0 && r.y0 % 2 == 0 && r.y1 % 2 == 0);
                    assert!(r.x0 < r.x1 && r.y0 < r.y1 && r.x1 <= ow && r.y1 <= oh);
                }
                for oy in 0..oh {
                    for ox in 0..ow {
                        let sx = ox * sw / ow;
                        let sy = oy * sh / oh;
                        let changed = (rect.left as usize..rect.right as usize).contains(&sx)
                            && (rect.top as usize..rect.bottom as usize).contains(&sy);
                        if changed {
                            assert!(
                                list.iter()
                                    .any(|r| (r.x0..r.x1).contains(&ox)
                                        && (r.y0..r.y1).contains(&oy)),
                                "{sw}x{sh}->{ow}x{oh} rect {rect:?} misses output ({ox},{oy})"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn missing_or_invalid_damage_requests_full_refresh() {
        let plan = Plan::new(640, 360, 640, 360).unwrap();
        assert_eq!(incremental_rects(&plan, None), None);
        let bad = [
            DamageRect {
                left: 10,
                top: 0,
                right: 10,
                bottom: 5,
            },
            DamageRect {
                left: 0,
                top: 7,
                right: 5,
                bottom: 7,
            },
            DamageRect {
                left: 0,
                top: 0,
                right: 641,
                bottom: 5,
            },
            DamageRect {
                left: 0,
                top: 0,
                right: 5,
                bottom: 361,
            },
        ];
        for rect in bad {
            assert_eq!(incremental_rects(&plan, Some(&[rect])), None, "{rect:?}");
        }
        let good = DamageRect {
            left: 0,
            top: 0,
            right: 8,
            bottom: 8,
        };
        assert_eq!(incremental_rects(&plan, Some(&[good, bad[0]])), None);
        assert_eq!(incremental_rects(&plan, Some(&[])), Some(Vec::new()));
    }

    #[test]
    fn resize_or_replaced_buffer_requires_full_refresh() {
        let full = Plan::new(640, 360, 640, 360).unwrap();
        let small = Plan::new(640, 360, 320, 180).unwrap();
        let dest = vec![0u8; full.output_len()];
        let retained = Some(Retained {
            ptr: dest.as_ptr() as usize,
            output_width: 640,
            output_height: 360,
        });
        assert!(can_patch(retained, &dest, &full));
        assert!(!can_patch(retained, &dest, &small));
        assert!(!can_patch(None, &dest, &full));
        let other = vec![0u8; full.output_len()];
        assert!(!can_patch(retained, &other, &full));
        assert!(!can_patch(retained, &dest[..10], &full));
    }

    #[test]
    fn cpu_options_parse_flags_and_clamp_threads() {
        assert_eq!(
            CpuConversionOptions::from_values(None, None, None),
            CpuConversionOptions::default()
        );
        assert_eq!(
            CpuConversionOptions::default(),
            CpuConversionOptions {
                simd: true,
                damage: true,
                threads: 1
            }
        );
        assert_eq!(
            CpuConversionOptions::from_values(Some("0"), Some("false"), Some("2")),
            CpuConversionOptions {
                simd: false,
                damage: false,
                threads: 2
            }
        );
        assert_eq!(
            CpuConversionOptions::from_values(Some("junk"), Some(" on "), Some("3")),
            CpuConversionOptions {
                simd: true,
                damage: true,
                threads: 1
            }
        );
        assert_eq!(
            CpuConversionOptions {
                simd: true,
                damage: true,
                threads: 9
            }
            .normalized()
            .threads,
            2
        );
    }
}
