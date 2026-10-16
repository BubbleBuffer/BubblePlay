//! Decode-free VM qualification window. It exercises the same SessionInput
//! and real /ws transport, without MF, D3D11, shell changes, or video claims.
//! The explicit environment opt-in prevents accidental local live testing.
use super::DevOptions;
use crate::{
    input::{CaptureStatus, SessionInput},
    net::{self, CookieJar, DecoderFeedback, NetConfig, NetEvent, TransportStats},
    shell::{InputCapture, Rect},
};
use anyhow::{bail, Context, Result};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*,
    },
};

pub const STALL_MESSAGE: u32 = WM_APP + 0x342;
pub const STATUS_MESSAGE: u32 = WM_APP + 0x343;
const WAKE_MESSAGE: u32 = WM_APP + 0x341;
struct State {
    capture: Option<SessionInput>,
    stop: Arc<AtomicBool>,
}
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = &*(lp.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    if !ptr.is_null() {
        let capture = (*ptr).capture.take();
        if let Some(mut capture) = capture {
            let result = capture.on_message(hwnd, msg, wp, lp);
            (*ptr).capture = Some(capture);
            if let Some(result) = result {
                return result;
            }
        }
        match msg {
            STATUS_MESSAGE => {
                let status = (*ptr)
                    .capture
                    .as_ref()
                    .map(|capture| capture.status().snapshot());
                let flags = status.map_or(0, |s| {
                    (s.captured as isize)
                        | ((s.hook_installed as isize) << 1)
                        | ((s.clipped as isize) << 2)
                        | ((s.input_ready as isize) << 3)
                        | ((s.released as isize) << 4)
                });
                return LRESULT(flags);
            }
            STALL_MESSAGE => {
                thread::sleep(Duration::from_millis((wp.0 as u64).min(10000)));
                return LRESULT(0);
            }
            WM_CLOSE => {
                (*ptr).stop.store(true, Ordering::Release);
                return LRESULT(0);
            }
            WM_DESTROY => {
                (*ptr).stop.store(true, Ordering::Release);
                PostQuitMessage(0);
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            WAKE_MESSAGE => return LRESULT(0),
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}
struct Window {
    hwnd: HWND,
    state: Box<State>,
}
impl Window {
    fn create(stop: Arc<AtomicBool>) -> Result<Self> {
        let mut state = Box::new(State {
            capture: None,
            stop,
        });
        unsafe {
            let module = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: module.into(),
                lpszClassName: w!("BubblePlay.InputQualification"),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            // ERROR_CLASS_ALREADY_EXISTS is harmless in a reused test process.
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class.lpszClassName,
                w!("BubblePlay input qualification"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                120,
                120,
                960,
                640,
                None,
                None,
                Some(module.into()),
                Some((&mut *state as *mut State).cast()),
            )?;
            Ok(Self { hwnd, state })
        }
    }
    fn frame(&mut self) {
        let mut rect = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rect);
        }
        let video = Rect {
            x: 40.0,
            y: 40.0,
            w: (rect.right - rect.left - 80).max(0) as f32,
            h: (rect.bottom - rect.top - 80).max(0) as f32,
        };
        if let Some(mut capture) = self.state.capture.take() {
            capture.on_frame(self.hwnd, Some(video));
            self.state.capture = Some(capture);
        }
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        self.state.capture.take();
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
struct Worker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<(), net::NetError>>>,
    drain: Option<JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if let Some(drain) = self.drain.take() {
            let _ = drain.join();
        }
    }
}

pub fn run(options: DevOptions) -> Result<()> {
    if std::env::var("BUBBLEPLAY_INPUT_LIVE_TESTS").as_deref() != Ok("1") {
        bail!("--dev-input-only requires BUBBLEPLAY_INPUT_LIVE_TESTS=1 on VM 101");
    }
    let endpoint = url::Url::parse(&options.url)?;
    if endpoint.scheme() != "ws" || endpoint.host_str() != Some("127.0.0.1") {
        bail!("input qualification only accepts ws://127.0.0.1");
    }
    let after = options
        .exit_after
        .context("--dev-input-only requires --exit-after")?;
    let started = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let mut window = Window::create(stop.clone())?;
    let hwnd = Arc::new(AtomicUsize::new(window.hwnd.0 as usize));
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new({
        let hwnd = hwnd.clone();
        move || unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd.load(Ordering::Acquire) as *mut _)),
                WAKE_MESSAGE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    });
    let input = net::InputLink::new(wake.clone());
    let capture = SessionInput::new(options.mode, input.clone(), stop.clone(), wake.clone());
    let status: CaptureStatus = capture.status();
    window.state.capture = Some(capture);
    let mut config = NetConfig::new(options.url.clone(), net::Codec::H264, CookieJar::new());
    config.input = Some(input.clone());
    config.stream_mode = Some(options.mode.stream_mode().into());
    let stats = Arc::new(TransportStats::new());
    let (event_tx, event_rx) = mpsc::sync_channel(8);
    let (feedback_tx, feedback_rx) = mpsc::channel();
    let mut worker = Worker {
        stop: stop.clone(),
        drain: None,
        handle: Some(
            thread::Builder::new()
                .name("bubbleplay-input-wss".into())
                .spawn({
                    let stop = stop.clone();
                    let stats = stats.clone();
                    move || net::run(config, event_tx, feedback_rx, stop, stats)
                })?,
        ),
    };
    // Keep draining network frames during a slow UI frame, as the real decode
    // worker does. Only metadata is admitted to a latest-frame mailbox here;
    // the fixture never claims that encoded bytes were decoded.
    let latest = Arc::new(Mutex::new(None));
    worker.drain = Some(
        thread::Builder::new()
            .name("bubbleplay-input-frame-drain".into())
            .spawn({
                let latest = latest.clone();
                let stop = stop.clone();
                let wake = wake.clone();
                move || {
                    while !stop.load(Ordering::Acquire) {
                        match event_rx.recv_timeout(Duration::from_millis(25)) {
                            Ok(NetEvent::Frame(frame)) => {
                                *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
                                wake();
                            }
                            Ok(NetEvent::Reset(_)) => {
                                latest.lock().unwrap_or_else(|e| e.into_inner()).take();
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                }
            })?,
    );
    let mut changes = Vec::<serde_json::Value>::new();
    let mut last = None;
    let mut placeholder_frames = 0u64;
    while !stop.load(Ordering::Acquire) && started.elapsed() < after {
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                if message.message == WM_QUIT {
                    stop.store(true, Ordering::Release);
                    break;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if let Some(frame) = latest.lock().unwrap_or_else(|e| e.into_inner()).take() {
            // A test placeholder is admitted on the UI thread. This is
            // explicitly not a hardware decode/presentation assertion.
            placeholder_frames += 1;
            let _ = feedback_tx.send(DecoderFeedback::Presented {
                local_epoch: frame.local_epoch,
                generation: frame.generation,
                seq: frame.seq,
            });
        }
        window.frame();
        let snap = serde_json::to_value(status.snapshot())?;
        if last.as_ref() != Some(&snap) {
            changes.push(
                serde_json::json!({"at_ms":started.elapsed().as_secs_f64()*1000.0,"state":snap}),
            );
            last = Some(snap);
        }
        unsafe {
            MsgWaitForMultipleObjectsEx(None, 8, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    }
    let quit_at = started.elapsed();
    hwnd.store(0, Ordering::Release);
    drop(window);
    stop.store(true, Ordering::Release);
    let network = worker
        .handle
        .take()
        .unwrap()
        .join()
        .map_err(|_| anyhow::anyhow!("input network worker panicked"))?;
    drop(worker);
    let report = serde_json::json!({"boundary":"decode-free input qualification; placeholder admission, no MF/D3D11/video proof","elapsed_ms":quit_at.as_secs_f64()*1000.0,"placeholder_frames":placeholder_frames,"input":input.snapshot(),"capture_state":status.snapshot(),"capture_events":changes,"network_error":network.as_ref().err().map(ToString::to_string)});
    if let Some(path) = options.stats_path {
        std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
    }
    network?;
    if placeholder_frames == 0 {
        bail!("input qualification received no frames");
    }
    Ok(())
}
