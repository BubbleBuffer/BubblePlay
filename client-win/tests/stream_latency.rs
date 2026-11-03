//! Latency of the Flutter texture path on Windows, headless (no window is ever created).
//!
//! Two ignored tests, run with `cargo test --test stream_latency -- --ignored --nocapture`:
//!
//! * `handoff_1080p_300_frames`: the cross-device hand-off alone, 300 frames at 1920x1080 on two
//!   D3D11 devices: draw + Flush on the producer, then open the shared handle and read the first
//!   pixel on the second device (the way Flutter's engine reads it).
//! * `stream_session_against_fake_host`: a real `StreamSession` fed by the loopback fake host
//!   (`BUBBLEPLAY_FAKE_HOST_EXE` = a built `fake-wss-host.exe`, recordings in
//!   `BUBBLEPLAY_STREAMS_DIR`, default `F:/build/spike-streams`), about 20 s of 1080p60 H.264.
//!   Prints the session's own decode-to-present figures plus the time from "frame published" to
//!   "first pixel read on a second device".
#![cfg(windows)]

use std::{
    io::{BufRead, BufReader},
    os::windows::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

use bp_device_key::DeviceIdentity;
use bubbleplay_client::{
    model::StreamMode,
    net::ViewerAuth,
    stream::{
        player::{create_device, StreamParams, StreamSession},
        target::SharedPair,
    },
};
use windows::Win32::{
    Foundation::HANDLE,
    Graphics::{
        Direct3D11::{
            ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX, D3D11_CPU_ACCESS_READ,
            D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
        },
        Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
    },
};

fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let rank = (p * (sorted.len() - 1) as f64).round() as usize;
    sorted[rank]
}

fn report(label: &str, values_ms: &[f64]) {
    println!(
        "  {label:<44} n={:<4} p50={:>8.3} ms  p95={:>8.3} ms  max={:>8.3} ms",
        values_ms.len(),
        percentile(values_ms, 0.50),
        percentile(values_ms, 0.95),
        values_ms.iter().cloned().fold(f64::NAN, f64::max),
    );
}

fn ms(a: Instant, b: Instant) -> f64 {
    b.duration_since(a).as_secs_f64() * 1000.0
}

/// A 1x1 CPU-readable staging texture on `device`, reused for every read.
struct PixelReader {
    ctx: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
}

impl PixelReader {
    fn new(device: &ID3D11Device) -> Self {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 1,
            Height: 1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap()
        };
        Self {
            ctx: unsafe { device.GetImmediateContext().unwrap() },
            staging: staging.unwrap(),
        }
    }

    /// Copies the texture's first pixel to the staging texture and maps it. The map blocks until
    /// the GPU has finished every write the copy depends on.
    fn first_pixel(&self, texture: &ID3D11Texture2D) -> [u8; 4] {
        let corner = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: 1,
            bottom: 1,
            back: 1,
        };
        unsafe {
            self.ctx
                .CopySubresourceRegion(&self.staging, 0, 0, 0, 0, texture, 0, Some(&corner));
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let pixel = *(mapped.pData as *const [u8; 4]);
            self.ctx.Unmap(&self.staging, 0);
            pixel
        }
    }
}

fn open_shared(device: &ID3D11Device, handle: HANDLE) -> ID3D11Texture2D {
    let mut opened: Option<ID3D11Texture2D> = None;
    unsafe { device.OpenSharedResource(handle, &mut opened) }.expect("open the shared handle");
    opened.expect("opened texture")
}

