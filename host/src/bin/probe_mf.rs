//! Synthetic MF encoder acceptance probe. Never captures a desktop or starts a host.
use anyhow::{ensure, Context, Result};
use bubbleplay_host::{
    encode::{EncoderOptions, Nvenc, Preset, VideoCodec},
    gpu_gate,
    mf_encode::{annex_b_nals, inspect_access_unit, MfEncoder},
    video_encoder::{EncoderCapabilities, EncoderOverride, EncoderPreference, VideoEncoder},
};
use std::{ffi::c_void, path::PathBuf, time::Instant};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

// Match probe_encoder_color.rs's fixture for measured NV12/VUI comparisons.
const COLOR_PATCHES: [[u8; 3]; 8] = [
    [0, 0, 0],
    [255, 255, 255],
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [128, 128, 128],
    [0, 255, 255],
    [255, 0, 255],
];

fn create_device(warp: bool, no_video_support: bool) -> Result<ID3D11Device> {
    let mut device = None;
    let flags = if warp || no_video_support {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT
    } else {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT
    };
    if warp {
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }
        .context("synthetic WARP D3D11 device")?;
    } else {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
        let mut nvidia = None;
        for index in 0..64 {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
                break;
            };
            let desc = unsafe { adapter.GetDesc1() }?;
            if desc.VendorId == 0x10de {
                let name_end = desc
                    .Description
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(desc.Description.len());
                println!(
                    "synthetic capture adapter: {} vendor=0x{:04x}",
                    String::from_utf16_lossy(&desc.Description[..name_end]),
                    desc.VendorId
                );
                nvidia = Some(adapter);
                break;
            }
        }
        let nvidia = nvidia.context(
            "probe requires NVIDIA adapter; use --warp --software-only to qualify CPU fallback",
        )?;
        unsafe {
            D3D11CreateDevice(
                &nvidia,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }
        .context("synthetic NVIDIA D3D11 device")?;
    }
    let device = device.context("synthetic D3D11 device missing")?;
    gpu_gate::protect_device(&device)?;
    Ok(device)
}

