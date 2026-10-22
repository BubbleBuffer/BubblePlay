//! Ignored end-to-end test: decode the full spike streams on hardware and draw
//! every output through `VideoRenderer`. Run with `cargo test media -- --ignored`.

use super::*;
use crate::shell::{Rect, VideoLayer};
use anyhow::{bail, ensure, Context, Result};
use std::{
    collections::BTreeSet,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::Interface,
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
            Direct3D11::*,
            Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
        },
    },
};

const FRAMES: usize = 1200;

/// Splits an Annex-B byte stream at access-unit delimiters, as the spike did.
fn access_units(data: &[u8], codec: Codec) -> Result<Vec<&[u8]>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 4 < data.len() {
        let prefix = if data[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        let nal = data[i + prefix];
        let aud = match codec {
            Codec::H264 => nal & 31 == 9,
            Codec::Hevc => (nal >> 1) & 63 == 35,
        };
        if aud {
            starts.push(i);
        }
        i += prefix + 1;
    }
    if starts.is_empty() {
        bail!("stream has no access-unit delimiters");
    }
    starts[0] = 0;
    starts.push(data.len());
    Ok(starts.windows(2).map(|r| &data[r[0]..r[1]]).collect())
}

/// Own device for the test: video support, BGRA, multithread protected, as `Shell::device` provides.
fn test_device() -> Result<ID3D11Device> {
    let mut device = None;
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
            None,
        )?;
    }
    let device = device.context("D3D11CreateDevice returned no device")?;
    let context = unsafe { device.GetImmediateContext()? };
    let multithread: ID3D11Multithread = context.cast()?;
    unsafe {
        let _ = multithread.SetMultithreadProtected(true);
    }
    Ok(device)
}

/// Offscreen BGRA render target standing in for the swap-chain back buffer.
fn render_target(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11RenderTargetView> {
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
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..Default::default()
    };
    let mut texture = None;
    let mut rtv = None;
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut texture))?;
        let texture: ID3D11Texture2D = texture.context("no render target texture")?;
        device.CreateRenderTargetView(&texture, None, Some(&mut rtv))?;
    }
    rtv.context("no render target view")
}

/// Decodes one whole stream on hardware, pushing each output through the mailbox
/// and drawing it. Fails on any decoder error, a duplicate or missing PTS, or a
/// missing drain.
fn decode_stream(runtime_device: &ID3D11Device, codec: Codec, path: &str) -> Result<()> {
    let data = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    let units = access_units(&data, codec)?;
    ensure!(
        units.len() == FRAMES,
        "{path}: expected {FRAMES} access units, found {}",
        units.len()
    );

    let mut backend = MfBackend::new(codec, runtime_device, 1920, 1080)?;
    println!("backend: {}", backend.description());
    let stats = MediaStats::new();
    let mut renderer = VideoRenderer::new(runtime_device, stats.clone(), None)?;
    let sink = renderer.sink();
    let context = unsafe { runtime_device.GetImmediateContext()? };
    let rtv = render_target(runtime_device, 1920, 1080)?;
    let viewport = Rect {
        x: 0.0,
        y: 0.0,
        w: 1920.0,
        h: 1080.0,
    };

    let deadline = Instant::now() + Duration::from_secs(120);
    let mut fed = 0usize;
    let mut drained_requested = false;
    let mut outputs = BTreeSet::new();
    let mut first_size = None;
    while !(drained_requested && backend.is_drained()) {
        if Instant::now() > deadline {
            bail!(
                "{path}: timed out; fed={fed} outputs={} drain_requested={drained_requested}",
                outputs.len()
            );
        }
        let mut progressed = false;
        if fed < units.len() {
            if fed < 4 || fed.is_multiple_of(300) {
                println!(
                    "{} progress: fed={fed}, outputs={}",
                    codec.name(),
                    outputs.len()
                );
            }
            let pts = fed as i64 * 10_000_000 / 60;
            if backend.submit(units[fed], pts)? {
                fed += 1;
                progressed = true;
            }
        } else if !drained_requested {
            backend.drain()?;
            drained_requested = true;
            progressed = true;
        }
        while let Some(frame) = backend.poll()? {
            progressed = true;
            ensure!(
                outputs.insert(frame.pts_100ns),
                "{path}: duplicate output PTS {}",
                frame.pts_100ns
            );
            ensure!(
                (frame.width, frame.height) == (1920, 1080),
                "{path}: expected visible 1920x1080, got {}x{}",
                frame.width,
                frame.height
            );
            ensure!(
                (
                    frame.source_rect.left,
                    frame.source_rect.top,
                    frame.source_rect.right,
                    frame.source_rect.bottom
                ) == (0, 0, 1920, 1080),
                "{path}: unexpected visible source crop {:?}",
                frame.source_rect
            );
            if first_size.is_none() {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { frame.texture.GetDesc(&mut desc) };
                println!(
                    "{path}: visible={}x{}, source={:?}, allocation={}x{}",
                    frame.width, frame.height, frame.source_rect, desc.Width, desc.Height
                );
            }
            first_size.get_or_insert((frame.width, frame.height));
            sink.push(frame);
            renderer.draw(&context, &rtv, viewport);
            // A real swap-chain Present submits this work. This offscreen
            // target has no Present, so submit explicitly before decoder
            // surfaces can be reclaimed through the renderer's GPU fence.
            unsafe {
                context.Flush();
            }
            // The shell has an independent UI thread to retire leases. This
            // single-threaded test must retire them before the next MF call,
            // otherwise synchronous ProcessInput can wait on its own pool.
            while renderer.gpu_work_pending() {
                ensure!(
                    Instant::now() < deadline,
                    "{path}: GPU blit did not complete before deadline"
                );
                thread::sleep(Duration::from_micros(200));
            }
            if let Some(error) = renderer.last_error() {
                bail!("{path}: draw failed: {error}");
            }
        }
        if !progressed {
            thread::sleep(Duration::from_micros(200));
        }
    }

    let expected: BTreeSet<i64> = (0..FRAMES as i64).map(|i| i * 10_000_000 / 60).collect();
    ensure!(
        outputs == expected,
        "{path}: outputs do not match inputs: got {} unique, expected {}",
        outputs.len(),
        expected.len()
    );
    let totals = stats.snapshot(Instant::now()).totals;
    ensure!(
        totals.decode_errors == 0,
        "decode errors: {}",
        totals.decode_errors
    );
    ensure!(
        totals.present_errors == 0,
        "present errors: {}",
        totals.present_errors
    );
    ensure!(
        totals.decoded == FRAMES as u64,
        "decoded={}",
        totals.decoded
    );
    ensure!(
        totals.presented == FRAMES as u64,
        "presented={}",
        totals.presented
    );
    ensure!(totals.dropped == 0, "dropped={}", totals.dropped);
    println!(
        "{path}: {} outputs, first size {:?}, {}",
        outputs.len(),
        first_size,
        stats.snapshot(Instant::now())
    );
    Ok(())
}

#[test]
#[ignore = "needs F:/build/spike-streams and a hardware D3D11 video decoder"]
fn decodes_full_spike_streams_end_to_end() -> Result<()> {
    let _runtime = MediaRuntime::new()?;
    let device = test_device()?;
    let cases = [
        (Codec::H264, "F:/build/spike-streams/h264.h264"),
        (Codec::Hevc, "F:/build/spike-streams/hevc.hevc"),
    ];
    for (codec, path) in cases {
        if !Path::new(path).exists() {
            println!("skipped {path}: not present");
            continue;
        }
        decode_stream(&device, codec, path)?;
    }
    Ok(())
}
