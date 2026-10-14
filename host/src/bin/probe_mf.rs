//! Synthetic MF encoder acceptance probe. Never captures a desktop or starts a host.
use anyhow::{ensure, Context, Result};
use bubbleplay_host::{
    encode::{EncoderOptions, Nvenc, Preset, VideoCodec},
    gpu_gate,
    mf_encode::{annex_b_nals, inspect_access_unit, probe_availability, MfEncoder, MfFrameTimings},
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

#[derive(Clone, Copy, Default)]
struct ConversionDiagnostic {
    force_cpu: bool,
    require_video_processor: bool,
    gpu_completion: bool,
}

fn timing_summary(samples: &[MfFrameTimings], metric: impl Fn(&MfFrameTimings) -> u64) -> String {
    let mut values: Vec<u64> = samples.iter().map(metric).collect();
    values.sort_unstable();
    let average = values.iter().sum::<u64>() as f64 / values.len() as f64 / 1000.0;
    let p50 = values[(values.len() * 50).div_ceil(100) - 1] as f64 / 1000.0;
    let p95 = values[(values.len() * 95).div_ceil(100) - 1] as f64 / 1000.0;
    format!("avg:{average:.3}/p50:{p50:.3}/p95:{p95:.3}ms")
}

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

#[allow(clippy::too_many_arguments)]
fn run(
    device: &ID3D11Device,
    hardware: bool,
    output: Option<&PathBuf>,
    width: u32,
    height: u32,
    properties: bool,
    diagnostic: ConversionDiagnostic,
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
    if diagnostic.force_cpu {
        encoder.force_cpu_conversion()?;
    }
    encoder.set_diagnostic_gpu_completion(diagnostic.gpu_completion);
    if diagnostic.require_video_processor {
        ensure!(
            encoder.converter_name().starts_with("D3D11 VideoProcessor"),
            "requested VideoProcessor conversion unavailable: {}",
            encoder.converter_name()
        );
    }
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
    let mut timings = Vec::with_capacity(60);
    let mut timing_csv = String::from("frame,converter,conversion_readback_us,input_pack_us,mft_encode_us,output_processing_us,total_us,gpu_completion_waited,idr\n");
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
        let timing = encoder.last_frame_timings();
        ensure!(
            timing.total_us == encoded.encode_us,
            "frame {frame}: timing total mismatch"
        );
        if diagnostic.require_video_processor {
            ensure!(
                encoder.converter_name().starts_with("D3D11 VideoProcessor"),
                "frame {frame}: VideoProcessor fell back to {}",
                encoder.converter_name()
            );
        }
        timing_csv.push_str(&format!(
            "{frame},\"{}\",{},{},{},{},{},{},{}\n",
            encoder.converter_name(),
            timing.conversion_readback_us,
            timing.input_pack_us,
            timing.mft_encode_us,
            timing.output_processing_us,
            timing.total_us,
            timing.conversion_gpu_completion_waited,
            encoded.is_idr
        ));
        timings.push(timing);
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
    println!("TIMINGS backend={} converter={} dimensions={width}x{height} frames=60 debug_assertions={} conversion_mode={} conversion_readback={} mft_encode_including_input_output_wait={} input_pack={} output_processing={} total={}",
        encoder.backend_name(), encoder.converter_name(), cfg!(debug_assertions),
        if !hardware { "blocking readback/CPU conversion complete" } else if timings.iter().all(|t| t.conversion_gpu_completion_waited) { "GPU completion diagnostic; includes 1ms polling overhead" } else { "GPU submission only; GPU execution may overlap MFT" },
        timing_summary(&timings, |t| t.conversion_readback_us),
        timing_summary(&timings, |t| t.mft_encode_us),
        timing_summary(&timings, |t| t.input_pack_us),
        timing_summary(&timings, |t| t.output_processing_us),
        timing_summary(&timings, |t| t.total_us));
    if let Some(dir) = output {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(if hardware {
            "mf-hardware.h264"
        } else {
            "mf-software.h264"
        });
        std::fs::write(&path, &stream)?;
        println!("synthetic stream: {}", path.display());
        let timing_path = dir.join(if hardware {
            "mf-hardware-timings.csv"
        } else {
            "mf-software-timings.csv"
        });
        std::fs::write(&timing_path, timing_csv)?;
        println!("per-frame timing CSV: {}", timing_path.display());
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
    if args.iter().any(|a| a == "--availability-only") {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
        let mut luid = None;
        for index in 0..64 {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
                break;
            };
            let desc = unsafe { adapter.GetDesc1() }?;
            if desc.VendorId == 0x10de {
                luid = Some(
                    (u64::from(desc.AdapterLuid.HighPart as u32) << 32)
                        | u64::from(desc.AdapterLuid.LowPart),
                );
                break;
            }
        }
        let luid = luid.context("availability probe requires the synthetic NVIDIA adapter")?;
        let started = Instant::now();
        let availability = probe_availability(Some(luid))?;
        println!("MF availability adapter_luid={luid:#x} result={availability:?} elapsed_ms={:.3} encoder_activated=false d3d_device_created=false frames=0",
            started.elapsed().as_secs_f64() * 1000.0);
        return Ok(());
    }
    let diagnostic = ConversionDiagnostic {
        force_cpu: args.iter().any(|a| a == "--force-cpu-conversion"),
        require_video_processor: args.iter().any(|a| a == "--require-video-processor"),
        gpu_completion: args.iter().any(|a| a == "--timing-gpu-completion"),
    };
    ensure!(
        !diagnostic.force_cpu || software_only,
        "--force-cpu-conversion requires --software-only"
    );
    ensure!(
        !(diagnostic.force_cpu && diagnostic.require_video_processor),
        "forced CPU conversion conflicts with required VideoProcessor"
    );
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
        run(
            &device,
            false,
            output.as_ref(),
            width,
            height,
            properties,
            diagnostic,
        )?;
    }
    if !software_only {
        run(
            &device,
            true,
            output.as_ref(),
            width,
            height,
            properties,
            diagnostic,
        )?;
    }
    Ok(())
}