fn run(
    device: &ID3D11Device,
    hardware: bool,
    output: Option<&PathBuf>,
    width: u32,
    height: u32,
    properties: bool,
) -> Result<()> {
    let (fps, bitrate) = (30u32, 6_000_000u32);
    let gate = gpu_gate::for_device(device);
    let (texture, context) = {
        let _gpu = gate.lock();
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
            BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
            ..Default::default()
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }?;
        (texture.context("synthetic texture missing")?, unsafe {
            device.GetImmediateContext()
        }?)
    };
    let init_started = Instant::now();
    let mut encoder = MfEncoder::new_with_gop(
        device.as_raw(),
        width,
        height,
        fps,
        bitrate,
        true,
        hardware,
        2,
    )?;
    println!(
        "backend={} implementation={} init_ms={:.2}",
        encoder.backend_name(),
        encoder.implementation_name(),
        init_started.elapsed().as_secs_f64() * 1000.0
    );
    if hardware {
        ensure!(
            encoder
                .backend_name()
                .to_ascii_lowercase()
                .contains("nvidia"),
            "forced NVIDIA MF probe selected another implementation: {}",
            encoder.implementation_name()
        );
    }
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let mut stream = Vec::new();
    let mut elapsed_us = 0u64;
    let mut idrs = Vec::new();
    for frame in 0..60usize {
        for y in 0..height as usize {
            for x in 0..width as usize {
                let p = (y * width as usize + x) * 4;
                // First frame is the unchanged eight-patch colour fixture.
                // A small moving box exercises later-frame motion.
                let box_x = frame * 13 % (width as usize - 96);
                let white = frame > 0 && x >= box_x && x < box_x + 96 && (240..336).contains(&y);
                let rgb = COLOR_PATCHES[y / (height as usize / 2) * 4 + x / (width as usize / 4)];
                pixels[p] = if white { 255 } else { rgb[2] };
                pixels[p + 1] = if white { 255 } else { rgb[1] };
                pixels[p + 2] = if white { 255 } else { rgb[0] };
                pixels[p + 3] = 255;
            }
        }
        {
            let _gpu = gate.lock();
            unsafe {
                context.UpdateSubresource(
                    &texture,
                    0,
                    None,
                    pixels.as_ptr() as *const c_void,
                    width * 4,
                    0,
                )
            };
        }
        if frame == 30 {
            encoder.force_idr();
            ensure!(encoder.idr_pending(), "forced IDR was not queued");
        }
        let encoded = encoder
            .encode_texture(texture.as_raw())
            .with_context(|| format!("encode synthetic frame {frame}"))?;
        let inspect = inspect_access_unit(&encoded.data)?;
        ensure!(
            inspect.access_units == 1 && !inspect.has_b_slice,
            "frame {frame}: one AU and no B-slices required"
        );
        ensure!(
            inspect.is_idr == encoded.is_idr,
            "frame {frame}: wrong IDR flag"
        );
        if inspect.is_idr {
            ensure!(
                inspect.sps_before_idr
                    && inspect.pps_before_idr
                    && inspect.profile_idc == Some(100),
                "frame {frame}: High SPS/PPS must precede IDR"
            );
            idrs.push(frame);
        }
        ensure!(!encoder.idr_pending(), "completed frame left IDR pending");
        if frame == 0 {
            if let Some(dir) = output {
                std::fs::create_dir_all(dir)?;
                let path = dir.join(if hardware {
                    "mf-hardware-first-au.h264"
                } else {
                    "mf-software-first-au.h264"
                });
                std::fs::write(&path, &encoded.data)?;
                println!(
                    "synthetic eight-patch first AU: {} dimensions={width}x{height}",
                    path.display()
                );
            }
        }
        elapsed_us += encoded.encode_us;
        stream.extend_from_slice(&encoded.data);
    }
    ensure!(
        idrs == [0, 30],
        "infinite GOP produced unexpected IDRs: {idrs:?}"
    );
    println!("PASS backend={} inputs=60 access_units=60 idrs={idrs:?} b_slices=0 profile_idc=100 annex_b=true avg_encode_ms={:.3} bytes={}",
        encoder.backend_name(),elapsed_us as f64/60.0/1000.0,stream.len());
    if let Some(dir) = output {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(if hardware {
            "mf-hardware.h264"
        } else {
            "mf-software.h264"
        });
        std::fs::write(&path, &stream)?;
        println!("synthetic stream: {}", path.display());
    }
    if properties {
        run_properties(device, &mut encoder, &texture, output, hardware)?;
    }
    Ok(())
}

fn fixture(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let rgb = COLOR_PATCHES[(y / (height / 2) * 4 + x / (width / 4)) as usize];
            let p = ((y * width + x) * 4) as usize;
            pixels[p..p + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
        }
    }
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
        BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
        ..Default::default()
    };
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: width * 4,
        SysMemSlicePitch: width * height * 4,
    };
    let gate = gpu_gate::for_device(device);
    let _gpu = gate.lock();
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(&initial), Some(&mut texture)) }?;
    texture.context("property-test fixture texture missing")
}

fn check_property_frame(
    encoder: &mut MfEncoder,
    texture: &ID3D11Texture2D,
    idr: bool,
) -> Result<Vec<u8>> {
    let frame = encoder.encode_texture(texture.as_raw())?;
    let check = inspect_access_unit(&frame.data)?;
    ensure!(
        check.access_units == 1 && !check.has_b_slice && check.is_idr == idr,
        "property transition must yield one AU, no B, expectedIDR={idr}: {check:?}"
    );
    if idr {
        ensure!(
            check.sps_before_idr && check.pps_before_idr && check.profile_idc == Some(100),
            "profile/reset IDR headers invalid"
        );
    }
    ensure!(
        !encoder.idr_pending(),
        "completed property frame left IDR pending"
    );
    Ok(frame.data)
}