/// One pass of the hand-off loop. `reopen` opens the shared handle again on every frame (the
/// worst case); otherwise each of the two surfaces is opened once and kept (what a consumer
/// that caches its imported textures does).
fn handoff_pass(reopen: bool, frames: usize) {
    const W: u32 = 1920;
    const H: u32 = 1080;
    let producer = create_device().expect("producer device");
    let consumer = create_device().expect("consumer device");
    let ctx = unsafe { producer.GetImmediateContext().unwrap() };
    let reader = PixelReader::new(&consumer);
    let mut pair = SharedPair::new(&producer, W, H).unwrap();
    let handles = pair.handles();
    let mut cached: [Option<ID3D11Texture2D>; 2] = [None, None];

    let warmup = 20;
    let (mut draw, mut open, mut read, mut total) = (vec![], vec![], vec![], vec![]);
    let mut mismatches = 0;
    for n in 0..frames + warmup {
        // Every frame a different colour, so a stale or half-visible frame is detected.
        let c = (n % 251) as u8;
        let color = [
            c as f32 / 255.0,
            (255 - c) as f32 / 255.0,
            c as f32 / 255.0,
            1.0,
        ];
        let expected = [c, 255 - c, c, 255]; // BGRA

        let t0 = Instant::now();
        let handle = pair.back().handle();
        unsafe { ctx.ClearRenderTargetView(pair.back().rtv(), &color) };
        pair.finish(&ctx); // includes the Flush
        let t1 = Instant::now();
        let slot = usize::from(handle != handles[0]);
        if reopen || cached[slot].is_none() {
            cached[slot] = Some(open_shared(&consumer, handle));
        }
        let t2 = Instant::now();
        let pixel = reader.first_pixel(cached[slot].as_ref().unwrap());
        let t3 = Instant::now();

        if n >= warmup {
            draw.push(ms(t0, t1));
            open.push(ms(t1, t2));
            read.push(ms(t2, t3));
            total.push(ms(t0, t3));
            if pixel != expected {
                mismatches += 1;
            }
        }
    }
    println!(
        "hand-off, 1920x1080 BGRA, {frames} frames, shared handle {}:",
        if reopen {
            "re-opened every frame"
        } else {
            "opened once per surface"
        }
    );
    report("draw (clear) + Flush on producer", &draw);
    report("OpenSharedResource on second device", &open);
    report("copy first pixel + Map (waits for GPU)", &read);
    report("total: before draw -> first pixel read", &total);
    println!("  pixel mismatches: {mismatches} of {frames}");
    assert_eq!(
        mismatches, 0,
        "a frame was not visible on the second device"
    );
}

#[test]
#[ignore = "latency measurement; run with --ignored --nocapture"]
fn handoff_1080p_300_frames() {
    handoff_pass(true, 300);
    handoff_pass(false, 300);
}

// ----- the session against the fake host -----

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The fake host's PC seed (test-only key `handshake.pc` of fixtures/device-keys-contract.json).
const PC_SEED_HEX: &str = "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb";

fn hex32(text: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap();
    }
    out
}

fn start_fake_host(seconds: u64) -> (OwnedChild, String) {
    let exe = std::env::var_os("BUBBLEPLAY_FAKE_HOST_EXE")
        .expect("set BUBBLEPLAY_FAKE_HOST_EXE to a built fake-wss-host.exe");
    let streams = std::env::var("BUBBLEPLAY_STREAMS_DIR")
        .unwrap_or_else(|_| "F:/build/spike-streams".to_owned());
    let mut child = Command::new(exe)
        .args([
            "--stream",
            "h264",
            "--streams-dir",
            &streams,
            "--exit-after",
            &seconds.to_string(),
            "--port",
            "0",
        ])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch the fake host");
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut first = true;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if first {
                first = false;
                let _ = tx.send(line);
            }
        }
    });
    let owned = OwnedChild(child);
    // The host loads its recordings before it binds; allow for that.
    let line = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the fake host announces its listener");
    let address = line
        .strip_prefix("fake-wss-host: ")
        .and_then(|rest| rest.split_whitespace().next())
        .expect("listener address in the announcement")
        .trim_end_matches("/ws")
        .to_owned();
    (owned, address)
}

