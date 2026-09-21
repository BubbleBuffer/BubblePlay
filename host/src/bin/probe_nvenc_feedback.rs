//! Proves that the active HEVC NVENC path can write a reconstructed frame to
//! an application-owned D3D11 NV12 texture. This diagnostic intentionally
//! reads the texture back only to establish the hardware/API boundary; the
//! streaming experiment consumes the same surface with a compute shader.

use bubbleplay_host::encode;
use bubbleplay_host::reconstruction_feedback;

use std::ffi::c_void;

use anyhow::{ensure, Context, Result};
use encode::{EncoderOptions, Nvenc, Preset, RoiMode, VideoCodec};
use reconstruction_feedback::ReconstructionFeedbackAnalyzer;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;

fn texture_desc(
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: WIDTH,
        Height: HEIGHT,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: if format == DXGI_FORMAT_NV12 {
            D3D11_BIND_SHADER_RESOURCE.0 as u32
        } else {
            0
        },
        CPUAccessFlags: 0,
        MiscFlags: 0,
    }
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .context("D3D11CreateDevice")?;
    Ok((
        device.context("D3D11CreateDevice returned no device")?,
        context.context("D3D11CreateDevice returned no context")?,
    ))
}

fn create_input(device: &ID3D11Device) -> Result<ID3D11Texture2D> {
    let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let offset = ((y * WIDTH + x) * 4) as usize;
            pixels[offset] = ((x * 2) & 0xff) as u8;
            pixels[offset + 1] = ((y * 4) & 0xff) as u8;
            pixels[offset + 2] = (((x + y) * 3) & 0xff) as u8;
            pixels[offset + 3] = 0xff;
        }
    }
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast::<c_void>(),
        SysMemPitch: WIDTH * 4,
        SysMemSlicePitch: (WIDTH * HEIGHT * 4),
    };
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(
            &texture_desc(DXGI_FORMAT_B8G8R8A8_UNORM),
            Some(&initial),
            Some(&mut texture),
        )
    }
    .context("CreateTexture2D(input)")?;
    texture.context("CreateTexture2D(input) returned no texture")
}

fn read_luma(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    source: &ID3D11Texture2D,
) -> Result<(u8, u8, u64)> {
    let mut desc = texture_desc(DXGI_FORMAT_NV12);
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    let mut staging = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }
        .context("CreateTexture2D(reconstruction staging)")?;
    let staging = staging.context("missing reconstruction staging texture")?;
    unsafe { context.CopyResource(&staging, source) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
        .context("Map(reconstruction staging)")?;
    let mut minimum = u8::MAX;
    let mut maximum = u8::MIN;
    let mut sum = 0u64;
    for row in 0..HEIGHT as usize {
        let bytes = unsafe {
            std::slice::from_raw_parts(
                mapped
                    .pData
                    .cast::<u8>()
                    .add(row * mapped.RowPitch as usize),
                WIDTH as usize,
            )
        };
        for value in bytes {
            minimum = minimum.min(*value);
            maximum = maximum.max(*value);
            sum += u64::from(*value);
        }
    }
    unsafe { context.Unmap(&staging, 0) };
    Ok((minimum, maximum, sum))
}

fn main() -> Result<()> {
    let (device, context) = create_device()?;
    let input = create_input(&device)?;
    let mut feedback = ReconstructionFeedbackAnalyzer::new(&device, WIDTH, HEIGHT)?;
    let mut options = EncoderOptions::production(Preset::P3);
    options.roi_mode = RoiMode::Feedback;
    let mut encoder = Nvenc::new_with_options_native_codec(
        device.as_raw(),
        WIDTH,
        HEIGHT,
        WIDTH,
        HEIGHT,
        30,
        1_000_000,
        options,
        VideoCodec::Hevc,
    )?;
    let encoded = encoder.encode_texture_with_feedback(
        input.as_raw(),
        None,
        feedback.reconstruction_texture_raw(1),
    )?;
    let (minimum, maximum, sum) = read_luma(&device, &context, feedback.reconstruction_texture(1))?;
    ensure!(maximum > minimum, "reconstructed luma surface is constant");
    ensure!(sum > 0, "reconstructed luma surface is empty");
    let step = feedback.submit_and_collect(&input, 1)?;
    ensure!(
        step.submitted,
        "reconstruction error reducer did not submit"
    );
    let reduced = step
        .completed
        .or(feedback.collect_blocking(2)?)
        .context("reconstruction error reducer returned no result")?;
    ensure!(
        reduced.errors.len() == WIDTH.div_ceil(32) as usize * HEIGHT.div_ceil(32) as usize,
        "reconstruction error reducer returned the wrong CTB grid"
    );
    let error_sum = reduced
        .errors
        .iter()
        .map(|value| u64::from(*value))
        .sum::<u64>();
    ensure!(
        error_sum > 0,
        "reconstruction error reducer returned an empty error map"
    );
    println!(
        "HEVC recon surface verified: bytes={} idr={} luma_min={} luma_max={} luma_sum={} ctb_errors={} ctb_error_sum={} submit_us={} readback_us={}",
        encoded.data.len(),
        encoded.is_idr,
        minimum,
        maximum,
        sum,
        reduced.errors.len(),
        error_sum,
        step.submit_us,
        reduced.readback_us,
    );
    Ok(())
}
