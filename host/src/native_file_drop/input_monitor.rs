//! A temporary, category-only input lease for a prepared native drop.
//! Never infer wheel/keys from cursor timestamps: injected stationary movement
//! is still movement. These hooks neither read payloads nor suppress any input.
use anyhow::{bail, Context, Result};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::*;

const START_LIMIT: Duration = Duration::from_secs(2);
// A stalled pump/chain invalidates the lease. It is never a grace period for
// meaningful input. Windows can silently remove timed-out hooks, so also mark
// callback stalls themselves as failures rather than trusting pump recovery.
const PUMP_LIMIT: Duration = Duration::from_millis(500);
const PUMP_INTERVAL_MS: u32 = 25;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeDropInputStatus {
    Starting,
    Clear,
    Interrupted,
    Unavailable,
}

struct MonitorState {
    born: Instant,
    heartbeat_ms: AtomicU64,
    ready: AtomicBool,
    interrupted: AtomicBool,
    failed: AtomicBool,
    stop: AtomicBool,
    finished: AtomicBool,
    thread: AtomicU32,
}
impl MonitorState {
    fn new() -> Self {
        Self {
            born: Instant::now(),
            heartbeat_ms: AtomicU64::new(0),
            ready: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            thread: AtomicU32::new(0),
        }
    }
    fn elapsed_ms(&self) -> u64 {
        self.born.elapsed().as_millis() as u64
    }
    fn record(&self, meaningful: bool) {
        if meaningful && !self.stop.load(Ordering::Acquire) {
            self.interrupted.store(true, Ordering::Release);
        }
    }
    fn status(&self, now_ms: u64, held: bool) -> NativeDropInputStatus {
        if self.failed.load(Ordering::Acquire)
            || self.finished.load(Ordering::Acquire)
            || now_ms.saturating_sub(self.heartbeat_ms.load(Ordering::Acquire))
                > if self.ready.load(Ordering::Acquire) {
                    PUMP_LIMIT.as_millis() as u64
                } else {
                    START_LIMIT.as_millis() as u64
                }
        {
            NativeDropInputStatus::Unavailable
        } else if held || self.interrupted.load(Ordering::Acquire) {
            NativeDropInputStatus::Interrupted
        } else if self.ready.load(Ordering::Acquire) {
            NativeDropInputStatus::Clear
        } else {
            NativeDropInputStatus::Starting
        }
    }
}

thread_local! {
    // Only dereferenced on the installing thread, while its Arc and hooks live.
    static ACTIVE: Cell<*const MonitorState> = const { Cell::new(std::ptr::null()) };
}
fn meaningful_mouse(code: i32, message: u32) -> bool {
    code == HC_ACTION as i32
        && matches!(
            message,
            WM_LBUTTONDOWN
                | WM_RBUTTONDOWN
                | WM_MBUTTONDOWN
                | WM_XBUTTONDOWN
                | WM_MOUSEWHEEL
                | WM_MOUSEHWHEEL
        )
}
fn meaningful_keyboard(code: i32, message: u32) -> bool {
    code == HC_ACTION as i32 && matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN)
}
unsafe fn hook(code: i32, wparam: WPARAM, lparam: LPARAM, meaningful: bool) -> LRESULT {
    let started = Instant::now();
    ACTIVE.with(|active| {
        if let Some(state) = active.get().as_ref() {
            state.record(meaningful);
        }
    });
    // Never swallow input; payload (key, text, position, flags) is not read.
    let result = CallNextHookEx(None, code, wparam, lparam);
    if started.elapsed() >= PUMP_LIMIT {
        ACTIVE.with(|active| {
            if let Some(state) = active.get().as_ref() {
                state.failed.store(true, Ordering::Release);
            }
        });
    }
    result
}
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    hook(
        code,
        wparam,
        lparam,
        meaningful_mouse(code, wparam.0 as u32),
    )
}
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    hook(
        code,
        wparam,
        lparam,
        meaningful_keyboard(code, wparam.0 as u32),
    )
}
fn held_input() -> bool {
    // Only the current high bit is reliable. Hook categories catch short presses
    // without relying on GetAsyncKeyState's shared, nondeterministic low bit.
    (1..=255).any(|key| unsafe { GetAsyncKeyState(key) } < 0)
}