fn run_properties(
    device: &ID3D11Device,
    encoder: &mut MfEncoder,
    initial: &ID3D11Texture2D,
    output: Option<&PathBuf>,
    hardware: bool,
) -> Result<()> {
    encoder.set_bitrate(4_000_000)?;
    ensure!(
        encoder.bitrate() == 4_000_000,
        "desired bitrate getter did not update"
    );
    check_property_frame(encoder, initial, false)
        .context("live bitrate must preserve P-frame/infinite GOP")?;
    let mut texture = initial.clone();
    for (width, height, fps) in [(960, 540, 24), (640, 576, 30), (1280, 720, 30)] {
        encoder.reconfigure_profile(width, height, fps)?;
        ensure!(
            encoder.idr_pending() && encoder.fps() == fps,
            "profile getters must expose queued transition"
        );
        texture = fixture(device, width, height)?;
        let data = check_property_frame(encoder, &texture, true)
            .with_context(|| format!("profile {width}x{height}@{fps}"))?;
        if let Some(dir) = output {
            let path = dir.join(format!(
                "mf-{}-profile-{width}x{height}.h264",
                if hardware { "hardware" } else { "software" }
            ));
            std::fs::write(path, data)?;
        }
    }
    encoder.set_bitrate(5_000_000)?;
    encoder.reset_rate_control()?;
    ensure!(
        encoder.idr_pending(),
        "rate reset must expose queued recreation IDR"
    );
    check_property_frame(encoder, &texture, true).context("rate reset")?;
    ensure!(
        encoder.bitrate() == 5_000_000 && encoder.fps() == 30,
        "reset lost desired settings"
    );
    encoder.set_max_qp(Some(32))?;
    ensure!(
        encoder.max_qp().is_none(),
        "unsupported MF QP ceiling must not claim applied state"
    );
    check_property_frame(encoder, &texture, false).context("P-frame after reset")?;
    println!("PASS properties backend={} live_bitrate_P=true profiles=960x540@24,640x576@30,1280x720@30 reset_preserves_settings=true qp_ceiling_unsupported=true",encoder.backend_name());
    Ok(())
}

