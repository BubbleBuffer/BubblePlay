//! WSS development session: network and decode workers feeding the UI shell.

mod app;
mod decode;
mod options;
mod order;
mod pacing;
mod stats;

pub use options::DevOptions;
pub use stats::SessionStats;

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    media::{Codec, MediaRuntime, MediaStats, StatsSnapshot, VideoRenderer},
    model::{AppState, Backend, Link, Monitor, Quality, Session, Stats},
    net::{self, CookieJar, DecoderFeedback, NetConfig, NetState, TransportStats},
    shell::{Shell, ShellConfig},
};
use anyhow::{bail, Context, Result};
use app::{DevApp, SessionBackend};
use decode::{DecodeCallbacks, FrameMeta};
use options::CodecChoice;

#[derive(Default)]
struct Diagnostics {
    errors: Vec<String>,
    resets: Vec<serde_json::Value>,
    waiting: Option<(u64, String, Instant)>,
    recoveries: Vec<serde_json::Value>,
}

impl Diagnostics {
    fn error(&mut self, error: String) {
        if self.errors.len() < 16 {
            self.errors.push(error);
        }
    }
    fn reset(&mut self, epoch: u64, reason: &str, at: Instant, started: Instant) {
        if self.resets.len() < 64 {
            self.resets.push(serde_json::json!({"local_epoch":epoch,"reason":reason,"at_ms":at.duration_since(started).as_secs_f64()*1000.0}));
        }
        if reason != "decoder-chain" && reason != "host-joined" {
            self.waiting = Some((epoch, reason.to_string(), at));
        }
    }
    fn presented(&mut self, meta: FrameMeta, at: Instant, started: Instant) {
        if let Some((epoch, reason, reset_at)) = self.waiting.take() {
            if epoch == meta.local_epoch && self.recoveries.len() < 64 {
                self.recoveries.push(serde_json::json!({
                    "reason":reason,"local_epoch":epoch,"generation":meta.generation,"seq":meta.seq,
                    "reset_at_ms":reset_at.duration_since(started).as_secs_f64()*1000.0,
                    "frame_received_at_ms":meta.received_at.duration_since(started).as_secs_f64()*1000.0,
                    "presented_at_ms":at.duration_since(started).as_secs_f64()*1000.0,
                    "frame_receive_to_presented_ms":at.duration_since(meta.received_at).as_secs_f64()*1000.0,
                    "reset_to_presented_ms":at.duration_since(reset_at).as_secs_f64()*1000.0
                }));
            }
        }
    }
}

/// The rolling measurement window is frozen before shell teardown; totals are
/// refreshed after worker shutdown separately. These remain media blit counts.
struct SummarySnapshot {
    sample: StatsSnapshot,
    arrival_times_ms: Vec<f64>,
    end_ms: f64,
    boundary: &'static str,
}

impl SummarySnapshot {
    fn capture(stats: &SessionStats, started: Instant, boundary: &'static str) -> (Self, Instant) {
        let mut sample = stats.snapshot();
        let end = Instant::now();
        let arrivals: Vec<_> = stats
            .transport
            .receive_timestamps()
            .into_iter()
            .filter(|at| *at <= end && end.duration_since(*at) <= Duration::from_secs(1))
            .collect();
        // Keep the raw receipt list and reported receipt FPS on the same
        // endpoint even if the network advances during the snapshot copy.
        sample.received_fps = arrivals.len() as f64;
        let arrival_times_ms = arrivals
            .into_iter()
            .map(|at| at.saturating_duration_since(started).as_secs_f64() * 1000.0)
            .collect();
        (
            Self {
                sample,
                arrival_times_ms,
                end_ms: end.saturating_duration_since(started).as_secs_f64() * 1000.0,
                boundary,
            },
            end,
        )
    }
}

/// All threads are joined, including on startup or shell errors. The timer is
/// interruptible so closing the window never leaves an exit watchdog behind.
struct Running {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<Result<()>>>,
    timer_cancel: Option<mpsc::Sender<()>>,
    diagnostics: Arc<Mutex<Diagnostics>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(cancel) = self.timer_cancel.take() {
            let _ = cancel.send(());
        }
        for handle in self.handles.drain(..) {
            let error = match handle.join() {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(format!("worker failed: {error:#}")),
                Err(_) => Some("session worker panicked".into()),
            };
            if let Some(error) = error {
                self.diagnostics.lock().unwrap().error(error);
            }
        }
    }
}

