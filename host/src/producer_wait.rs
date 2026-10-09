//! Cursor notifications wake capture's owner without a 16 ms static-screen poll.
use anyhow::{Context, Result};
use bubbleplay_host::activity::Activity;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::warn;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

struct CursorActivity {
    activity: Activity,
    active: AtomicBool,
    dirty: AtomicBool,
    available: AtomicBool,
}

const CURSOR_CONTROL: u32 = WM_APP + 0x39;

struct NativeHooks {
    mouse: HHOOK,
    events: Vec<HWINEVENTHOOK>,
}

impl NativeHooks {
    fn install() -> Result<Self> {
        let module = unsafe { GetModuleHandleW(None) }.context("cursor wake module")?;
        let mouse = unsafe {
            SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(mouse_changed),
                Some(HINSTANCE(module.0)),
                0,
            )
        }
        .context("cursor movement hook")?;
        let mut hooks = Self {
            mouse,
            events: Vec::new(),
        };
        for (first, last) in [
            // NAMECHANGE includes cursor shape changes after LOCATIONCHANGE.
            (EVENT_OBJECT_SHOW, EVENT_OBJECT_NAMECHANGE),
            (EVENT_SYSTEM_DESKTOPSWITCH, EVENT_SYSTEM_DESKTOPSWITCH),
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        ] {
            let hook = unsafe {
                SetWinEventHook(
                    first,
                    last,
                    None,
                    Some(object_changed),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            if hook.is_invalid() {
                anyhow::bail!("cursor/desktop event hook unavailable");
            }
            hooks.events.push(hook);
        }
        Ok(hooks)
    }
}

impl Drop for NativeHooks {
    fn drop(&mut self) {
        for hook in self.events.drain(..) {
            if !unsafe { UnhookWinEvent(hook) }.as_bool() {
                warn!("cursor event hook removal failed");
            }
        }
        if let Err(error) = unsafe { UnhookWindowsHookEx(self.mouse) } {
            warn!(%error, "cursor movement hook removal failed");
        }
    }
}

fn reconcile_hooks<H>(
    hooks: &mut Option<H>,
    active: bool,
    install: impl FnOnce() -> Result<H>,
) -> Result<()> {
    if active {
        if hooks.is_none() {
            *hooks = Some(install()?);
        }
    } else {
        *hooks = None;
    }
    Ok(())
}

impl CursorActivity {
    fn changed(&self) {
        if self.active.load(Ordering::Acquire) && !self.dirty.swap(true, Ordering::AcqRel) {
            self.activity.notify();
        }
    }
}

thread_local! {
    static CURSOR_ACTIVITY: RefCell<Option<Arc<CursorActivity>>> = const { RefCell::new(None) };
}

unsafe extern "system" fn mouse_changed(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        CURSOR_ACTIVITY.with(|state| {
            if let Some(state) = state.borrow().as_ref() {
                state.changed();
            }
        });
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe extern "system" fn object_changed(
    _: HWINEVENTHOOK,
    event: u32,
    _: HWND,
    object: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    CURSOR_ACTIVITY.with(|state| {
        if let Some(state) = state.borrow().as_ref() {
            if object == OBJID_CURSOR.0 {
                state.changed();
            } else if (event == EVENT_SYSTEM_DESKTOPSWITCH || event == EVENT_SYSTEM_FOREGROUND)
                && state.active.load(Ordering::Acquire)
            {
                state.activity.notify();
            }
        }
    });
}

pub(super) struct CursorWake {
    state: Arc<CursorActivity>,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl CursorWake {
    pub(super) fn new(activity: Activity) -> Result<Self> {
        let state = Arc::new(CursorActivity {
            activity,
            active: AtomicBool::new(false),
            dirty: AtomicBool::new(true),
            available: AtomicBool::new(false),
        });
        let worker = state.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let join = std::thread::Builder::new()
            .name("bubbleplay-cursor-wake".into())
            .spawn(move || {
                let mut message = MSG::default();
                unsafe {
                    let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
                }
                CURSOR_ACTIVITY.with(|state| *state.borrow_mut() = Some(worker.clone()));
                let mut hooks = None;
                let _ = ready_tx.send(unsafe { GetCurrentThreadId() });
                // No hooks or timeout polling while parked. The thread and
                // message queue remain ready for the next activation.
                while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
                    if message.message == CURSOR_CONTROL {
                        let active = worker.active.load(Ordering::Acquire);
                        if let Err(error) = reconcile_hooks(&mut hooks, active, NativeHooks::install) {
                            warn!(%error, "cursor notification activation failed; polling required");
                        }
                        worker.available.store(hooks.is_some() && active, Ordering::Release);
                        // Acknowledge successful activation or publish an
                        // immediate availability failure to the producer.
                        worker.activity.notify();
                    } else {
                        unsafe { DispatchMessageW(&message); }
                    }
                }
                worker.available.store(false, Ordering::Release);
                drop(hooks);
                worker.activity.notify();
                CURSOR_ACTIVITY.with(|state| *state.borrow_mut() = None);
            })
            .context("spawn cursor notification thread")?;
        // Startup has no viewer yet; the notification pump thereafter blocks.
        let thread_id = ready_rx
            .recv_timeout(Duration::from_secs(2))
            .context("start cursor notifications")?;
        Ok(Self {
            state,
            thread_id,
            join: Some(join),
        })
    }

    pub(super) fn set_active(&self, active: bool) {
        if self.state.active.swap(active, Ordering::AcqRel) != active {
            self.state.available.store(false, Ordering::Release);
            if active {
                self.state.dirty.store(true, Ordering::Release);
            }
            if let Err(error) =
                unsafe { PostThreadMessageW(self.thread_id, CURSOR_CONTROL, WPARAM(0), LPARAM(0)) }
            {
                warn!(%error, "cursor notification control failed; polling required");
                self.state.activity.notify();
            }
        }
    }

    /// True only after this activation was acknowledged by the hook owner.
    /// Failed/pending activations must use the existing 16 ms polling path.
    pub(super) fn available(&self) -> bool {
        self.state.active.load(Ordering::Acquire) && self.state.available.load(Ordering::Acquire)
    }

    pub(super) fn take_changed(&self) -> bool {
        self.state.dirty.swap(false, Ordering::AcqRel)
    }

    pub(super) fn changed_pending(&self) -> bool {
        self.state.dirty.load(Ordering::Acquire)
    }
}

impl Drop for CursorWake {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Hooks(Arc<AtomicUsize>);
    impl Drop for Hooks {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn parked_hook_owner_installs_nothing_and_releases_hooks_between_sessions() {
        let released = Arc::new(AtomicUsize::new(0));
        let mut hooks = None;
        reconcile_hooks::<Hooks>(&mut hooks, false, || {
            panic!("parked owner must not install hooks")
        })
        .unwrap();
        reconcile_hooks(&mut hooks, true, || Ok(Hooks(released.clone()))).unwrap();
        reconcile_hooks(&mut hooks, true, || {
            panic!("unchanged active state reuses hooks")
        })
        .unwrap();
        assert!(hooks.is_some());
        reconcile_hooks(&mut hooks, false, || {
            panic!("pause must only release hooks")
        })
        .unwrap();
        assert!(hooks.is_none());
        assert_eq!(released.load(Ordering::SeqCst), 1);
        reconcile_hooks(&mut hooks, true, || Ok(Hooks(released.clone()))).unwrap();
        assert!(hooks.is_some());
        reconcile_hooks(&mut hooks, false, || {
            panic!("second pause must release recreated hooks")
        })
        .unwrap();
        assert_eq!(released.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn failed_hook_reactivation_leaves_no_available_hook_set() {
        let released = Arc::new(AtomicUsize::new(0));
        let mut hooks: Option<Hooks> = None;
        let failed = reconcile_hooks(&mut hooks, true, || {
            let _partial = Hooks(released.clone());
            anyhow::bail!("injected incomplete hook installation");
        });
        assert!(failed.is_err());
        assert!(hooks.is_none());
        assert_eq!(released.load(Ordering::SeqCst), 1);
        reconcile_hooks(&mut hooks, true, || Ok(Hooks(released.clone()))).unwrap();
        assert!(hooks.is_some());
    }
}
