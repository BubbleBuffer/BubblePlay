//! Bounded development timing trace; records CPU boundaries, never scanout.
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use windows::Win32::{
    Foundation::{FILETIME, HANDLE},
    System::Threading::{GetCurrentThread, GetCurrentThreadId, GetThreadTimes},
};

#[link(name = "kernel32")]
extern "system" {
    fn QueryPerformanceCounter(value: *mut i64) -> i32;
    fn QueryPerformanceFrequency(value: *mut i64) -> i32;
    fn GetSystemTimePreciseAsFileTime(value: *mut u64);
    // windows 0.62 places this binding under WindowsProgramming. Keep the
    // existing feature set by using the same kernel32 FFI as the clock anchor.
    fn QueryThreadCycleTime(thread: HANDLE, cycles: *mut u64) -> i32;
}

#[derive(Clone)]
pub struct Pacing {
    started: Instant,
    frame_id: Arc<AtomicU64>,
    inner: Arc<Mutex<Inner>>,
    anchor: Arc<serde_json::Value>,
}

#[derive(serde::Serialize)]
struct Event {
    stage: &'static str,
    pts: i64,
    at_ms: f64,
    frame_id: u64,
    thread_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_cpu_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_cycles: Option<u64>,
}

/// Cumulative kernel plus user CPU time for the recording thread. Windows CPU
/// accounting can be quantized; a zero delta is not proof of zero CPU work.
fn current_thread_cpu_ms() -> Option<f64> {
    let (mut creation, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
        .ok()?;
    }
    let ticks =
        |value: FILETIME| ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64;
    Some((ticks(kernel) + ticks(user)) as f64 / 10_000.0)
}

fn current_thread_cycles() -> Option<u64> {
    let mut cycles = 0;
    if unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) } != 0 {
        Some(cycles)
    } else {
        None
    }
}
#[derive(Default)]
struct Inner {
    events: Vec<Event>,
    overflow: u64,
}

impl Pacing {
    pub fn new(started: Instant) -> Self {
        let (mut before, mut after, mut frequency, mut filetime) = (0i64, 0i64, 0i64, 0u64);
        unsafe {
            QueryPerformanceFrequency(&mut frequency);
            QueryPerformanceCounter(&mut before);
            GetSystemTimePreciseAsFileTime(&mut filetime);
            QueryPerformanceCounter(&mut after);
        }
        let elapsed = started.elapsed().as_nanos();
        Self {
            started,
            frame_id: Arc::new(AtomicU64::new(0)),
            inner: Arc::new(Mutex::new(Inner::default())),
            anchor: Arc::new(serde_json::json!({
                "qpc":before+(after-before)/2,"frequency":frequency,
                "filetime_100ns":filetime,"session_elapsed_ns":elapsed,
                "sampling_span_us":(after-before) as f64*1e6/frequency as f64
            })),
        }
    }
    pub fn mark(&self, stage: &'static str) {
        self.record(stage, 0, Instant::now());
    }
    pub fn begin_frame(&self) {
        self.frame_id.fetch_add(1, Ordering::Relaxed);
        self.mark("ui_frame_enter");
    }
    pub fn record(&self, stage: &'static str, pts: i64, at: Instant) {
        let event = Event {
            stage,
            pts,
            at_ms: at.saturating_duration_since(self.started).as_secs_f64() * 1000.0,
            frame_id: self.frame_id.load(Ordering::Relaxed),
            thread_id: unsafe { GetCurrentThreadId() },
            thread_cpu_ms: current_thread_cpu_ms(),
            thread_cycles: current_thread_cycles(),
        };
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.events.len() < 65536 {
            inner.events.push(event);
        } else {
            inner.overflow += 1;
        }
    }
    pub fn snapshot(&self) -> serde_json::Value {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::json!({"clock_anchor":&*self.anchor,"events":inner.events,"overflow":inner.overflow,
            "boundary":"CPU trace: wake to callback includes shell admission/message processing; DXGI ETW separately records Present return",
            "thread_cpu_scope":"Cumulative kernel+user CPU time and raw cycles of recording thread, sampled during callback; CPU accounting may be quantized and includes observer overhead. Cycles are not converted to time. Retroactive receive timestamps do not have simultaneous receipt CPU samples."})
    }
}
