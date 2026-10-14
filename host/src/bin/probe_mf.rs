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
    let p99 = values[(values.len() * 99).div_ceil(100) - 1] as f64 / 1000.0;
    format!("avg:{average:.3}/p50:{p50:.3}/p95:{p95:.3}/p99:{p99:.3}ms")
}

fn argument<'a>(args: &'a [String], flag: &str) -> Result<Option<&'a str>> {
    args.iter()
        .position(|a| a == flag)
        .map(|i| {
            args.get(i + 1)
                .map(String::as_str)
                .context(format!("missing {flag} value"))
        })
        .transpose()
}

fn size_argument(args: &[String], flag: &str) -> Result<Option<(u32, u32)>> {
    argument(args, flag)?
        .map(|s| {
            let (w, h) = s.split_once('x').context("expected WIDTHxHEIGHT")?;
            Ok((w.parse()?, h.parse()?))
        })
        .transpose()
}

/// Flags also map to the hidden CPU-only host environment controls. Hardware
/// MFT settings are deliberately independent of these environment variables.
fn cpu_environment(args: &[String]) -> Result<()> {
    for (flag, env) in [
        ("--cpu-simd", "BP_CPU_SIMD"),
        ("--cpu-damage", "BP_CPU_DAMAGE"),
        ("--cpu-convert-threads", "BP_CPU_CONVERT_THREADS"),
        ("--mft-threads", "BP_CPU_MFT_THREADS"),
        ("--quality-speed", "BP_CPU_MFT_QUALITY_SPEED"),
        ("--cabac", "BP_CPU_MFT_CABAC"),
        ("--low-latency", "BP_CPU_MFT_LOW_LATENCY"),
        ("--warp-bypass", "BP_CPU_WARP_BYPASS"),
        ("--adaptive-fps", "BP_CPU_ADAPTIVE_FPS"),
    ] {
        if let Some(value) = argument(args, flag)? {
            std::env::set_var(env, value);
            println!("cpu_control {env}={value}");
        }
    }
    if args.iter().any(|a| a == "--force-cpu-conversion") {
        std::env::set_var("BP_CPU_FORCE_CONVERSION", "1");
    }
    Ok(())
}

fn bench_pixels(width: u32, height: u32, frame: usize, full: bool) -> Vec<u8> {
    let mut pixels = vec![0; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            // Desktop-like panels, text strokes, a shaded image and a moving
            // window. Every full-content frame changes the whole RGB surface.
            let panel = x < width / 3;
            let text = panel && y % 24 < 2 && x % 120 < 98;
            let motion = x >= (frame as u32 * 13) % (width - 96)
                && x < (frame as u32 * 13) % (width - 96) + 96
                && y >= height / 3
                && y < height / 3 + 96;
            let v = if text {
                30
            } else if panel {
                225
            } else {
                48 + ((x / 4 + y / 4) % 128) as u8
            };
            let delta = if full { (frame % 37) as u8 } else { 0 };
            let p = ((y * width + x) * 4) as usize;
            pixels[p..p + 4].copy_from_slice(&[
                if motion { 240 } else { v.wrapping_add(delta) },
                if motion { 180 } else { v.wrapping_add(delta) },
                if motion { 60 } else { v.wrapping_add(delta) },
                255,
            ]);
        }
    }
    pixels
}

fn bench_damage(
    width: u32,
    height: u32,
    frame: usize,
) -> Vec<bubbleplay_host::frame_damage::DamageRect> {
    use bubbleplay_host::frame_damage::DamageRect;
    [frame.saturating_sub(1) % 36, frame % 36]
        .map(|n| {
            let left = (n as u32 * 13) % (width - 96);
            DamageRect {
                left,
                top: height / 3,
                right: left + 96,
                bottom: height / 3 + 96,
            }
        })
        .to_vec()
}