fn selection_preflight(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    cpu_only: bool,
) -> Result<()> {
    let texture = fixture(device, width, height)?;
    let cases = [
        ("auto", EncoderPreference::default()),
        (
            "cpu-mode",
            EncoderPreference {
                cpu_mode: true,
                encoder: EncoderOverride::Auto,
            },
        ),
        (
            "mf-hw",
            EncoderPreference {
                cpu_mode: false,
                encoder: EncoderOverride::MfHardware,
            },
        ),
        (
            "mf-sw",
            EncoderPreference {
                cpu_mode: false,
                encoder: EncoderOverride::MfSoftware,
            },
        ),
    ];
    for (label, preference) in cases {
        if cpu_only && !preference.cpu_mode {
            continue;
        }
        let requested = VideoCodec::preferred();
        let mut nvenc_called = false;
        let mut encoder = VideoEncoder::select(
            device.as_raw(),
            width,
            height,
            30,
            6_000_000,
            false,
            2,
            preference,
            None,
            || {
                assert!(
                    !preference.cpu_mode,
                    "CPU mode must never invoke NVENC factory"
                );
                nvenc_called = true;
                // Same production RTC constructor/configuration as host_encode's
                // ordinary preset path: P3+ULL, CBR, one-frame VBV, spatial AQ,
                // no lookahead/B, native maximum dimensions and preferred codec.
                let create = |preset| {
                    Nvenc::new_with_options_native_codec_gop(
                        device.as_raw(),
                        width,
                        height,
                        width,
                        height,
                        30,
                        6_000_000,
                        EncoderOptions::production(preset),
                        requested,
                        false,
                    )
                };
                create(Preset::P3).or_else(|_| create(Preset::P1))
            },
        )?;
        let capabilities = encoder.capabilities();
        if preference.cpu_mode || preference.encoder == EncoderOverride::MfSoftware {
            ensure!(
                encoder.backend_name() == "Microsoft software H.264",
                "{label}: software was not selected"
            );
            ensure!(!nvenc_called, "{label}: NVENC factory must not be called");
        } else if preference.encoder == EncoderOverride::MfHardware {
            ensure!(
                encoder.backend_name() == "Media Foundation hardware H.264, NVIDIA",
                "{label}: forced Nvidia hardware was not selected"
            );
            ensure!(!nvenc_called, "MF override must not invoke NVENC factory");
        } else {
            ensure!(
                encoder.backend_name() == "NVENC" && nvenc_called,
                "Nvidia Auto must preserve NVENC as first choice"
            );
        }
        let codec = encoder.codec();
        if encoder.backend_name() == "NVENC" {
            ensure!(
                capabilities == EncoderCapabilities::NVENC && codec == requested,
                "NVENC production capabilities/codec changed"
            );
        } else {
            ensure!(
                capabilities == EncoderCapabilities::MF && codec == VideoCodec::H264,
                "MF must report H264-only and no NVENC extension capabilities"
            );
        }
        let frame = encoder.encode_texture(texture.as_raw())?;
        ensure!(
            frame.is_idr && !frame.data.is_empty(),
            "{label}: first synthetic selection frame must be IDR"
        );
        if codec == VideoCodec::H264 {
            let check = inspect_access_unit(&frame.data)?;
            ensure!(
                check.access_units == 1
                    && !check.has_b_slice
                    && check.profile_idc == Some(100)
                    && check.sps_before_idr
                    && check.pps_before_idr,
                "{label}: selected H264 frame contract failed"
            );
        } else {
            let nals = annex_b_nals(&frame.data)?;
            ensure!(
                nals.iter()
                    .any(|nal| (16..=23).contains(&((nal[0] >> 1) & 63))),
                "Auto NVENC HEVC frame is not random-access AnnexB"
            );
        }
        println!("PASS selection={label} backend={} codec={} capabilities={capabilities:?} nvenc_factory_called={nvenc_called} encoded_one_synthetic_IDR=true",encoder.backend_name(),codec.name());
    }
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_target(false)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let software_only = args.iter().any(|a| a == "--software-only");
    let hardware_only = args.iter().any(|a| a == "--hardware-only");
    let warp = args.iter().any(|a| a == "--warp");
    let no_video_support = args.iter().any(|a| a == "--no-video-support");
    let properties = args.iter().any(|a| a == "--properties");
    let preflight = args.iter().any(|a| a == "--selection-preflight");
    let cpu_mode = args.iter().any(|a| a == "--cpu-mode");
    let (width, height) = if let Some(i) = args.iter().position(|a| a == "--size") {
        let (w, h) = args
            .get(i + 1)
            .context("missing --size WIDTHxHEIGHT")?
            .split_once('x')
            .context("invalid --size WIDTHxHEIGHT")?;
        (w.parse::<u32>()?, h.parse::<u32>()?)
    } else {
        (1280, 720)
    };
    ensure!(
        width >= 128 && width.is_multiple_of(4) && height >= 2 && height.is_multiple_of(2),
        "fixture requires width>=128 divisible by4 and positive even height"
    );
    ensure!(
        !(software_only && hardware_only),
        "software/hardware-only flags conflict"
    );
    ensure!(
        !warp || software_only || preflight && cpu_mode,
        "--warp requires --software-only or selection preflight CPU mode"
    );
    let output = args
        .iter()
        .position(|a| a == "--output-dir")
        .map(|i| {
            args.get(i + 1)
                .map(PathBuf::from)
                .context("missing --output-dir value")
        })
        .transpose()?;
    println!(
        "synthetic device video_support_flag={}",
        !warp && !no_video_support
    );
    let device = create_device(warp, no_video_support)?;
    if preflight {
        return selection_preflight(&device, width, height, cpu_mode);
    }
    if !hardware_only {
        run(&device, false, output.as_ref(), width, height, properties)?;
    }
    if !software_only {
        run(&device, true, output.as_ref(), width, height, properties)?;
    }
    Ok(())
}