#[test]
#[ignore = "latency measurement; run with --ignored --nocapture"]
fn stream_session_against_fake_host() {
    let (_host, address) = start_fake_host(90);
    let pc = DeviceIdentity::from_seed(&hex32(PC_SEED_HEX));
    let viewer = ViewerAuth::new(DeviceIdentity::from_seed(&[7u8; 32]), None)
        .with_paired(vec![(pc.id(), pc.public_key())]);

    let base = Instant::now();
    // Written by the owner thread right after the finished frame is published: first the time
    // (ns since `base`), then the running frame count.
    let published_ns = Arc::new(AtomicU64::new(0));
    let published = Arc::new(AtomicU64::new(0));
    let params = StreamParams {
        viewer,
        relay: address.replacen("ws://", "http://", 1),
        pc_id: pc.id().to_string(),
        pc_name: "fake host".into(),
        mode: StreamMode::Work,
        default_monitor: None,
        on_frame: Arc::new({
            let published_ns = published_ns.clone();
            let published = published.clone();
            move || {
                published_ns.store(base.elapsed().as_nanos() as u64, Ordering::Release);
                published.fetch_add(1, Ordering::Release);
            }
        }),
        on_change: Arc::new(|| {}),
        log_hook: None,
    };
    let session = StreamSession::start(params).expect("start the stream session");

    let consumer = create_device().expect("consumer device");
    let reader = PixelReader::new(&consumer);

    // Wait for the first picture.
    let wait_start = Instant::now();
    while published.load(Ordering::Acquire) == 0 {
        assert!(
            wait_start.elapsed() < Duration::from_secs(60),
            "no frame in 60 s; last error: {:?}",
            session.last_error()
        );
        thread::sleep(Duration::from_millis(50));
    }

    let measure = Duration::from_secs(20);
    let start = Instant::now();
    let mut seen = published.load(Ordering::Acquire);
    let mut next_sample = start + Duration::from_secs(1);
    let mut windows = Vec::new(); // (dtp p50, dtp p95, rtd p50, rtd p95, presented fps)
    let (mut since_publish, mut open_read) = (vec![], vec![]);
    let mut intervals = vec![];
    let mut last_publish_ns = published_ns.load(Ordering::Acquire);
    let mut skipped = 0u64;
    while start.elapsed() < measure && !session.is_stopped() {
        let count = published.load(Ordering::Acquire);
        if count != seen {
            let t_detect = Instant::now();
            let publish_ns = published_ns.load(Ordering::Acquire);
            skipped += count - seen - 1;
            seen = count;
            if let Some(front) = session.front() {
                let texture = open_shared(&consumer, HANDLE(front.handle as *mut _));
                let _ = reader.first_pixel(&texture);
                let t_done = Instant::now();
                let published_at = base + Duration::from_nanos(publish_ns);
                since_publish.push(ms(published_at, t_done));
                open_read.push(ms(t_detect, t_done));
            }
            intervals.push((publish_ns - last_publish_ns) as f64 / 1e6);
            last_publish_ns = publish_ns;
        } else {
            thread::sleep(Duration::from_micros(200));
        }
        if Instant::now() >= next_sample {
            next_sample += Duration::from_secs(1);
            let s = session.stats();
            windows.push((
                s.decode_to_present_p50_ms,
                s.decode_to_present_p95_ms,
                s.recv_to_decode_p50_ms,
                s.recv_to_decode_p95_ms,
                s.presented_fps,
            ));
        }
    }
    let last = session.stats();
    let error = session.last_error();
    drop(session);

    #[allow(clippy::type_complexity)]
    let col = |f: fn(&(Option<f64>, Option<f64>, Option<f64>, Option<f64>, f64)) -> Option<f64>| {
        windows.iter().filter_map(f).collect::<Vec<f64>>()
    };
    println!(
        "StreamSession vs fake host (h264 1080p60 recording), {:?} measured:",
        measure
    );
    println!("  {last}");
    println!("  session error: {error:?}");
    println!(
        "  per-second windows of StatsSnapshot ({} windows):",
        windows.len()
    );
    report("decode->present p50 of each window", &col(|w| w.0));
    report("decode->present p95 of each window", &col(|w| w.1));
    report("receive->decode p50 of each window", &col(|w| w.2));
    report("receive->decode p95 of each window", &col(|w| w.3));
    report("presented fps of each window (not ms)", &col(|w| Some(w.4)));
    println!(
        "  frames published: {}, skipped by the poller: {skipped}",
        seen
    );
    report("published-frame interval", &intervals);
    report("published -> first pixel read, 2nd device", &since_publish);
    report("  of which open handle + read (after detect)", &open_read);
}
