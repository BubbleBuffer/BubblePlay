//! Hardware check: can a rate-control QP ceiling be reconfigured mid-stream
//! without a reset or an IDR, and does NVENC honour it?
//!
//! Encodes changing 1080p noise (so CBR at a low bitrate drives QP high) with
//! the production v4 encoder options, sets a max-QP ceiling, then lifts it,
//! and prints per-phase frame QP, sizes and whether any IDR appeared.

use std::ffi::c_void;

use anyhow::{ensure, Context, Result};
use bubbleplay_host::encode::{EncoderOptions, Nvenc, Preset, RoiMode, VideoCodec};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FPS: u32 = 30;
const BITRATE_BPS: u32 = 1_000_000;
const CEILING_QP: u32 = 36;
const PHASE_FRAMES: usize = 30;
const TEXTURES: usize = 6;

fn create_device() -> Result<ID3D11Device> {
    let mut device = None;
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
            None,
        )
    }
    .context("D3D11CreateDevice")?;
    device.context("D3D11CreateDevice returned no device")
}

fn noise_texture(device: &ID3D11Device, seed: u32) -> Result<ID3D11Texture2D> {
    let mut state = seed.wrapping_mul(0x9e37_79b9) | 1;
    let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    for pixel in pixels.chunks_exact_mut(4) {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        pixel[0] = state as u8;
        pixel[1] = (state >> 8) as u8;
        pixel[2] = (state >> 16) as u8;
        pixel[3] = 0xff;
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: WIDTH,
        Height: HEIGHT,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: 0,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast::<c_void>(),
        SysMemPitch: WIDTH * 4,
        SysMemSlicePitch: WIDTH * HEIGHT * 4,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(&initial), Some(&mut texture)) }
        .context("CreateTexture2D(noise)")?;
    texture.context("CreateTexture2D returned no texture")
}

#[derive(Debug, Default)]
struct Phase {
    frames: usize,
    idrs: usize,
    qp_max: u32,
    qp_sum: u64,
    bytes: u64,
}

fn run_phase(
    encoder: &mut Nvenc,
    textures: &[ID3D11Texture2D],
    offset: usize,
    skip_first: usize,
) -> Result<Phase> {
    let mut phase = Phase::default();
    for index in 0..PHASE_FRAMES {
        let texture = &textures[(offset + index) % textures.len()];
        let frame = encoder.encode_texture(texture.as_raw())?;
        if index < skip_first {
            continue;
        }
        phase.frames += 1;
        phase.idrs += usize::from(frame.is_idr);
        phase.qp_max = phase.qp_max.max(frame.frame_avg_qp);
        phase.qp_sum += u64::from(frame.frame_avg_qp);
        phase.bytes += frame.data.len() as u64;
    }
    Ok(phase)
}

fn main() -> Result<()> {
    let device = create_device()?;
    let textures = (0..TEXTURES as u32)
        .map(|seed| noise_texture(&device, seed + 1))
        .collect::<Result<Vec<_>>>()?;
    let mut options = EncoderOptions::production(Preset::P3);
    options.roi_mode = RoiMode::Refine;
    let mut encoder = Nvenc::new_with_options_native_codec_gop(
        device.as_raw(),
        WIDTH,
        HEIGHT,
        WIDTH,
        HEIGHT,
        FPS,
        BITRATE_BPS,
        options,
        VideoCodec::Hevc,
        true,
    )?;
    // Skip the opening IDR and rate-control settling.
    let open = run_phase(&mut encoder, &textures, 0, 5)?;
    encoder.set_max_qp(Some(CEILING_QP))?;
    ensure!(encoder.max_qp() == Some(CEILING_QP), "ceiling not recorded");
    let capped = run_phase(&mut encoder, &textures, 1, 0)?;
    encoder.set_max_qp(None)?;
    let lifted = run_phase(&mut encoder, &textures, 2, 0)?;
    // A second cap / lift cycle.
    encoder.set_max_qp(Some(CEILING_QP))?;
    let recapped = run_phase(&mut encoder, &textures, 3, 0)?;
    encoder.set_max_qp(None)?;
    let relifted = run_phase(&mut encoder, &textures, 4, 0)?;
    for (name, phase) in [
        ("open", &open),
        ("capped", &capped),
        ("lifted", &lifted),
        ("recapped", &recapped),
        ("relifted", &relifted),
    ] {
        println!(
            "{name}: frames={} idrs={} qp_max={} qp_mean={:.1} kbps={:.0}",
            phase.frames,
            phase.idrs,
            phase.qp_max,
            phase.qp_sum as f64 / phase.frames.max(1) as f64,
            phase.bytes as f64 * 8.0 / 1000.0 / (phase.frames as f64 / f64::from(FPS))
        );
    }
    ensure!(
        [&capped, &lifted, &recapped, &relifted]
            .iter()
            .all(|phase| phase.idrs == 0),
        "reconfigure produced an IDR"
    );
    ensure!(
        capped.qp_max <= CEILING_QP && recapped.qp_max <= CEILING_QP,
        "ceiling not honoured"
    );
    ensure!(
        lifted.qp_max > CEILING_QP && relifted.qp_max > CEILING_QP,
        "ceiling not lifted"
    );
    println!("max-QP reconfigure: no reset, no IDR, ceiling honoured and lifted");
    Ok(())
}
