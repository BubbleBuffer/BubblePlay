mod backend;
mod display;
mod ffmpeg;
mod metrics;
mod mf;
mod pacing;
mod presenter;
mod streams;
mod timing;

use anyhow::{bail, Context, Result};
use backend::{Codec, DecodeBackend};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
};
use timing::{frequency, qpc, Sleeper};
use windows::Win32::{Media::MediaFoundation::*, System::Com::*};
struct Runtime;
impl Runtime {
    fn new() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        }
        Ok(Self)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}
fn arg(args: &[String], key: &str) -> Option<String> {
    args.windows(2).find(|a| a[0] == key).map(|a| a[1].clone())
}
fn main() -> Result<()> {
    timing::configure_process_timing()?;
    let args: Vec<String> = std::env::args().collect();
    if pacing::maybe_run_producer(&args)? {
        return Ok(());
    }
    if args.iter().any(|a| a == "--timer-check") {
        let count: usize = arg(&args, "--max-frames")
            .unwrap_or("1200".into())
            .parse()?;
        let tag = arg(&args, "--run").context("--run required for timer check")?;
        if count == 0
            || !tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("invalid timer check arguments");
        }
        let dir = PathBuf::from("F:/build/spike-results");
        let path = dir.join(format!("{tag}-timer.csv"));
        if path.exists() {
            bail!("timer check already exists");
        }
        let freq = frequency();
        let mut paced = pacing::PacedInput::new(count, qpc() + freq / 5, freq)?;
        let timer = Sleeper::new()?;
        let mut arrivals = Vec::with_capacity(count);
        while arrivals.len() < count {
            if let Some(a) = paced.next()? {
                arrivals.push(a);
            } else {
                timer.sleep_us(1000)?;
            }
        }
        if paced.finish()? != count {
            bail!("timer producer count mismatch");
        }
        let mut csv = String::from("frame,scheduled_qpc,arrived_qpc,late_ms\n");
        let mut late = Vec::new();
        for a in arrivals {
            let ms = (a.received - a.scheduled) as f64 * 1000.0 / freq as f64;
            late.push(ms);
            csv.push_str(&format!(
                "{},{},{},{:.6}\n",
                a.index, a.scheduled, a.received, ms
            ));
        }
        fs::write(path, csv)?;
        let summary=format!("timer HIGH_RESOLUTION + process HighQoS/no timer-resolution throttling + lean IPC producer MMCSS Pro Audio CRITICAL + process timer period1ms; frames={count}; median_ms={:.6}; p99_ms={:.6}; max_ms={:.6}; p99_under_1ms={}",metrics::percentile(&late,0.5),metrics::percentile(&late,0.99),metrics::percentile(&late,1.0),metrics::percentile(&late,0.99)<1.0);
        println!("{summary}");
        fs::write(dir.join(format!("{tag}-timer.txt")), summary)?;
        return Ok(());
    }
    let _runtime = Runtime::new()?;
    if args.iter().any(|a| a == "--list-decoders") {
        for codec in [Codec::H264, Codec::Hevc] {
            println!("{codec:?}:");
            for item in mf::availability_report(codec)? {
                println!("  {item}");
            }
        }
        return Ok(());
    }
    if args.len() == 1 || args.iter().any(|a| a == "--help") {
        println!("decode-latency --backend mf|ffmpeg --codec h264|hevc --input FILE --present immediate|vsync --run NAME [--output F:\\build\\spike-results] [--max-frames N]\ndecode-latency --list-decoders");
        return Ok(());
    }
    let backend_name = arg(&args, "--backend").unwrap_or("mf".into());
    if !["mf", "ffmpeg"].contains(&backend_name.as_str()) {
        bail!("--backend must be mf or ffmpeg");
    }
    let codec_name = arg(&args, "--codec").context("--codec required")?;
    let codec = match codec_name.as_str() {
        "h264" => Codec::H264,
        "hevc" => Codec::Hevc,
        _ => bail!("unknown codec"),
    };
    let mode = arg(&args, "--present").unwrap_or("immediate".into());
    if !["immediate", "vsync"].contains(&mode.as_str()) {
        bail!("unknown present mode");
    }
    let input = arg(&args, "--input").context("--input required")?;
    let tag = arg(&args, "--run").context("--run required (unique filename)")?;
    if tag.is_empty()
        || !tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid run name");
    }
    let output = PathBuf::from(arg(&args, "--output").unwrap_or("F:\\build\\spike-results".into()));
    if output.join(format!("{tag}.csv")).exists() {
        bail!("run already exists; choose another --run name");
    }
    fs::create_dir_all(&output)?;
    let data = fs::read(input)?;
    let mut units = streams::access_units(&data, codec)?;
    if let Some(n) = arg(&args, "--max-frames") {
        units.truncate(n.parse()?);
    }
    if units.is_empty() {
        bail!("empty stream");
    }
    let mut presenter = presenter::Presenter::new(&format!(
        "BubblePlay decode spike | {backend_name} | {codec_name} | {mode} | {tag}"
    ))?;
    for c in [Codec::H264, Codec::Hevc] {
        println!(
            "hardware_available({c:?})={}",
            mf::hardware_decoder_available(c, &presenter.device)?
        );
    }
    let mut decoder: Box<dyn DecodeBackend> = match backend_name.as_str() {
        "mf" => Box::new(mf::MfBackend::new(
            codec,
            &presenter.device,
            presenter::WIDTH,
            presenter::HEIGHT,
        )?),
        "ffmpeg" => Box::new(ffmpeg::FFmpegBackend::new(
            codec,
            &presenter.device,
            presenter::WIDTH,
            presenter::HEIGHT,
        )?),
        _ => unreachable!(),
    };
    let description = decoder.description().to_owned();
    println!(
        "adapter={}\ndisplay={}\nbackend={}\nAUs={} | 60fps | {mode}",
        presenter.adapter,
        presenter.display_mode,
        description,
        units.len()
    );
    println!("display_details={}", presenter.display_details);
    let sleeper = Sleeper::new()?;
    let freq = frequency();
    let epoch = qpc() + freq / 10;
    let mut paced = pacing::PacedInput::new(units.len(), epoch, freq)?;
    let epoch = paced.epoch();
    let mut pending = None;
    let mut submitted = HashMap::<i64, (usize, i64, i64, i64)>::new();
    let mut decoded = HashSet::new();
    let mut rows = Vec::with_capacity(units.len());
    let mut next = 0;
    let mut draining = false;
    let mut result = (|| -> Result<()> {
        loop {
            presenter::pump()?;
            let now = qpc();
            if now > epoch + freq * ((units.len() as i64 + 59) / 60 + 40) {
                bail!(
                    "run deadline exceeded: submitted={next}, decoded={}",
                    rows.len()
                );
            }
            if next < units.len() {
                if pending.is_none() {
                    pending = paced.next()?;
                }
                if let Some(arrival) = pending {
                    if arrival.index != next {
                        bail!("arrival index mismatch");
                    }
                    let pts = (next as i64 * 10_000_000) / 60;
                    let submit = qpc();
                    if decoder.submit(units[next], pts)? {
                        submitted.insert(pts, (next, arrival.scheduled, arrival.received, submit));
                        next += 1;
                        pending = None;
                    }
                }
            } else if !draining {
                decoder.drain()?;
                draining = true;
            }
            if let Some(frame) = decoder.poll()? {
                let output_time = qpc();
                let (index, scheduled, arrived, submit) = *submitted
                    .get(&frame.pts_100ns)
                    .with_context(|| format!("unmatched output PTS {}", frame.pts_100ns))?;
                if !decoded.insert(index) {
                    bail!("duplicate decoded PTS");
                }
                let complete = presenter.decode_complete(&frame)?;
                let present = presenter.present(&frame, index as u32, mode == "vsync")?;
                rows.push(metrics::Row {
                    index,
                    scheduled,
                    arrived,
                    submit,
                    output: output_time,
                    decoded: complete,
                    present,
                });
                // MF sample lease remains alive through VP submission; D3D retains GPU resources.
                drop(frame);
                if rows.len() % 300 == 0 {
                    println!("decoded/presented {} / {}", rows.len(), units.len());
                }
                continue;
            }
            if draining && decoder.is_drained() {
                if rows.len() != units.len() {
                    bail!(
                        "decoder drained with {} of {} frames",
                        rows.len(),
                        units.len()
                    );
                }
                break;
            }
            // High resolution waitable timer avoids a hot loop and coarse Sleep(1).
            let until = if next < units.len() {
                epoch + (next as i64 * freq) / 60 - qpc()
            } else {
                0
            };
            let us = if until > 0 {
                ((until as u64 * 1_000_000 / freq as u64).min(500)).max(50)
            } else {
                200
            };
            sleeper.sleep_us(us)?;
        }
        Ok(())
    })();
    match paced.finish() {
        Ok(sent) if result.is_ok() && sent != units.len() => {
            result = Err(anyhow::anyhow!(
                "producer sent {sent} of {} AUs",
                units.len()
            ))
        }
        Err(e) if result.is_ok() => result = Err(e),
        _ => {}
    }
    // Let final queued flip become observable; keep pumping messages.
    for _ in 0..100 {
        presenter::pump()?;
        sleeper.sleep_us(1000)?;
    }
    let final_stats = presenter.final_stats();
    println!("final DXGI stats: {final_stats:?}");
    if result.is_ok() {
        let rgb = presenter.rgb_check()?;
        println!("{rgb}");
        fs::write(output.join(format!("{tag}-rgb.txt")), rgb)?;
    }
    let error = result.as_ref().err().map(|e| format!("{e:#}"));
    let summary = metrics::write_results(
        &output,
        &tag,
        &codec_name,
        &backend_name,
        &mode,
        &rows,
        units.len(),
        freq,
        &description,
        presenter.wait_timeouts,
        presenter.gpu_polls,
        error.as_deref(),
    )?;
    use std::io::Write;
    let mut info = fs::OpenOptions::new()
        .append(true)
        .open(output.join(format!("{tag}.txt")))?;
    writeln!(info,"harness_version=fence-v7-ipc\ngpu_fence_waits={}\ngpu_query_polls=0\nprobe_copy_timed=false\narrival_scheduler=lean_child_process_MMCSS_Pro_Audio_CRITICAL\narrival_timer_period_ms=1\narrival_cleanup_timeout_ms=20000\ndecode_fence_timeout_ms=10000\nrun_drain_grace_seconds=40\nprocess_power_throttling=execution_speed_and_ignore_timer_resolution_disabled\ndisplay_details={}",presenter.gpu_fence_waits,presenter.display_details)?;
    fs::write(
        output.join(format!("{tag}-final-stats.txt")),
        format!("{final_stats:?}\n"),
    )?;
    println!("{summary}");
    result
}