fn validate_bench_au(data: &[u8], forced: bool, size: (u32, u32)) -> Result<()> {
    let inspect = inspect_access_unit(data)?;
    ensure!(
        inspect.access_units == 1 && !inspect.has_b_slice && inspect.is_idr == forced,
        "bench requires one AU, no B, and IDR only when forced"
    );
    if forced {
        ensure!(
            inspect.profile_idc == Some(100) && inspect.sps_before_idr && inspect.pps_before_idr,
            "bench requires High SPS/PPS before every IDR"
        );
        for nal in annex_b_nals(data)?.into_iter().filter(|n| n[0] & 31 == 7) {
            ensure!(
                check_sps_colour_omission(nal)? == size,
                "SPS size differs from CPU target {size:?}"
            );
        }
    }
    Ok(())
}

struct SpsBits {
    bytes: Vec<u8>,
    position: usize,
}
impl SpsBits {
    fn new(nal: &[u8]) -> Self {
        let mut bytes = Vec::new();
        let mut zeros = 0;
        for &byte in &nal[1..] {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            bytes.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        Self { bytes, position: 0 }
    }
    fn bits(&mut self, n: usize) -> Result<u32> {
        ensure!(
            n <= 32 && self.position + n <= self.bytes.len() * 8,
            "truncated SPS"
        );
        let mut value = 0;
        for _ in 0..n {
            value = (value << 1)
                | u32::from((self.bytes[self.position / 8] >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Ok(value)
    }
    fn flag(&mut self) -> Result<bool> {
        Ok(self.bits(1)? != 0)
    }
    fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            ensure!(zeros < 31, "SPS Exp-Golomb overflow");
        }
        Ok((1 << zeros) - 1 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Result<i32> {
        let value = self.ue()?;
        Ok(if value & 1 != 0 {
            (value / 2 + 1) as i32
        } else {
            -(value as i32 / 2)
        })
    }
}

/// Independent probe check of the established SPS colour omission. Conversion
/// colour is explicit on the MFT input type, never added to the encoded VUI.
fn check_sps_colour_omission(nal: &[u8]) -> Result<(u32, u32)> {
    let mut b = SpsBits::new(nal);
    ensure!(b.bits(8)? == 100, "SPS profile must remain High");
    b.bits(16)?;
    b.ue()?;
    let chroma = b.ue()?;
    ensure!(chroma <= 3, "invalid SPS chroma format");
    if chroma == 3 {
        b.bits(1)?;
    }
    b.ue()?;
    b.ue()?;
    b.bits(1)?;
    if b.flag()? {
        for i in 0..if chroma == 3 { 12 } else { 8 } {
            if b.flag()? {
                let mut last = 8;
                let mut next = 8;
                for _ in 0..if i < 6 { 16 } else { 64 } {
                    if next != 0 {
                        next = (last + b.se()? + 256) % 256;
                    }
                    if next != 0 {
                        last = next;
                    }
                }
            }
        }
    }
    b.ue()?;
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.bits(1)?;
            b.se()?;
            b.se()?;
            let n = b.ue()?;
            ensure!(n < 256, "invalid POC cycle");
            for _ in 0..n {
                b.se()?;
            }
        }
        2 => {}
        _ => anyhow::bail!("invalid SPS POC type"),
    }
    b.ue()?;
    b.bits(1)?;
    let coded_width = (b.ue()? + 1) * 16;
    let map_height = (b.ue()? + 1) * 16;
    let frame_only = b.flag()?;
    if !frame_only {
        b.bits(1)?;
    }
    b.bits(1)?;
    let crop = if b.flag()? {
        [b.ue()?, b.ue()?, b.ue()?, b.ue()?]
    } else {
        [0; 4]
    };
    ensure!(chroma == 1, "CPU output must remain 4:2:0");
    let frame_factor = if frame_only { 1 } else { 2 };
    let width = coded_width
        .checked_sub((crop[0] + crop[1]) * 2)
        .context("invalid SPS horizontal crop")?;
    let height = (map_height * frame_factor)
        .checked_sub((crop[2] + crop[3]) * 2 * frame_factor)
        .context("invalid SPS vertical crop")?;
    if b.flag()? {
        if b.flag()? && b.bits(8)? == 255 {
            b.bits(32)?;
        }
        if b.flag()? {
            b.bits(1)?;
        }
        ensure!(
            !b.flag()?,
            "SPS introduced video_signal_type/colour description"
        );
    }
    Ok((width, height))
}

fn run_cpu_bench(
    device: &ID3D11Device,
    args: &[String],
    source: (u32, u32),
    output: Option<&PathBuf>,
) -> Result<()> {
    let (width, height) = source;
    let (mut out_width, mut out_height) = size_argument(args, "--output-size")?.unwrap_or(source);
    let mut fps: u32 = argument(args, "--fps")?.unwrap_or("60").parse()?;
    let frames: usize = argument(args, "--frames")?.unwrap_or("180").parse()?;
    let warmup: usize = argument(args, "--warmup")?.unwrap_or("30").parse()?;
    let content = argument(args, "--content")?.unwrap_or("dirty");
    if matches!(content, "static" | "cursor") {
        return run_unchanged_bench(device, source, (out_width, out_height), frames, content);
    }
    let pipeline = args.iter().any(|a| a == "--pipeline");
    let shared_pipeline = args.iter().any(|a| a == "--pipeline-shared-device");
    ensure!(
        !shared_pipeline || pipeline,
        "--pipeline-shared-device requires --pipeline"
    );
    let adaptive = args.iter().any(|a| a == "--adaptive");
    let paced = args.iter().any(|a| a == "--paced");
    ensure!(
        !(adaptive && pipeline),
        "adaptive pipeline experiment is not implemented"
    );
    use bubbleplay_host::video_encoder::cpu_policy::{CpuPacing, CpuTarget};
    let requested = CpuTarget {
        width: out_width,
        height: out_height,
        fps,
    };
    let mut pacing = CpuPacing::from_env(Instant::now());
    let mut target = if adaptive {
        pacing.target(source, requested, Instant::now())
    } else {
        requested
    };
    (out_width, out_height, fps) = (target.width, target.height, target.fps);
    ensure!(
        !pipeline || args.iter().any(|a| a == "--warp"),
        "pipeline experiment uses WARP devices"
    );
    ensure!(
        frames > 0 && width >= 128 && height >= 192 && matches!(content, "dirty" | "full"),
        "bench needs positive frames, source >=128x192, content dirty/full"
    );
    let texture = fixture(device, width, height)?;
    let context = unsafe { device.GetImmediateContext() }?;
    let gate = gpu_gate::for_device(device);
    let init_started = Instant::now();
    let mut encoder = MfEncoder::new_with_gop(
        device.as_raw(),
        out_width,
        out_height,
        fps,
        6_000_000,
        !args.iter().any(|a| a == "--finite-gop-request"),
        false,
        2,
    )?;
    ensure!(
        encoder.infinite_gop(),
        "software must keep IDRs host-owned even for a finite-GOP request"
    );
    println!(
        "CPU_INIT elapsed_ms={:.3} intra_refresh_status={:?} tuning={:?}",
        init_started.elapsed().as_secs_f64() * 1000.0,
        encoder.software_intra_refresh_status()?,
        encoder.software_tuning_report()
    );
    let mut timings = Vec::with_capacity(frames);
    let mut intervals = Vec::with_capacity(frames);
    // Build the captured-like sequence outside the measured region. Both the
    // serial and pipelined experiments replay exactly the same 36-frame cycle.
    let pixels = std::sync::Arc::new(
        (0..36)
            .map(|n| bench_pixels(width, height, n, content == "full"))
            .collect::<Vec<_>>(),
    );
    // This checks the exact conversion bytes before lossy H.264, for each
    // measured size/matrix/configuration; decode error is a separate property.
    {
        use bubbleplay_host::mf_color::{bgra_to_nv12_scaled, CpuConversionOptions};
        let mut reference = Vec::new();
        let mut candidate = Vec::new();
        bgra_to_nv12_scaled(
            &pixels[0],
            width as usize * 4,
            width as usize,
            height as usize,
            out_width as usize,
            out_height as usize,
            CpuConversionOptions {
                simd: false,
                damage: false,
                threads: 1,
            },
            &mut reference,
        )?;
        bgra_to_nv12_scaled(
            &pixels[0],
            width as usize * 4,
            width as usize,
            height as usize,
            out_width as usize,
            out_height as usize,
            CpuConversionOptions::from_env(),
            &mut candidate,
        )?;
        ensure!(candidate == reference, "CPU scalar/SIMD colour mismatch");
        println!("PASS CPU_COLOUR exact_scalar_parity=true size={out_width}x{out_height} limited=true matrix={:?}",
            bubbleplay_host::mf_color::matrix_for_nvenc(out_width,out_height));
    }
    let mut csv = String::from("frame,width,height,fps,readback_us,cpu_conversion_us,conversion_readback_us,input_pack_us,mft_encode_us,output_processing_us,total_us,completed_interval_us,idr\n");
    let mut stream = Vec::new();
    std::thread::scope(|scope| -> Result<()> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let worker = if pipeline {
            let pixels = pixels.clone();
            let capture_device = device.clone();
            Some(scope.spawn(move || -> Result<()> {
                use bubbleplay_host::mf_color::CpuConverter;
                let source_device = if shared_pipeline {
                    capture_device
                } else {
                    create_device(true, false)?
                };
                let source_texture = fixture(&source_device, width, height)?;
                let source_context = unsafe { source_device.GetImmediateContext() }?;
                let source_gate = gpu_gate::for_device(&source_device);
                let mut converter = CpuConverter::new(&source_device, width, height)?;
                let mut nv12 = Vec::new();
                for frame in 0..warmup + frames {
                    {
                        let _gpu = source_gate.lock();
                        unsafe {
                            source_context.UpdateSubresource(
                                &source_texture,
                                0,
                                None,
                                pixels[frame % 36].as_ptr().cast(),
                                width * 4,
                                0,
                            )
                        };
                    }
                    let damage = bench_damage(width, height, frame);
                    let timing = converter.convert_frame(
                        &source_texture,
                        out_width,
                        out_height,
                        (frame > 0 && content == "dirty").then_some(damage.as_slice()),
                        &mut nv12,
                    )?;
                    if sender.send((nv12.clone(), timing)).is_err() {
                        return Ok(());
                    }
                }
                Ok(())
            }))
        } else {
            drop(sender);
            None
        };
        let mut completed = Instant::now();
        let mut admission = completed;
        let measured = (|| -> Result<()> {
            for frame in 0..warmup + frames {
                if adaptive {
                    let next = pacing.target(source, requested, Instant::now());
                    if next != target {
                        encoder.reconfigure_profile(next.width, next.height, next.fps)?;
                        println!("CPU_ADAPT frame={frame} previous={target:?} next={next:?} forces_IDR=true");
                        target = next;
                        (out_width, out_height, fps) = (next.width, next.height, next.fps);
                    }
                }
                if paced {
                    let now = Instant::now();
                    if admission > now {
                        std::thread::sleep(admission.duration_since(now));
                    }
                }
                let frame_started = Instant::now();
                if !pipeline {
                    {
                        let _gpu = gate.lock();
                        unsafe {
                            context.UpdateSubresource(
                                &texture,
                                0,
                                None,
                                pixels[frame % 36].as_ptr().cast(),
                                width * 4,
                                0,
                            )
                        };
                    }
                }
                let forced = frame == 0
                    || frame == warmup
                    || frame == warmup + frames / 2
                    || encoder.idr_pending();
                if forced {
                    encoder.force_idr();
                }
                let damage = bench_damage(width, height, frame);
                let mut conversion = None;
                let encoded = if pipeline {
                    let (bytes, t) = receiver.recv().context("conversion pipeline ended early")?;
                    conversion = Some(t);
                    encoder.encode_nv12(&bytes)?
                } else {
                    encoder.encode_texture_with_damage(
                        texture.as_raw(),
                        (frame > 0 && content == "dirty").then_some(damage.as_slice()),
                    )?
                };
                let finished = Instant::now();
                let interval_us = finished.duration_since(completed).as_micros() as u64;
                completed = finished;
                validate_bench_au(&encoded.data, forced, (out_width, out_height))?;
                if adaptive {
                    pacing.observe(encoder.last_frame_timings().total_us, target, finished);
                }
                let period = std::time::Duration::from_secs_f64(1.0 / fps as f64);
                let scheduled = admission + period;
                admission = if scheduled + period < frame_started {
                    frame_started + period
                } else {
                    scheduled
                };
                if frame >= warmup {
                    let mut t = encoder.last_frame_timings();
                    if let Some(c) = conversion {
                        t.readback_us = c.readback_us;
                        t.cpu_conversion_us = c.conversion_us;
                        t.conversion_readback_us += c.readback_us + c.conversion_us;
                        t.total_us += c.readback_us + c.conversion_us;
                    }
                    csv.push_str(&format!(
                    "{frame},{out_width},{out_height},{fps},{},{},{},{},{},{},{},{interval_us},{}\n",
                        t.readback_us,
                        t.cpu_conversion_us,
                        t.conversion_readback_us,
                        t.input_pack_us,
                        t.mft_encode_us,
                        t.output_processing_us,
                        t.total_us,
                        encoded.is_idr
                    ));
                    timings.push(t);
                    intervals.push(MfFrameTimings {
                        total_us: interval_us,
                        ..Default::default()
                    });
                    stream.extend_from_slice(&encoded.data);
                }
            }
            Ok(())
        })();
        drop(receiver);
        if let Some(worker) = worker {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("conversion worker panicked"))??;
        }
        measured
    })?;
    println!("PASS CPU_BENCH source={width}x{height} output={out_width}x{out_height} fps={fps} frames={frames} warmup={warmup} content={content} pipeline={pipeline} converter=\"{}\" High=100 AnnexB=true AU_per_frame=1 B=0 forced_IDR_only=true SPS_PPS_every_IDR=true", encoder.converter_name());
    println!("CPU_THROUGHPUT completed_interval={} fixture_generation_excluded=true texture_upload_included=true pipeline_devices=\"{}\"",timing_summary(&intervals,|t|t.total_us),if shared_pipeline {"shared WARP capture/encoder device and gate"} else if pipeline {"separate WARP capture device"} else {"encoder/capture shared device"});
    println!("CPU_TIMINGS readback={} cpu_conversion={} conversion_readback={} packing={} mft={} output={} total={}",
        timing_summary(&timings, |t|t.readback_us), timing_summary(&timings, |t|t.cpu_conversion_us),
        timing_summary(&timings, |t|t.conversion_readback_us), timing_summary(&timings, |t|t.input_pack_us),
        timing_summary(&timings, |t|t.mft_encode_us), timing_summary(&timings, |t|t.output_processing_us),
        timing_summary(&timings, |t|t.total_us));
    if let Some(dir) = output {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("cpu-timings.csv"), csv)?;
        std::fs::write(dir.join("cpu-stream.h264"), stream)?;
    }
    Ok(())
}

