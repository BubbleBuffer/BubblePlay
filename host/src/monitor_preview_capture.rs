//! Takes the monitor preview pictures (`monitor-previews-v1`, see `docs/architecture/monitor-previews.md`).
//!
//! A worker thread stretches each monitor that is not streamed from its display DC into a 360p
//! DIB at most once per second and hands the result to [`PreviewSender`], which decides whether a
//! snapshot goes out. GDI is used on purpose: no second Desktop Duplication session and no
//! encoder session, so the live stream is not touched. The main loop owns the thread through
//! [`PreviewService`] and only polls a channel.

use crate::capture::{enumerate_monitors, MonitorInfo};
use crate::monitor_previews::{self, LinkState, PreviewSender, Skip, Stats, Thumb};
use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDCW, CreateDIBSection, DeleteDC, DeleteObject, SelectObject,
    SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
    DIB_RGB_COLORS, HALFTONE, SRCCOPY,
};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
};

/// How often the worker looks at the monitor list and the due times.
const TICK: Duration = Duration::from_millis(250);
/// The monitor list is re-read this often (monitors come and go, the streamed one changes).
const LIST_INTERVAL: Duration = Duration::from_secs(2);

/// Stretches the monitor `device_name` (`\\.\DISPLAYn`), `src_w` x `src_h` pixels, into a
/// thumbnail of [`monitor_previews::preview_size`].
pub fn capture_thumb(device_name: &str, src_w: u32, src_h: u32) -> Result<Thumb> {
    if src_w == 0 || src_h == 0 {
        bail!("monitor {device_name} has no size");
    }
    let (width, height) = monitor_previews::preview_size(src_w, src_h);
    let name: Vec<u16> = device_name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: NUL-terminated device name; every handle made here is released below.
    let source = unsafe { CreateDCW(w!("DISPLAY"), PCWSTR(name.as_ptr()), PCWSTR::null(), None) };
    if source.is_invalid() {
        bail!("CreateDCW failed for {device_name}");
    }
    let result = (|| -> Result<Thumb> {
        // SAFETY: `source` is a live DC.
        let memory = unsafe { CreateCompatibleDC(Some(source)) };
        if memory.is_invalid() {
            bail!("CreateCompatibleDC failed");
        }
        let inner = (|| -> Result<Thumb> {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    biSizeImage: width * height * 4,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut::<core::ffi::c_void>();
            // SAFETY: `info` describes a top-down 32-bit DIB; `bits` receives its storage.
            let bitmap = unsafe {
                CreateDIBSection(
                    Some(memory),
                    &info,
                    DIB_RGB_COLORS,
                    &mut bits,
                    None::<HANDLE>,
                    0,
                )
            }
            .context("CreateDIBSection(monitor preview)")?;
            if bits.is_null() {
                // SAFETY: the bitmap was created above and is not selected anywhere.
                let _ = unsafe { DeleteObject(bitmap.into()) };
                bail!("CreateDIBSection returned no pixel storage");
            }
            // SAFETY: both handles are live; the previous object is restored before deleting.
            let previous = unsafe { SelectObject(memory, bitmap.into()) };
            let copied = (|| -> Result<Vec<u8>> {
                // SAFETY: `memory` and `source` are live DCs.
                unsafe {
                    SetStretchBltMode(memory, HALFTONE);
                    if !StretchBlt(
                        memory,
                        0,
                        0,
                        width as i32,
                        height as i32,
                        Some(source),
                        0,
                        0,
                        src_w as i32,
                        src_h as i32,
                        SRCCOPY | CAPTUREBLT,
                    )
                    .as_bool()
                    {
                        bail!("StretchBlt failed for {device_name}");
                    }
                }
                // SAFETY: the DIB holds width * height * 4 bytes and stays selected until here.
                Ok(unsafe {
                    std::slice::from_raw_parts(bits as *const u8, (width * height * 4) as usize)
                }
                .to_vec())
            })();
            // SAFETY: restore the DC's original bitmap, then free ours.
            unsafe {
                SelectObject(memory, previous);
                let _ = DeleteObject(bitmap.into());
            }
            Thumb::new(width, height, copied?)
        })();
        // SAFETY: `memory` was created above.
        let _ = unsafe { DeleteDC(memory) };
        inner
    })();
    // SAFETY: `source` was created above.
    let _ = unsafe { DeleteDC(source) };
    result
}

/// One snapshot ready to go out as a ctl message.
#[derive(Debug)]
pub struct PreviewOut {
    pub seq: u64,
    pub monitor: String,
    pub width: u32,
    pub height: u32,
    pub jpeg: Vec<u8>,
}

/// Counters of one service (a viewer session): the sender's plus what only the worker sees.
#[derive(Debug, Clone, Copy, Default)]
pub struct ServiceStats {
    pub sender: Stats,
    pub capture_failures: u64,
    pub max_capture_ms: u64,
}

struct Shared {
    stop: AtomicBool,
    stable: AtomicBool,
    streamed: Mutex<String>,
    stats: Mutex<ServiceStats>,
}

/// The worker thread of one viewer session.
pub struct PreviewService {
    shared: Arc<Shared>,
    outgoing: Receiver<PreviewOut>,
    worker: Option<JoinHandle<()>>,
}

