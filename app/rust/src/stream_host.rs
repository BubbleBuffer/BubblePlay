//! The running stream session of the app, and the C ABI the Windows runner uses to reach it.
//!
//! One session at a time lives in a process-wide slot. The backend starts and stops it on the
//! platform thread (the thread Dart's synchronous calls run on, which also runs the window
//! procedure), and the runner's stream plugin calls the `bp_stream_*` functions below from that
//! same thread: window messages for the input capture, the video rectangle from Dart, and the
//! shared texture handle for the Flutter texture. Video never passes through Dart or the
//! bridge: the session draws into shared D3D11 textures and the engine samples them.
//!
//! Without Windows there is no session: [`start`] fails and the runner functions do not exist.

use std::sync::{Arc, Mutex, MutexGuard};

use bubbleplay_client::model::{Session, StreamMode};
use bubbleplay_client::net::ViewerAuth;

/// What the backend hands over to start a session.
pub struct Start {
    pub viewer: ViewerAuth,
    pub relay: String,
    pub pc_id: String,
    pub pc_name: String,
    pub mode: StreamMode,
    pub default_monitor: Option<String>,
    pub on_change: Arc<dyn Fn() + Send + Sync>,
}

/// The state of the running session for the snapshot.
pub struct Status {
    pub session: Session,
    /// The session ended by itself; `error` is the last worker error, if any.
    pub ended: bool,
    pub error: Option<String>,
}

#[cfg(windows)]
pub use imp::*;

#[cfg(not(windows))]
pub fn start(_: Start) -> anyhow::Result<()> {
    anyhow::bail!("streaming is only available on Windows")
}
#[cfg(not(windows))]
pub fn status() -> Option<Status> {
    None
}
#[cfg(not(windows))]
pub fn act(_: bubbleplay_client::model::Action) -> bool {
    false
}
#[cfg(not(windows))]
pub fn stop() {}