struct Hooks(Vec<HHOOK>);
impl Drop for Hooks {
    fn drop(&mut self) {
        for hook in self.0.drain(..).rev() {
            unsafe {
                let _ = UnhookWindowsHookEx(hook);
            }
        }
        ACTIVE.with(|active| active.set(std::ptr::null()));
    }
}
fn pump(state: &Arc<MonitorState>) -> Result<()> {
    unsafe {
        let mut message = MSG::default();
        // Create our private message queue before publishing its thread ID.
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
        state.thread.store(GetCurrentThreadId(), Ordering::Release);
        let module = GetModuleHandleW(None).context("input monitor module")?;
        let mut hooks = Hooks(Vec::with_capacity(2));
        ACTIVE.with(|active| active.set(Arc::as_ptr(state)));
        hooks.0.push(
            SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(HINSTANCE(module.0)), 0)
                .context("install prepared drop mouse monitor")?,
        );
        hooks.0.push(
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_hook),
                Some(HINSTANCE(module.0)),
                0,
            )
            .context("install prepared drop keyboard monitor")?,
        );
        // Covers input still held at startup; installed hooks catch short
        // presses after installation, including before readiness is published.
        state.record(held_input());
        state
            .heartbeat_ms
            .store(state.elapsed_ms(), Ordering::Release);
        state.ready.store(true, Ordering::Release);
        tracing::info!("prepared native drop input monitor ready");
        while !state.stop.load(Ordering::Acquire) {
            let now = state.elapsed_ms();
            if now.saturating_sub(state.heartbeat_ms.load(Ordering::Acquire))
                >= PUMP_LIMIT.as_millis() as u64
            {
                bail!("prepared drop input monitor stalled");
            }
            state.heartbeat_ms.store(now, Ordering::Release);
            // Bound each drain so a movement flood cannot starve stop/health.
            for _ in 0..128 {
                if !PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    break;
                }
                if message.message == WM_QUIT {
                    return Ok(());
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
                if state.stop.load(Ordering::Acquire) {
                    return Ok(());
                }
            }
            if MsgWaitForMultipleObjectsEx(None, PUMP_INTERVAL_MS, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
                == WAIT_FAILED
            {
                bail!("prepared drop input monitor wait failed");
            }
        }
    }
    Ok(())
}