impl PreviewService {
    pub fn start(streamed: &str) -> Self {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            stable: AtomicBool::new(false),
            streamed: Mutex::new(streamed.to_owned()),
            stats: Mutex::new(ServiceStats::default()),
        });
        let (sender, outgoing) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("monitor-previews".into())
            .spawn(move || run(&worker_shared, &sender))
            .ok();
        Self {
            shared,
            outgoing,
            worker,
        }
    }

    /// The monitor selector the live stream shows now.
    pub fn set_streamed(&self, selector: &str) {
        let mut streamed = lock(&self.shared.streamed);
        if *streamed != selector {
            *streamed = selector.to_owned();
        }
    }

    /// Whether the link is stable right now (see the main loop's gate).
    pub fn set_stable(&self, stable: bool) {
        self.shared.stable.store(stable, Ordering::Relaxed);
    }

    /// The next snapshot to send, if any. Never blocks.
    pub fn try_next(&self) -> Option<PreviewOut> {
        self.outgoing.try_recv().ok()
    }

    pub fn stats(&self) -> ServiceStats {
        *lock(&self.shared.stats)
    }

    /// Stops the worker and returns the final counters.
    pub fn finish(mut self) -> ServiceStats {
        self.stop();
        self.stats()
    }

    fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

impl Drop for PreviewService {
    fn drop(&mut self) {
        self.stop();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn run(shared: &Shared, out: &Sender<PreviewOut>) {
    // Below the capture, encode and input threads: a snapshot may wait for a core.
    // SAFETY: lowers the priority of this thread only.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    let mut sender = PreviewSender::new(Instant::now());
    let mut monitors: Vec<MonitorInfo> = Vec::new();
    let mut listed_at: Option<Instant> = None;
    while !shared.stop.load(Ordering::Relaxed) {
        // `stop` unparks this thread; otherwise it wakes every tick.
        std::thread::park_timeout(TICK);
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        if listed_at.is_none_or(|at| now.duration_since(at) >= LIST_INTERVAL) {
            let streamed = lock(&shared.streamed).clone();
            monitors = enumerate_monitors(Some(&streamed))
                .unwrap_or_default()
                .into_iter()
                .filter(|monitor| !monitor.selected)
                .collect();
            let names: Vec<String> = monitors.iter().map(|m| m.selector.clone()).collect();
            sender.retain(&names);
            listed_at = Some(now);
        }
        let stable = shared.stable.load(Ordering::Relaxed)
            && crate::desktop_session::input_desktop_is_default();
        for monitor in &monitors {
            if !sender.due(&monitor.selector, now) {
                continue;
            }
            if !stable {
                sender.gated(&monitor.selector, now);
                continue;
            }
            let started = Instant::now();
            let thumb = match capture_thumb(&monitor.selector, monitor.width, monitor.height) {
                Ok(thumb) => thumb,
                Err(error) => {
                    tracing::debug!(%error, monitor = %monitor.selector, "monitor preview capture failed");
                    sender.gated(&monitor.selector, now);
                    lock(&shared.stats).capture_failures += 1;
                    continue;
                }
            };
            let took = started.elapsed().as_millis() as u64;
            let (width, height) = (thumb.width, thumb.height);
            let decision = sender.consider(&monitor.selector, thumb, LinkState { stable }, now);
            {
                let mut stats = lock(&shared.stats);
                stats.max_capture_ms = stats.max_capture_ms.max(took);
                stats.sender = sender.stats();
            }
            match decision {
                Ok((seq, jpeg)) => {
                    let _ = out.send(PreviewOut {
                        seq,
                        monitor: monitor.selector.clone(),
                        width,
                        height,
                        jpeg,
                    });
                }
                Err(Skip::Unchanged | Skip::Gated | Skip::Budget | Skip::TooLarge) => {}
            }
        }
        lock(&shared.stats).sender = sender.stats();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a real display; run by hand: `cargo test monitor_preview_capture -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs an interactive desktop"]
    fn captures_each_monitor_at_preview_size_and_reports_the_cost() {
        let monitors = enumerate_monitors(None).expect("monitors");
        assert!(!monitors.is_empty());
        for monitor in monitors {
            let (width, height) = monitor_previews::preview_size(monitor.width, monitor.height);
            let mut times = Vec::new();
            let mut thumb = None;
            for _ in 0..10 {
                let started = Instant::now();
                thumb =
                    Some(capture_thumb(&monitor.selector, monitor.width, monitor.height).unwrap());
                times.push(started.elapsed());
            }
            let thumb = thumb.unwrap();
            assert_eq!((thumb.width, thumb.height), (width, height));
            assert!(thumb.bgra.iter().any(|&byte| byte != 0), "all black");
            let jpeg = monitor_previews::encode_jpeg(&thumb).unwrap();
            let encode_started = Instant::now();
            let _ = monitor_previews::encode_jpeg(&thumb).unwrap();
            times.sort();
            println!(
                "{} {}x{} -> {}x{}: stretch median {:?} max {:?}; jpeg {} bytes in {:?}",
                monitor.selector,
                monitor.width,
                monitor.height,
                width,
                height,
                times[times.len() / 2],
                times[times.len() - 1],
                jpeg.len(),
                encode_started.elapsed()
            );
        }
    }
}