pub fn run_dev(options: DevOptions) -> Result<()> {
    let started = Instant::now();
    let pacing = pacing::Pacing::new(started);
    pacing.mark("runtime_start");
    let _runtime = MediaRuntime::new()?;
    pacing.mark("runtime_ready");
    pacing.mark("shell_create_start");
    let shell = Shell::new(ShellConfig {
        title: "BubblePlay".into(),
        size: (crate::ui::theme::WINDOW_W, crate::ui::theme::WINDOW_H),
        min_size: (760.0, 520.0),
    })?;
    pacing.mark("shell_ready");
    let device = shell.device();
    pacing.mark("h264_probe_start");
    let h264_available = crate::media::hardware_decoder_available(Codec::H264, &device)?;
    pacing.mark("h264_probe_end");
    pacing.mark("hevc_probe_start");
    let hevc_available = crate::media::hardware_decoder_available(Codec::Hevc, &device)?;
    pacing.mark("hevc_probe_end");
    let codec = match options.codec {
        CodecChoice::Auto | CodecChoice::H264 if h264_available => Codec::H264,
        CodecChoice::Hevc if hevc_available => Codec::Hevc,
        CodecChoice::Auto | CodecChoice::H264 => {
            bail!("no hardware H.264 decoder on the shell device")
        }
        CodecChoice::Hevc => bail!("no hardware HEVC decoder on the shell device"),
    };
    let mut relay = url::Url::parse(&options.url).context("invalid --dev-wss URL")?;
    let scheme = match relay.scheme() {
        "ws" => "http",
        "wss" => "https",
        _ => bail!("--dev-wss requires ws:// or wss://"),
    };
    relay
        .set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid WebSocket scheme"))?;
    let config = NetConfig::new(
        relay.to_string(),
        match codec {
            Codec::H264 => net::Codec::H264,
            Codec::Hevc => net::Codec::Hevc,
        },
        CookieJar::new(),
    );
    let wake = shell.wake_handle();
    let media_stats = MediaStats::new();
    let transport = Arc::new(TransportStats::new());
    let stats = SessionStats {
        media: media_stats.clone(),
        transport: transport.clone(),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let diagnostics = Arc::new(Mutex::new(Diagnostics::default()));
    let metadata: Arc<Mutex<BTreeMap<i64, FrameMeta>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let (event_tx, event_rx) = mpsc::sync_channel(8);
    let (feedback_tx, feedback_rx) = mpsc::channel();
    let mut video = VideoRenderer::new(
        &device,
        media_stats.clone(),
        Some(Arc::new({
            let wake = wake.clone();
            move || wake.wake()
        })),
    )?;
    video.set_trace_callback(Some(Arc::new({
        let pacing = pacing.clone();
        move |stage, pts, at| pacing.record(stage, pts, at)
    })));
    video.set_presented_callback(Arc::new({
        let pacing = pacing.clone();
        let metadata = metadata.clone();
        let feedback = feedback_tx.clone();
        let diagnostics = diagnostics.clone();
        move |pts, at| {
            pacing.record("first_blit", pts, at);
            let meta = {
                let mut entries = metadata.lock().unwrap();
                let meta = entries.remove(&pts);
                entries.retain(|key, _| *key > pts);
                meta
            };
            if let Some(meta) = meta {
                let _ = feedback.send(DecoderFeedback::Presented {
                    local_epoch: meta.local_epoch,
                    generation: meta.generation,
                    seq: meta.seq,
                });
                diagnostics.lock().unwrap().presented(meta, at, started);
            }
        }
    }));
    let sink = video.sink();
    let mut running = Running {
        stop: stop.clone(),
        handles: Vec::new(),
        timer_cancel: None,
        diagnostics: diagnostics.clone(),
    };
    running.handles.push(
        thread::Builder::new()
            .name("bubbleplay-wss".into())
            .spawn({
                let pacing = pacing.clone();
                let stop = stop.clone();
                let diagnostics = diagnostics.clone();
                let wake = wake.clone();
                move || {
                    pacing.mark("net_thread_enter");
                    let result = net::run(config, event_tx, feedback_rx, stop.clone(), transport)
                        .map_err(anyhow::Error::new);
                    if let Err(error) = &result {
                        diagnostics
                            .lock()
                            .unwrap()
                            .error(format!("network: {error:#}"));
                        stop.store(true, Ordering::Release);
                        wake.wake();
                    }
                    result
                }
            })?,
    );
    running.handles.push(
        thread::Builder::new()
            .name("bubbleplay-decode".into())
            .spawn({
                let stop = stop.clone();
                let diagnostics = diagnostics.clone();
                let wake = wake.clone();
                let publish = {
                    let pacing = pacing.clone();
                    let metadata = metadata.clone();
                    let sink = sink.clone();
                    let stats = media_stats.clone();
                    move |frame: crate::media::DecodedFrame,
                          meta: FrameMeta,
                          decoded_at: Instant| {
                        pacing.record("receive", meta.pts_100ns, meta.received_at);
                        stats.note_received(meta.pts_100ns, meta.received_at);
                        {
                            let mut entries = metadata.lock().unwrap();
                            entries.insert(meta.pts_100ns, meta);
                            while entries.len() > 64 {
                                entries.pop_first();
                            }
                        }
                        sink.push_timed(frame, decoded_at);
                    }
                };
                let reset = {
                    let metadata = metadata.clone();
                    let sink = sink.clone();
                    let diagnostics = diagnostics.clone();
                    let wake = wake.clone();
                    move |epoch, reason: &str, at| {
                        metadata.lock().unwrap().clear();
                        sink.clear_pending();
                        diagnostics
                            .lock()
                            .unwrap()
                            .reset(epoch, reason, at, started);
                        wake.wake();
                    }
                };
                let error = {
                    let stats = media_stats.clone();
                    let diagnostics = diagnostics.clone();
                    let wake = wake.clone();
                    move |error| {
                        stats.note_decode_error();
                        diagnostics.lock().unwrap().error(error);
                        wake.wake();
                    }
                };
                let dropped = {
                    let stats = media_stats.clone();
                    move |count| {
                        for _ in 0..count {
                            stats.note_dropped(Instant::now());
                        }
                    }
                };
                move || {
                    let result = decode::run(
                        device,
                        event_rx,
                        feedback_tx,
                        stop.clone(),
                        DecodeCallbacks {
                            publish,
                            reset,
                            error,
                            dropped,
                        },
                    );
                    if let Err(error) = &result {
                        diagnostics
                            .lock()
                            .unwrap()
                            .error(format!("decode worker: {error:#}"));
                        stop.store(true, Ordering::Release);
                        wake.wake();
                    }
                    result
                }
            })?,
    );
    if let Some(after) = options.exit_after {
        let (cancel_tx, cancel_rx) = mpsc::channel();
        running.timer_cancel = Some(cancel_tx);
        running.handles.push(
            thread::Builder::new()
                .name("bubbleplay-exit".into())
                .spawn({
                    let wake = wake.clone();
                    move || {
                        if cancel_rx
                            .recv_timeout(after.saturating_sub(started.elapsed()))
                            .is_err()
                        {
                            wake.wake();
                        }
                        Ok(())
                    }
                })?,
        );
    }
    let mut backend = SessionBackend {
        poll: Arc::new({
            let stats = stats.clone();
            move || app_state(&stats)
        }),
        stop: stop.clone(),
        notifier: None,
    };
    backend.set_notifier(Arc::new({
        let wake = wake.clone();
        move || wake.wake()
    }));
    let summary_snapshot = Arc::new(Mutex::new(None));
    let mut app = DevApp {
        ui: crate::ui::App::new(Box::new(backend)),
        pacing: pacing.clone(),
        stop,
        started,
        exit_after: options.exit_after,
        capture: options.capture_path,
        before_close: Some(Box::new({
            let summary_snapshot = summary_snapshot.clone();
            let stats = stats.clone();
            let pacing = pacing.clone();
            move || {
                let (snapshot, at) =
                    SummarySnapshot::capture(&stats, started, "dev_app_before_close");
                pacing.record("summary_snapshot", 0, at);
                *summary_snapshot.lock().unwrap() = Some(snapshot);
            }
        })),
        has_video: Arc::new({
            let stats = media_stats.clone();
            move || stats.snapshot(Instant::now()).totals.presented > 0
        }),
    };
    pacing.mark("shell_run_enter");
    let shell_result = shell.run(&mut app, &mut video);
    let summary = summary_snapshot.lock().unwrap().take().unwrap_or_else(|| {
        // OS/window errors can bypass DevApp's close request entirely.
        let (snapshot, at) = SummarySnapshot::capture(&stats, started, "shell_return_fallback");
        pacing.record("summary_snapshot", 0, at);
        snapshot
    });
    let mut last_window = summary.sample;
    drop(running);
    last_window.totals = stats.snapshot().totals;
    let mut document = report(
        &last_window,
        &stats,
        &diagnostics.lock().unwrap(),
        started.elapsed(),
        h264_available,
        hevc_available,
    );
    document["pacing"] = pacing.snapshot();
    document["arrival_times_last_second_ms"] = serde_json::json!(summary.arrival_times_ms);
    document["arrival_window_end_ms"] = serde_json::json!(summary.end_ms);
    document["summary_window_end_ms"] = serde_json::json!(summary.end_ms);
    document["summary_snapshot_boundary"] = serde_json::json!(summary.boundary);
    document["summary_window_boundary"] = serde_json::json!(if summary.boundary
        == "dev_app_before_close"
    {
        "close-request frame before its final shell video blit/egui/Present; rolling windows frozen here, totals refreshed after worker drain"
    } else {
        "Shell.run returned without a DevApp close snapshot; rolling windows sampled after shell teardown, totals refreshed after worker drain"
    });
    let text = serde_json::to_string_pretty(&document)?;
    println!("{text}");
    if let Some(path) = options.stats_path {
        std::fs::write(&path, format!("{text}\n"))
            .with_context(|| format!("write {}", path.display()))?;
    }
    shell_result?;
    if last_window.totals.presented == 0 {
        bail!("no video frames presented; inspect the development stats report");
    }
    if last_window.totals.decode_errors != 0
        || last_window.totals.present_errors != 0
        || !diagnostics.lock().unwrap().errors.is_empty()
    {
        bail!("session reported errors; inspect the development stats report");
    }
    Ok(())
}

fn app_state(stats: &SessionStats) -> AppState {
    let sample = stats.snapshot();
    let net = stats.transport.snapshot();
    AppState {
        session: Some(Session {
            pc_id: "dev-replay".into(),
            pc_name: "Local replay".into(),
            quality: Quality::Sharp,
            link: if net.state == NetState::Streaming {
                Link::Live
            } else {
                Link::Reconnecting {
                    attempt: net.attempts as u32,
                    secs: 0,
                }
            },
            monitors: vec![Monitor {
                name: "Replay".into(),
                width: sample.width,
                height: sample.height,
                refresh_hz: 60,
                primary: true,
                ..Default::default()
            }],
            stats: Stats {
                resolution: format!("{} × {}", sample.width, sample.height),
                fps: sample.presented_fps as f32,
                decode_ms: sample.recv_to_decode_p50_ms.unwrap_or(0.0) as f32,
                dropped: sample.totals.dropped as u32,
                transport: "WSS v4".into(),
                codec: sample.codec.map_or("", Codec::name).into(),
                ..Default::default()
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn report(
    sample: &StatsSnapshot,
    stats: &SessionStats,
    diagnostics: &Diagnostics,
    elapsed: Duration,
    h264: bool,
    hevc: bool,
) -> serde_json::Value {
    let net = stats.transport.snapshot();
    let totals = sample.totals;
    let seconds = elapsed.as_secs_f64();
    serde_json::json!({
        "codec":sample.codec.map(Codec::name),"resolution":[sample.width,sample.height],"duration_s":seconds,
        "hardware_decoders":{"h264":h264,"hevc":hevc},
        "fps_last_second":{"received":sample.received_fps,"decoded":sample.decoded_fps,"presented":sample.presented_fps},
        "fps_average":{"received":totals.received as f64/seconds,"decoded":totals.decoded as f64/seconds,"presented":totals.presented as f64/seconds},
        "receive_to_decoded_ms":{"p50":sample.recv_to_decode_p50_ms,"p95":sample.recv_to_decode_p95_ms},
        "decoded_to_presented_ms":{"p50":sample.decode_to_present_p50_ms,"p95":sample.decode_to_present_p95_ms},
        "totals":{"received":totals.received,"decoded":totals.decoded,"presented":totals.presented,"drops":totals.dropped,"keyframe_requests":totals.keyframe_requests,"decode_errors":totals.decode_errors,"present_errors":totals.present_errors},
        "transport":{"attempts":net.attempts,"established":net.established,"local_epoch":net.local_epoch,"acks_sent":net.acks_sent,"gate_drops":net.gate_drops,"queue_overflows":net.queue_overflows,"rejected_packets":net.rejected_packets,"ignored_audio":net.bpa1_ignored,"last_error":stats.transport.last_error()},
        "resets":diagnostics.resets,"recoveries":diagnostics.recoveries,"errors":diagnostics.errors,
        "presentation_boundary":"first successful VideoProcessorBlt submission before shell egui/Present; no scanout timing",
        "pass":totals.presented>0 && totals.decode_errors==0 && totals.present_errors==0 && diagnostics.errors.is_empty()
    })
}