/// Construct only after preview cleanup. Startup and cleanup are asynchronous;
/// main must wait for `Clear` before committing a prepared drop.
pub struct NativeDropInputGuard {
    state: Arc<MonitorState>,
}
impl NativeDropInputGuard {
    pub fn new() -> Result<Self> {
        let state = Arc::new(MonitorState::new());
        state.record(held_input());
        let worker = state.clone();
        std::thread::Builder::new()
            .name("native-drop-input".into())
            .spawn(move || {
                if let Err(error) = pump(&worker) {
                    worker.failed.store(true, Ordering::Release);
                    tracing::warn!(%error, "prepared native drop input monitor unavailable");
                }
                worker.finished.store(true, Ordering::Release);
                tracing::info!("prepared native drop input monitor stopped and unhooked");
            })
            .context("start prepared drop input monitor")?;
        Ok(Self { state })
    }
    pub fn poll(&self) -> NativeDropInputStatus {
        self.state.status(self.state.elapsed_ms(), held_input())
    }
}
impl Drop for NativeDropInputGuard {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        let thread = self.state.thread.load(Ordering::Acquire);
        if thread != 0 {
            unsafe {
                let _ = PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        // Never join on the capture loop. The private pump wakes within 25ms;
        // Hooks RAII unhooks both handles before its state/thread finishes.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prepared_stationary_heartbeat_applied_after_remote_flag_reset_is_not_input() {
        let state = MonitorState::new();
        state.ready.store(true, Ordering::Release);
        // Arbitrarily delayed stationary ABS packets are always WM_MOUSEMOVE;
        // no point, timestamp, injection flag, or main-loop flag is consulted.
        for _ in 0..1000 {
            state.record(meaningful_mouse(0, WM_MOUSEMOVE));
        }
        assert_eq!(state.status(100, false), NativeDropInputStatus::Clear);
        state.record(meaningful_mouse(0, WM_MOUSEWHEEL));
        assert_eq!(state.status(100, false), NativeDropInputStatus::Interrupted);
    }
    #[test]
    fn clicks_keys_and_both_wheels_interrupt_but_movement_and_releases_do_not() {
        for message in [
            WM_MOUSEMOVE,
            WM_LBUTTONUP,
            WM_RBUTTONUP,
            WM_MBUTTONUP,
            WM_XBUTTONUP,
        ] {
            assert!(!meaningful_mouse(0, message));
        }
        for message in [
            WM_LBUTTONDOWN,
            WM_RBUTTONDOWN,
            WM_MBUTTONDOWN,
            WM_XBUTTONDOWN,
            WM_MOUSEWHEEL,
            WM_MOUSEHWHEEL,
        ] {
            assert!(meaningful_mouse(0, message));
            assert!(!meaningful_mouse(-1, message));
        }
        for message in [WM_KEYDOWN, WM_SYSKEYDOWN] {
            assert!(meaningful_keyboard(0, message));
            assert!(!meaningful_keyboard(-1, message));
        }
        for message in [WM_KEYUP, WM_SYSKEYUP] {
            assert!(!meaningful_keyboard(0, message));
        }
    }
    #[test]
    fn held_input_startup_and_monitor_failures_are_closed() {
        let state = MonitorState::new();
        assert_eq!(state.status(0, false), NativeDropInputStatus::Starting);
        assert_eq!(state.status(0, true), NativeDropInputStatus::Interrupted);
        assert_eq!(
            state.status(2001, false),
            NativeDropInputStatus::Unavailable
        );
        state.ready.store(true, Ordering::Release);
        assert_eq!(state.status(501, false), NativeDropInputStatus::Unavailable);
        state.heartbeat_ms.store(500, Ordering::Release);
        assert_eq!(state.status(501, false), NativeDropInputStatus::Clear);
        state.finished.store(true, Ordering::Release);
        assert_eq!(state.status(501, false), NativeDropInputStatus::Unavailable);
    }
    #[test]
    fn stopping_monitor_ignores_own_later_drag_bootstrap() {
        let state = MonitorState::new();
        state.stop.store(true, Ordering::Release);
        state.record(meaningful_mouse(0, WM_LBUTTONDOWN));
        assert!(!state.interrupted.load(Ordering::Acquire));
    }
    #[test]
    fn short_press_and_runtime_failure_remain_latched() {
        let state = MonitorState::new();
        state.ready.store(true, Ordering::Release);
        state.record(meaningful_keyboard(0, WM_KEYDOWN));
        state.record(meaningful_keyboard(0, WM_KEYUP));
        assert_eq!(state.status(0, false), NativeDropInputStatus::Interrupted);
        state.failed.store(true, Ordering::Release);
        state.heartbeat_ms.store(1000, Ordering::Release);
        assert_eq!(
            state.status(1000, false),
            NativeDropInputStatus::Unavailable
        );
    }
    #[test]
    #[ignore = "installs temporary desktop hooks; run only in the coordinated native fixture"]
    fn native_input_monitor_installs_and_unhooks_without_input_injection() {
        let guard = NativeDropInputGuard::new().unwrap();
        let deadline = Instant::now() + START_LIMIT;
        while guard.poll() == NativeDropInputStatus::Starting && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(guard.poll(), NativeDropInputStatus::Clear);
        let state = guard.state.clone();
        drop(guard);
        let deadline = Instant::now() + START_LIMIT;
        while !state.finished.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // finished is published only after pump locals (both hook handles)
        // unwind. No callback/worker remains tied to this retired lease.
        assert!(state.finished.load(Ordering::Acquire));
        assert!(!state.failed.load(Ordering::Acquire));
        assert!(state.stop.load(Ordering::Acquire));
    }
}