/// Isolated inbox-MFT experiment. This deliberately owns its transform so the
/// property is set before streaming without adding a production encoder knob.
fn run_intra_refresh_bench(
    device: &ID3D11Device,
    args: &[String],
    size: (u32, u32),
    output: Option<&PathBuf>,
    refresh: u32,
) -> Result<()> {
    use bubbleplay_host::mf_color::{bgra_to_nv12_scaled, CpuConversionOptions, CpuConverter};
    use std::mem::ManuallyDrop;
    use windows::core::GUID;
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Variant::VARIANT;

    let (width, height) = size;
    let frames: usize = argument(args, "--frames")?.unwrap_or("360").parse()?;
    let content = argument(args, "--content")?.unwrap_or("full");
    ensure!(
        frames > 1 && height >= 192 && matches!(content, "full" | "dirty" | "colour"),
        "intra-refresh experiment needs >1 frames, height>=192, content full/dirty/colour"
    );
    ensure!(
        !args
            .iter()
            .any(|a| matches!(a.as_str(), "--adaptive" | "--pipeline" | "--output-size")),
        "intra-refresh experiment uses fixed geometry and serial conversion"
    );
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _apartment = Apartment;
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
    struct Foundation;
    impl Drop for Foundation {
        fn drop(&mut self) {
            let _ = unsafe { MFShutdown() };
        }
    }
    let _foundation = Foundation;
    let encoder: IMFTransform = unsafe {
        CoCreateInstance(
            &GUID::from_u128(0x6ca50344_051a_4ded_9779_a43305165e35),
            None,
            CLSCTX_INPROC_SERVER,
        )?
    };
    let codec: ICodecAPI = encoder.cast()?;
    unsafe { encoder.GetAttributes()?.SetUINT32(&MF_LOW_LATENCY, 1)? };
    for (key, value) in [
        (
            CODECAPI_AVEncCommonRateControlMode,
            VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
        ),
        (CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32)),
        (CODECAPI_AVLowLatencyMode, VARIANT::from(true)),
        (CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(6_000_000u32)),
        (CODECAPI_AVEncCommonBufferSize, VARIANT::from(12_500u32)),
        (CODECAPI_AVEncMPVGOPSize, VARIANT::from(u32::MAX)),
        (CODECAPI_AVEncMPVSceneDetection, VARIANT::from(0u32)),
        (CODECAPI_AVEncH264CABACEnable, VARIANT::from(true)),
        (CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(0u32)),
    ] {
        let result = unsafe { codec.SetValue(&key, &value) };
        if key == CODECAPI_AVEncMPVDefaultBPictureCount || key == CODECAPI_AVEncMPVSceneDetection {
            // Production also treats these two requests as optional; the
            // bitstream checks still require no B pictures or unsolicited IDRs.
            if let Err(error) = result {
                println!("INTRA_OPTIONAL key={key:?} result={error:?}");
            }
        } else {
            result.with_context(|| format!("intra experiment setting {key:?}"))?;
        }
    }
    let media_type = |subtype: &GUID| -> Result<IMFMediaType> {
        let media = unsafe { MFCreateMediaType()? };
        unsafe {
            media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            media.SetGUID(&MF_MT_SUBTYPE, subtype)?;
            media.SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(width) << 32) | u64::from(height),
            )?;
            media.SetUINT64(&MF_MT_FRAME_RATE, (60u64 << 32) | 1)?;
            media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        }
        Ok(media)
    };
    let out_type = media_type(&MFVideoFormat_H264)?;
    let in_type = media_type(&MFVideoFormat_NV12)?;
    unsafe {
        out_type.SetUINT32(&MF_MT_AVG_BITRATE, 6_000_000)?;
        out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, 100)?;
        encoder.SetOutputType(0, &out_type, 0)?;
        in_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, width)?;
        in_type.SetUINT32(&MF_MT_SAMPLE_SIZE, width * height * 3 / 2)?;
        in_type.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        in_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        in_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        in_type.SetUINT32(
            &MF_MT_YUV_MATRIX,
            if width <= 768 && height <= 576 {
                MFVideoTransferMatrix_BT601.0 as u32
            } else {
                MFVideoTransferMatrix_BT709.0 as u32
            },
        )?;
        encoder.SetInputType(0, &in_type, 0)?;
    }
    let requested = VARIANT::from(refresh); // VT_UI4, including explicit baseline 0.
    let set_hr = unsafe {
        (Interface::vtable(&codec).SetValue)(
            codec.as_raw(),
            &CODECAPI_AVEncVideoGradualIntraRefresh,
            &requested,
        )
    };
    let readback = unsafe { codec.GetValue(&CODECAPI_AVEncVideoGradualIntraRefresh) };
    println!("INTRA_REFRESH requested={refresh} SetValue={set_hr:?} GetValue={readback:?} before_streaming=true");
    unsafe {
        encoder.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        encoder.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
    }
    let texture = fixture(device, width, height)?;
    let context = unsafe { device.GetImmediateContext()? };
    let gate = gpu_gate::for_device(device);
    let mut converter = CpuConverter::new(device, width, height)?;
    let pixels: Vec<_> = (0..36)
        .map(|n| bench_pixels(width, height, n, content == "full"))
        .collect();
    if content != "colour" {
        let mut scalar = Vec::new();
        let mut simd = Vec::new();
        for (options, bytes) in [
            (
                CpuConversionOptions {
                    simd: false,
                    damage: false,
                    threads: 1,
                },
                &mut scalar,
            ),
            (CpuConversionOptions::from_env(), &mut simd),
        ] {
            bgra_to_nv12_scaled(
                &pixels[0],
                width as usize * 4,
                width as usize,
                height as usize,
                width as usize,
                height as usize,
                options,
                bytes,
            )?;
        }
        ensure!(scalar == simd, "intra-refresh scalar/SIMD colour mismatch");
    }
    let mut nv12 = Vec::new();
    let mut sizes = Vec::new();
    let mut idr_sizes = Vec::new();
    let mut p_sizes = Vec::new();
    let mut stream = Vec::new();
    let mut csv = String::from("frame,bytes,idr,i_slices,p_slices\n");
    let mut non_idr_i_frames = Vec::new();
    let (mut i_total, mut p_total) = (0usize, 0usize);
    for frame in 0..frames {
        if content != "colour" {
            let _gpu = gate.lock();
            unsafe {
                context.UpdateSubresource(
                    &texture,
                    0,
                    None,
                    pixels[frame % 36].as_ptr().cast(),
                    width * 4,
                    0,
                )
            };
        }
        let damage = bench_damage(width, height, frame);
        converter.convert_frame(
            &texture,
            width,
            height,
            (content == "dirty" && frame > 0).then_some(damage.as_slice()),
            &mut nv12,
        )?;
        let sample = unsafe { MFCreateSample()? };
        let buffer = unsafe { MFCreateMemoryBuffer(nv12.len() as u32)? };
        let mut ptr = std::ptr::null_mut();
        unsafe {
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12.len() as u32)?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(frame as i64 * 10_000_000 / 60)?;
            sample.SetSampleDuration(10_000_000 / 60)?;
        }
        let forced = frame == 0 || frame == frames / 2;
        if forced {
            unsafe { codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32))? };
        }
        unsafe { encoder.ProcessInput(0, &sample, 0)? };
        let info = unsafe { encoder.GetOutputStreamInfo(0)? };
        let out_sample = unsafe { MFCreateSample()? };
        let capacity = info.cbSize.max(width * height * 2).max(65536);
        let out_buffer = unsafe {
            if info.cbAlignment > 1 {
                MFCreateAlignedMemoryBuffer(capacity, info.cbAlignment - 1)?
            } else {
                MFCreateMemoryBuffer(capacity)?
            }
        };
        unsafe { out_sample.AddBuffer(&out_buffer)? };
        let mut out = MFT_OUTPUT_DATA_BUFFER {
            pSample: ManuallyDrop::new(Some(out_sample)),
            ..Default::default()
        };
        let mut status = 0;
        let result =
            unsafe { encoder.ProcessOutput(0, std::slice::from_mut(&mut out), &mut status) };
        let sample = unsafe { ManuallyDrop::take(&mut out.pSample) };
        unsafe { ManuallyDrop::drop(&mut out.pEvents) };
        result?;
        ensure!(
            out.dwStatus & MFT_OUTPUT_DATA_BUFFER_INCOMPLETE.0 as u32 == 0,
            "incomplete AU"
        );
        let sample = sample.context("missing encoded sample")?;
        ensure!(
            unsafe { sample.GetSampleTime()? } == frame as i64 * 10_000_000 / 60,
            "reordered output"
        );
        let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
        let mut len = 0;
        unsafe { buffer.Lock(&mut ptr, None, Some(&mut len))? };
        let mut data = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
        unsafe { buffer.Unlock()? };
        let inspection = inspect_access_unit(&data)?;
        if inspection.is_idr && !(inspection.sps_before_idr && inspection.pps_before_idr) {
            let media = unsafe { encoder.GetOutputCurrentType(0)? };
            let mut sequence =
                vec![0; unsafe { media.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER)? } as usize];
            unsafe { media.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut sequence, None)? };
            let nals = annex_b_nals(&data)?;
            let insert = if nals[0][0] & 31 == 9 {
                nals[0].as_ptr() as usize - data.as_ptr() as usize + nals[0].len()
            } else {
                0
            };
            data.splice(insert..insert, sequence);
        }
        validate_bench_au(&data, forced, size)?;
        let (mut i, mut p) = (0usize, 0usize);
        for nal in annex_b_nals(&data)? {
            if matches!(nal[0] & 31, 1 | 5) {
                let mut bits = SpsBits::new(nal);
                bits.ue()?;
                match bits.ue()? % 5 {
                    0 => p += 1,
                    2 => i += 1,
                    _ => anyhow::bail!("unexpected slice type"),
                }
            }
        }
        if !forced && i > 0 {
            non_idr_i_frames.push(frame);
        }
        i_total += i;
        p_total += p;
        sizes.push(data.len());
        if forced {
            idr_sizes.push(data.len());
        } else {
            p_sizes.push(data.len());
        }
        csv.push_str(&format!("{frame},{},{forced},{i},{p}\n", data.len()));
        if frame == 0 {
            if let Some(dir) = output {
                std::fs::create_dir_all(dir)?;
                std::fs::write(dir.join("intra-first-au.h264"), &data)?;
            }
        }
        stream.extend_from_slice(&data);
    }
    sizes.sort_unstable();
    println!("PASS INTRA_BENCH value={refresh} content={content} frames={frames} AU_mean={:.3} AU_p95={} AU_max={} IDR_sizes={idr_sizes:?} P_max={} I_slices={i_total} P_slices={p_total} non_IDR_I_frames={non_idr_i_frames:?} High=100 AnnexB=true AU_per_frame=1 B=0 SPS_PPS_every_IDR=true colour_scalar_parity={} macroblocks_parsed=false",
        sizes.iter().sum::<usize>() as f64 / frames as f64, sizes[(frames * 95).div_ceil(100)-1], sizes[frames-1], p_sizes.iter().max().unwrap_or(&0), content != "colour");
    println!("INTRA_REFRESH after_streaming_GetValue={:?}", unsafe {
        codec.GetValue(&CODECAPI_AVEncVideoGradualIntraRefresh)
    });
    if let Some(dir) = output {
        std::fs::write(dir.join("intra-sizes.csv"), csv)?;
        std::fs::write(dir.join("intra-stream.h264"), stream)?;
    }
    Ok(())
}

