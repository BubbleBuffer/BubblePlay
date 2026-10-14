//! Foreground Game keyboard capture. The callback has no locks, allocation,
//! socket/UI calls, or logging: copy a key into a bounded SPSC ring and return.
//! The hook's own message loop wakes the UI after callbacks have returned.
use anyhow::{Context, Result};
use std::{
    cell::{Cell, UnsafeCell},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::WindowsAndMessaging::*,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyEvent {
    pub vk: u16,
    pub scan: u16,
    pub extended: bool,
    pub up: bool,
}

const DEPTH: usize = 4096;
struct Queue {
    slots: Box<[UnsafeCell<KeyEvent>; DEPTH]>,
    head: AtomicUsize,
    tail: AtomicUsize,
}
// Exactly one producer (the hook thread) and one consumer (the UI thread).
// Release/acquire head publishes the slot; tail prevents overwrite until read.
unsafe impl Sync for Queue {}
impl Queue {
    fn new() -> Self {
        Self {
            slots: Box::new(std::array::from_fn(
                |_| UnsafeCell::new(KeyEvent::default()),
            )),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }
    fn push(&self, event: KeyEvent) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        if head.wrapping_sub(self.tail.load(Ordering::Acquire)) >= DEPTH {
            return false;
        }
        unsafe {
            *self.slots[head % DEPTH].get() = event;
        }
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }
    fn pop(&self) -> Option<KeyEvent> {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail == self.head.load(Ordering::Acquire) {
            return None;
        }
        let key = unsafe { *self.slots[tail % DEPTH].get() };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(key)
    }
    fn pending(&self) -> bool {
        self.tail.load(Ordering::Acquire) != self.head.load(Ordering::Acquire)
    }
}
struct Shared {
    active: AtomicBool,
    overflow: AtomicBool,
    stop: AtomicBool,
    queue: Queue,
    session_stop: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
}
thread_local! { static CALLBACK: Cell<*const Shared> = const { Cell::new(std::ptr::null()) }; }

unsafe extern "system" fn callback(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && matches!(
            wp.0 as u32,
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
        )
    {
        let swallowed = CALLBACK
            .try_with(|slot| {
                let ptr = slot.get();
                if ptr.is_null() {
                    return false;
                }
                let state = &*ptr;
                if !state.active.load(Ordering::Acquire)
                    || state.session_stop.load(Ordering::Acquire)
                    || !state.ready.load(Ordering::Acquire)
                {
                    return false;
                }
                let key = &*(lp.0 as *const KBDLLHOOKSTRUCT);
                let event = KeyEvent {
                    vk: key.vkCode as u16,
                    scan: key.scanCode as u16,
                    extended: key.flags.contains(LLKHF_EXTENDED),
                    up: key.flags.contains(LLKHF_UP),
                };
                if state.queue.push(event) {
                    true
                } else {
                    // Fail open on overflow; the message loop wakes the UI to
                    // release all and remove capture, rather than losing a key-up.
                    state.overflow.store(true, Ordering::Release);
                    state.active.store(false, Ordering::Release);
                    false
                }
            })
            .unwrap_or(false);
        if swallowed {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

struct Installed(HHOOK);
impl Drop for Installed {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.0);
        }
        CALLBACK.with(|c| c.set(std::ptr::null()));
    }
}

pub struct KeyboardHook {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    id: u32,
}
impl KeyboardHook {
    pub fn install(
        hwnd: HWND,
        wake: Arc<dyn Fn() + Send + Sync>,
        session_stop: Arc<AtomicBool>,
        ready: Arc<AtomicBool>,
    ) -> Result<Self> {
        let shared = Arc::new(Shared {
            active: AtomicBool::new(false),
            overflow: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            queue: Queue::new(),
            session_stop,
            ready,
        });
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        // HWND is only compared, never dereferenced. Avoid moving its raw
        // pointer type across threads; the UI retains window lifetime.
        let window = hwnd.0 as usize;
        let worker_shared = shared.clone();
        let handle = thread::Builder::new()
            .name("bubbleplay-keyboard-hook".into())
            .spawn(move || {
                let state = worker_shared;
                let run =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        unsafe {
                            let mut message = MSG::default();
                            // Create this thread's message queue before advertising id.
                            let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
                            if GetForegroundWindow().0 as usize != window {
                                anyhow::bail!("capture window is no longer foreground");
                            }
                            let module = GetModuleHandleW(None)?;
                            CALLBACK.with(|slot| slot.set(Arc::as_ptr(&state)));
                            let hook = SetWindowsHookExW(
                                WH_KEYBOARD_LL,
                                Some(callback),
                                Some(module.into()),
                                0,
                            )?;
                            let _installed = Installed(hook);
                            state.active.store(true, Ordering::Release);
                            let _ = tx.send(Ok(GetCurrentThreadId()));
                            while !state.stop.load(Ordering::Acquire)
                                && !state.session_stop.load(Ordering::Acquire)
                                && state.ready.load(Ordering::Acquire)
                            {
                                MsgWaitForMultipleObjectsEx(
                                    None,
                                    8,
                                    QS_ALLINPUT,
                                    MWMO_INPUTAVAILABLE,
                                );
                                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                                    if message.message == WM_QUIT {
                                        state.stop.store(true, Ordering::Release);
                                        break;
                                    }
                                    let _ = TranslateMessage(&message);
                                    DispatchMessageW(&message);
                                }
                                if GetForegroundWindow().0 as usize != window {
                                    state.active.store(false, Ordering::Release);
                                    break; // the guard uninstalls on this thread
                                }
                                if state.queue.pending() || !state.active.load(Ordering::Acquire) {
                                    wake();
                                }
                            }
                            state.active.store(false, Ordering::Release);
                        }
                        Ok(())
                    }));
                state.active.store(false, Ordering::Release);
                match run {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        let _ = tx.send(Err(format!("{error:#}")));
                    }
                    Err(_) => {
                        let _ = tx.send(Err("keyboard hook thread panicked".into()));
                    }
                }
                wake();
            })
            .context("start keyboard hook thread")?;
        match rx.recv() {
            Ok(Ok(id)) => Ok(Self {
                shared,
                thread: Some(handle),
                id,
            }),
            result => {
                shared.stop.store(true, Ordering::Release);
                let _ = handle.join();
                anyhow::bail!("install keyboard hook: {result:?}")
            }
        }
    }
    pub fn pop(&mut self) -> Option<KeyEvent> {
        self.shared.queue.pop()
    }
    pub fn active(&self) -> bool {
        self.shared.active.load(Ordering::Acquire)
    }
    pub fn overflowed(&self) -> bool {
        self.shared.overflow.load(Ordering::Acquire)
    }
}
impl Drop for KeyboardHook {
    fn drop(&mut self) {
        self.shared.active.store(false, Ordering::Release);
        self.shared.stop.store(true, Ordering::Release);
        unsafe {
            let _ = PostThreadMessageW(self.id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_ring_preserves_order_and_wraps_without_overwrite() {
        let q = Queue::new();
        for round in 0..3 {
            for i in 0..DEPTH {
                assert!(q.push(KeyEvent {
                    scan: i as u16,
                    vk: round,
                    ..Default::default()
                }));
            }
            assert!(!q.push(KeyEvent::default()));
            for i in 0..DEPTH {
                assert_eq!(q.pop().unwrap().scan, i as u16);
            }
            assert!(q.pop().is_none());
        }
    }
    #[test]
    fn ring_publication_across_threads() {
        let q = Arc::new(Queue::new());
        let producer = thread::spawn({
            let q = q.clone();
            move || {
                for i in 0..60000u16 {
                    while !q.push(KeyEvent {
                        scan: i,
                        ..Default::default()
                    }) {
                        thread::yield_now();
                    }
                }
            }
        });
        for i in 0..60000u16 {
            loop {
                if let Some(key) = q.pop() {
                    assert_eq!(key.scan, i);
                    break;
                }
                thread::yield_now();
            }
        }
        producer.join().unwrap();
    }
}