#[allow(dead_code)]
fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(windows)]
mod imp {
    use super::*;
    use anyhow::Result;
    use bubbleplay_client::{
        model::Action,
        stream::player::{StreamParams, StreamSession},
        surface::Rect,
    };
    use std::sync::OnceLock;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};

    /// The session is only touched on the platform thread, except `is_stopped`, `front`, `state`,
    /// `act` and `stop`, which are thread safe. The capture inside is not `Send`.
    struct Slot(StreamSession);
    unsafe impl Send for Slot {}

    static SESSION: Mutex<Option<Slot>> = Mutex::new(None);
    static FRAME: OnceLock<Mutex<Option<extern "C" fn()>>> = OnceLock::new();

    fn frame_cb() -> &'static Mutex<Option<extern "C" fn()>> {
        FRAME.get_or_init(|| Mutex::new(None))
    }

    /// Starts a session; replaces and drops a running one first. Platform thread only.
    pub fn start(params: Start) -> Result<()> {
        stop();
        let session = StreamSession::start(StreamParams {
            viewer: params.viewer,
            relay: params.relay,
            pc_id: params.pc_id,
            pc_name: params.pc_name,
            mode: params.mode,
            default_monitor: params.default_monitor,
            on_frame: Arc::new(|| {
                let callback = *lock(frame_cb());
                if let Some(callback) = callback {
                    callback();
                }
            }),
            on_change: throttled(params.on_change),
        })?;
        *lock(&SESSION) = Some(Slot(session));
        Ok(())
    }

    /// The state change callback fires on every network event; the UI needs about four a
    /// second, but a change must never be lost, so a call inside the window is delivered late.
    fn throttled(inner: Arc<dyn Fn() + Send + Sync>) -> Arc<dyn Fn() + Send + Sync> {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::time::{Duration, Instant};
        let epoch = Instant::now();
        let last = Arc::new(AtomicU64::new(0));
        let pending = Arc::new(AtomicBool::new(false));
        Arc::new(move || {
            let now = epoch.elapsed().as_millis() as u64 + 1;
            let before = last.load(Ordering::Acquire);
            if before == 0 || now.saturating_sub(before) >= 250 {
                last.store(now, Ordering::Release);
                inner();
            } else if !pending.swap(true, Ordering::AcqRel) {
                let inner = inner.clone();
                let pending = pending.clone();
                let last = last.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(250));
                    pending.store(false, Ordering::Release);
                    last.store(epoch.elapsed().as_millis() as u64 + 1, Ordering::Release);
                    inner();
                });
            }
        })
    }

    pub fn status() -> Option<Status> {
        let guard = lock(&SESSION);
        let slot = guard.as_ref()?;
        let session = &slot.0;
        Some(Status {
            session: session.state(),
            ended: session.is_stopped(),
            error: session.last_error(),
        })
    }

    pub fn act(action: Action) -> bool {
        lock(&SESSION)
            .as_ref()
            .is_some_and(|slot| slot.0.act(action))
    }

    /// Ends the session and releases the capture. Platform thread only.
    pub fn stop() {
        let slot = lock(&SESSION).take();
        drop(slot);
    }

    fn hwnd_of(value: isize) -> HWND {
        HWND(value as *mut _)
    }

    /// Whether a session is running.
    #[no_mangle]
    pub extern "C" fn bp_stream_active() -> bool {
        lock(&SESSION).is_some()
    }

    /// Registers the function called whenever a new picture is ready. It may run on any thread.
    #[no_mangle]
    pub extern "C" fn bp_stream_set_frame_callback(callback: Option<extern "C" fn()>) {
        *lock(frame_cb()) = callback;
    }

    /// The picture to show: writes the shared handle, width, height and serial. Returns false
    /// before the first frame or without a session.
    ///
    /// # Safety
    /// All four pointers must be valid for writes.
    #[no_mangle]
    pub unsafe extern "C" fn bp_stream_front(
        handle: *mut isize,
        width: *mut u32,
        height: *mut u32,
        serial: *mut u64,
    ) -> bool {
        let Some(front) = lock(&SESSION).as_ref().and_then(|slot| slot.0.front()) else {
            return false;
        };
        unsafe {
            *handle = front.handle;
            *width = front.width;
            *height = front.height;
            *serial = front.serial;
        }
        true
    }

    /// Where the video sits in the window's client area, physical pixels; width or height 0
    /// clears it. Platform thread only.
    #[no_mangle]
    pub extern "C" fn bp_stream_set_rect(hwnd: isize, x: f32, y: f32, w: f32, h: f32) {
        if let Some(slot) = lock(&SESSION).as_mut() {
            let rect = (w > 0.0 && h > 0.0).then_some(Rect { x, y, w, h });
            slot.0.set_video_rect(hwnd_of(hwnd), rect);
        }
    }

    /// Window procedure hook: offers a message to the input capture. Returns true when it was
    /// handled; the result is then written to `result`. Platform thread only.
    ///
    /// # Safety
    /// `result` must be valid for writes.
    #[no_mangle]
    pub unsafe extern "C" fn bp_stream_message(
        hwnd: isize,
        msg: u32,
        wparam: usize,
        lparam: isize,
        result: *mut isize,
    ) -> bool {
        // Never block the window procedure, and never re-enter from inside `start`.
        let Ok(mut guard) = SESSION.try_lock() else {
            return false;
        };
        let Some(slot) = guard.as_mut() else {
            return false;
        };
        match slot
            .0
            .on_message(hwnd_of(hwnd), msg, WPARAM(wparam), LPARAM(lparam))
        {
            Some(LRESULT(value)) => {
                unsafe { *result = value };
                true
            }
            None => false,
        }
    }

    /// The capture's per-frame step; the runner calls it from a timer.
    #[no_mangle]
    pub extern "C" fn bp_stream_tick(hwnd: isize) {
        if let Ok(mut guard) = SESSION.try_lock() {
            if let Some(slot) = guard.as_mut() {
                slot.0.tick(hwnd_of(hwnd));
            }
        }
    }
}