fn run_unchanged_bench(
    device: &ID3D11Device,
    source: (u32, u32),
    output: (u32, u32),
    frames: usize,
    content: &str,
) -> Result<()> {
    use bubbleplay_host::mf_color::CpuConverter;
    let texture = fixture(device, source.0, source.1)?;
    let mut converter = CpuConverter::new(device, source.0, source.1)?;
    let mut nv12 = Vec::new();
    converter.convert_frame(&texture, output.0, output.1, None, &mut nv12)?;
    let before = nv12.clone();
    let mut times = Vec::new();
    for _ in 0..frames {
        let start = Instant::now();
        let t = converter.convert_frame(&texture, output.0, output.1, Some(&[]), &mut nv12)?;
        let elapsed_us = start.elapsed().as_micros() as u64;
        ensure!(
            t.readback_us == 0 && t.conversion_us == 0 && nv12 == before,
            "unchanged frame was converted"
        );
        times.push(MfFrameTimings {
            total_us: elapsed_us,
            ..Default::default()
        });
    }
    ensure!(!times.is_empty(), "positive --frames required");
    println!("PASS CPU_UNCHANGED content={content} repeats={frames} readbacks=0 conversions=0 source=DDA-metadata-simulation capture_calls=0 actual_cursor_injection=false timings={}",timing_summary(&times,|t|t.total_us));
    Ok(())
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
            &mut String::new(),
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
    cpu_environment(&args)?;
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
    if let Some(value) = argument(&args, "--intra-refresh")? {
        ensure!(
            software_only && warp,
            "--intra-refresh requires --software-only --warp"
        );
        return run_intra_refresh_bench(
            &device,
            &args,
            (width, height),
            output.as_ref(),
            value.parse()?,
        );
    }
    if args.iter().any(|a| a == "--bench") {
        ensure!(software_only, "--bench requires --software-only");
        return run_cpu_bench(&device, &args, (width, height), output.as_ref());
    }
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
