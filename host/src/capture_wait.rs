//! Demand-driven DDA acquisition. The worker never uses a D3D immediate context.
//!
//! One request waits for one frame; timeout retries remain on the worker. A
//! delivered frame owns its texture and duplication lease. ReleaseFrame is
//! deferred until that frame is dropped and another request arrives (or the
//! waiter is disabled), avoiding an acquire/release loop on an idle desktop.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D, D3D11_TEXTURE2D_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutputDuplication, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO,
};
use windows_capture::dxgi_duplication_api::Error as CaptureError;

use crate::activity::Activity;

const ACQUIRE_TIMEOUT_MS: u32 = 100;

/// A notification-only observer survives cursor-hook parking. It never
/// acquires a DDA frame, samples pixels, or polls a healthy output.
pub struct CaptureInvalidation {
    state: Arc<InvalidationState>,
    thread_id: u32,
    #[cfg(test)]
    window: usize,
    thread: Option<JoinHandle<()>>,
}

struct InvalidationState {
    invalidated: AtomicBool,
    display_state: std::sync::atomic::AtomicU32,
    activity: Mutex<Activity>,
}

impl InvalidationState {
    fn notify(&self) {
        if !self.invalidated.swap(true, Ordering::AcqRel) {
            self.activity.lock().unwrap().notify();
        }
    }

    fn record_display_state(&self, display_state: u32) {
        let previous = self.display_state.swap(display_state, Ordering::AcqRel);
        // The initial ON notification and ON-to-dim transitions do not
        // invalidate duplication. Sleep and wake must invalidate it.
        if previous != display_state && (previous == 0 || display_state == 0) {
            self.notify();
        }
    }
}

thread_local! {
    static INVALIDATION_STATE: std::cell::RefCell<Option<Arc<InvalidationState>>> = const {
        std::cell::RefCell::new(None)
    };
}

// Keep the existing windows feature set. These user32 exports accept a
// window recipient; their C signatures match the Power module projections.
#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterPowerSettingNotification(
        recipient: windows::Win32::Foundation::HANDLE,
        setting: *const windows::core::GUID,
        flags: u32,
    ) -> windows::Win32::Foundation::HANDLE;
    fn UnregisterPowerSettingNotification(registration: windows::Win32::Foundation::HANDLE) -> i32;
}

#[repr(C)]
struct DisplayPowerSettingHeader {
    setting: windows::core::GUID,
    data_length: u32,
}

unsafe extern "system" fn invalidation_window(
    window: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::*;
    INVALIDATION_STATE.with(|state| {
        let state = state.borrow();
        let Some(state) = state.as_ref() else { return };
        if message == WM_DISPLAYCHANGE {
            state.notify();
        } else if message == WM_POWERBROADCAST {
            if wparam.0 == PBT_POWERSETTINGCHANGE as usize && lparam.0 != 0 {
                let payload = lparam.0 as *const u8;
                let setting = payload.cast::<DisplayPowerSettingHeader>().read_unaligned();
                if setting.setting
                    == windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE
                    && setting.data_length == std::mem::size_of::<u32>() as u32
                {
                    let display_state = payload
                        .add(std::mem::size_of::<DisplayPowerSettingHeader>())
                        .cast::<u32>()
                        .read_unaligned();
                    state.record_display_state(display_state);
                }
            } else if matches!(
                wparam.0 as u32,
                PBT_APMSUSPEND | PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND
            ) {
                state.notify();
            }
        }
    });
    DefWindowProcW(window, message, wparam, lparam)
}

unsafe extern "system" fn invalidation_desktop(
    _: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    _: u32,
    _: windows::Win32::Foundation::HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    INVALIDATION_STATE.with(|state| {
        if let Some(state) = state.borrow().as_ref() {
            state.notify();
        }
    });
}

impl CaptureInvalidation {
    pub fn new(activity: Activity) -> io::Result<Self> {
        use windows::core::w;
        use windows::Win32::Foundation::{GetLastError, HINSTANCE};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::System::Threading::GetCurrentThreadId;
        use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent};
        use windows::Win32::UI::WindowsAndMessaging::*;
        let state = Arc::new(InvalidationState {
            invalidated: AtomicBool::new(false),
            display_state: std::sync::atomic::AtomicU32::new(u32::MAX),
            activity: Mutex::new(activity),
        });
        let worker = state.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread = thread::Builder::new().name("capture-invalidation".into()).spawn(move || unsafe {
            INVALIDATION_STATE.with(|state| *state.borrow_mut() = Some(worker));
            let setup = (|| -> windows::core::Result<_> {
                let module = GetModuleHandleW(None)?;
                let instance = HINSTANCE(module.0);
                let class = w!("BubblePlayCaptureInvalidation");
                let definition = WNDCLASSW {
                    lpfnWndProc: Some(invalidation_window),
                    hInstance: instance,
                    lpszClassName: class,
                    ..Default::default()
                };
                if RegisterClassW(&definition) == 0 && GetLastError().0 != 1410 {
                    return Err(windows::core::Error::from_thread());
                }
                // Broadcast display/power messages require a top-level
                // window; message-only HWNDs do not receive them. No SHOW
                // style is used, so this window never appears to the user.
                let window = CreateWindowExW(
                    WINDOW_EX_STYLE::default(), class, w!(""), WINDOW_STYLE::default(),
                    0, 0, 0, 0, None, None, Some(instance), None,
                )?;
                let hook = SetWinEventHook(
                    EVENT_SYSTEM_DESKTOPSWITCH, EVENT_SYSTEM_DESKTOPSWITCH, None,
                    Some(invalidation_desktop), 0, 0, WINEVENT_OUTOFCONTEXT,
                );
                if hook.is_invalid() {
                    let error = windows::core::Error::from_thread();
                    let _ = DestroyWindow(window);
                    return Err(error);
                }
                let power = RegisterPowerSettingNotification(
                    windows::Win32::Foundation::HANDLE(window.0),
                    &windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE, 0,
                );
                if power.is_invalid() {
                    tracing::warn!("display power notifications unavailable; display/desktop notifications remain active");
                }
                Ok((window, hook, power))
            })();
            let (window, hook, power) = match setup {
                Ok(resources) => resources,
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return;
                }
            };
            let _ = ready_tx.send(Ok((GetCurrentThreadId(), window.0 as usize)));
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            if !power.is_invalid() {
                let _ = UnregisterPowerSettingNotification(power);
            }
            let _ = UnhookWinEvent(hook);
            let _ = DestroyWindow(window);
        })?;
        let (thread_id, _window) = match ready_rx.recv() {
            Ok(Ok(identity)) => identity,
            other => {
                let _ = thread.join();
                return Err(io::Error::other(format!(
                    "capture notification startup: {other:?}"
                )));
            }
        };
        Ok(Self {
            state,
            thread_id,
            #[cfg(test)]
            window: _window,
            thread: Some(thread),
        })
    }

    pub fn take(&self) -> bool {
        self.state.invalidated.swap(false, Ordering::AcqRel)
    }

    pub fn set_activity(&self, activity: Activity) {
        let mut notifier = self.state.activity.lock().unwrap();
        *notifier = activity;
        if self.state.invalidated.load(Ordering::Acquire) {
            notifier.notify();
        }
    }

    pub fn display_is_off(&self) -> bool {
        self.state.display_state.load(Ordering::Acquire) == 0
    }
}

impl Drop for CaptureInvalidation {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                    self.thread_id,
                    windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                    windows::Win32::Foundation::WPARAM::default(),
                    windows::Win32::Foundation::LPARAM::default(),
                );
            }
            let _ = thread.join();
        }
    }
}

trait Backend: Send + 'static {
    type Frame: Send;
    fn acquire(&mut self) -> Result<Option<Self::Frame>, CaptureError>;
    fn release(&mut self) -> Result<(), CaptureError>;
}

struct NativeBackend {
    duplication: IDXGIOutputDuplication,
    device: ID3D11Device,
}

struct NativeFrame {
    texture: ID3D11Texture2D,
    device: ID3D11Device,
    duplication: IDXGIOutputDuplication,
    info: DXGI_OUTDUPL_FRAME_INFO,
    desc: D3D11_TEXTURE2D_DESC,
}

fn windows_error(error: windows::core::Error) -> CaptureError {
    if error.code() == DXGI_ERROR_ACCESS_LOST {
        CaptureError::AccessLost
    } else {
        CaptureError::WindowsError(error)
    }
}

impl Backend for NativeBackend {
    type Frame = NativeFrame;

    fn acquire(&mut self) -> Result<Option<NativeFrame>, CaptureError> {
        // Demand may have been armed before a desktop switch, or may have
        // retried after its preceding timeout. Check current Default at the
        // actual acquisition boundary, never a cached producer observation.
        if !crate::desktop_session::input_desktop_is_default() {
            return Err(CaptureError::WindowsError(
                windows::core::Error::from_hresult(E_ACCESSDENIED),
            ));
        }
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        match unsafe {
            self.duplication
                .AcquireNextFrame(ACQUIRE_TIMEOUT_MS, &mut info, &mut resource)
        } {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(error) => return Err(windows_error(error)),
        }
        // Once Acquire succeeds every path must release exactly once. Failed
        // resource conversion cannot leave an untracked outstanding frame.
        let texture = resource
            .ok_or_else(|| windows::core::Error::from_hresult(HRESULT(0x8000FFFFu32 as i32)))
            .and_then(|resource| resource.cast::<ID3D11Texture2D>());
        let texture = match texture {
            Ok(texture) => texture,
            Err(error) => {
                self.release()?;
                return Err(windows_error(error));
            }
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        Ok(Some(NativeFrame {
            texture,
            device: self.device.clone(),
            duplication: self.duplication.clone(),
            info,
            desc,
        }))
    }

    fn release(&mut self) -> Result<(), CaptureError> {
        unsafe { self.duplication.ReleaseFrame() }.map_err(windows_error)
    }
}

struct State<B: Backend> {
    enabled: bool,
    requested: bool,
    request_at: Instant,
    live_frame: bool,
    shutdown: bool,
    failed: bool,
    response: Option<Result<LeasedFrame<B>, CaptureError>>,
}

struct Shared<B: Backend> {
    state: Mutex<State<B>>,
    wake: Condvar,
    activity: Activity,
}

struct Lease<B: Backend> {
    backend: Arc<Mutex<B>>,
    acquired: AtomicBool,
    live: AtomicBool,
    shared: Weak<Shared<B>>,
}

impl<B: Backend> Lease<B> {
    fn release(&self) -> Result<(), CaptureError> {
        // A failed ReleaseFrame is surfaced, never retried against the same
        // acquire. Recovery must discard the complete duplication instead.
        if self.acquired.swap(false, Ordering::AcqRel) {
            self.backend.lock().unwrap().release()?;
        }
        Ok(())
    }
}

impl<B: Backend> Drop for Lease<B> {
    fn drop(&mut self) {
        // Defensive fallback only when a caller drops the waiter while still
        // retaining an owned frame. Its texture has been dropped by then.
        if let Err(error) = self.release() {
            tracing::warn!(?error, "DDA frame lease release failed during final drop");
        }
    }
}

struct LeasedFrame<B: Backend> {
    frame: Option<B::Frame>,
    lease: Arc<Lease<B>>,
}

impl<B: Backend> Drop for LeasedFrame<B> {
    fn drop(&mut self) {
        // Drop texture and duplication references before allowing ReleaseFrame.
        drop(self.frame.take());
        self.lease.live.store(false, Ordering::Release);
        if let Some(shared) = self.lease.shared.upgrade() {
            // Synchronize with the condition-variable predicate to prevent a
            // frame drop between the worker's predicate check and its sleep.
            let mut state = shared.state.lock().unwrap();
            state.live_frame = false;
            shared.wake.notify_one();
        }
    }
}

struct Waiter<B: Backend> {
    shared: Arc<Shared<B>>,
    thread: Option<JoinHandle<Result<(), CaptureError>>>,
}

impl<B: Backend> Waiter<B> {
    fn new(backend: B, activity: Activity) -> Result<Self, CaptureError> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                enabled: true,
                requested: false,
                request_at: Instant::now(),
                live_frame: false,
                shutdown: false,
                failed: false,
                response: None,
            }),
            wake: Condvar::new(),
            activity,
        });
        let worker_shared = shared.clone();
        let backend = Arc::new(Mutex::new(backend));
        let thread = thread::Builder::new()
            .name("dda-wait".into())
            .spawn(move || run(backend, worker_shared))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    fn set_enabled(&self, enabled: bool) {
        let response = {
            let mut state = self.shared.state.lock().unwrap();
            if state.shutdown {
                return;
            }
            if state.enabled == enabled {
                return;
            }
            state.enabled = enabled;
            if !enabled {
                state.requested = false;
                if matches!(state.response, Some(Ok(_))) {
                    state.response.take()
                } else {
                    None
                }
            } else {
                None
            }
        };
        // LeasedFrame::drop locks shared state, so drain outside that lock.
        drop(response);
        self.shared.wake.notify_one();
    }

    fn request_frame(&self) {
        self.request_frame_at(Instant::now());
    }

    fn request_frame_at(&self, not_before: Instant) {
        let mut state = self.shared.state.lock().unwrap();
        if state.enabled
            && !state.shutdown
            && !state.failed
            && !state.live_frame
            && state.response.is_none()
        {
            let deadline = if state.requested {
                state.request_at.min(not_before)
            } else {
                not_before
            };
            let changed = !state.requested || deadline != state.request_at;
            state.requested = true;
            state.request_at = deadline;
            if changed {
                self.shared.wake.notify_one();
            }
        }
    }

    fn poll(&self) -> Result<Option<LeasedFrame<B>>, CaptureError> {
        self.shared
            .state
            .lock()
            .unwrap()
            .response
            .take()
            .transpose()
    }

    fn stop(&mut self) -> Result<(), CaptureError> {
        if self.thread.is_none() {
            return Ok(());
        }
        // First drop any queued frame. Announcing shutdown before that drop
        // could race the worker into reporting it as a caller-held live frame.
        self.set_enabled(false);
        let thread = self.thread.take().unwrap();
        let response = {
            let mut state = self.shared.state.lock().unwrap();
            state.shutdown = true;
            state.enabled = false;
            state.requested = false;
            state.response.take()
        };
        let pending_error = match response {
            Some(Err(error)) => Some(error),
            other => {
                drop(other);
                None
            }
        };
        self.shared.wake.notify_one();
        thread
            .join()
            .map_err(|_| CaptureError::IoError(io::Error::other("DDA waiter panicked")))??;
        if let Some(error) = pending_error {
            return Err(error);
        }
        Ok(())
    }
}

impl<B: Backend> Drop for Waiter<B> {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::warn!(?error, "DDA waiter shutdown failed");
        }
    }
}

fn run<B: Backend>(backend: Arc<Mutex<B>>, shared: Arc<Shared<B>>) -> Result<(), CaptureError> {
    let mut held: Option<Arc<Lease<B>>> = None;
    loop {
        let state = shared.state.lock().unwrap();
        if held
            .as_ref()
            .is_some_and(|lease| !lease.live.load(Ordering::Acquire))
            && (state.requested || !state.enabled || state.shutdown)
        {
            drop(state);
            if let Err(error) = held.take().unwrap().release() {
                let mut state = shared.state.lock().unwrap();
                if state.shutdown {
                    return Err(error);
                }
                state.failed = true;
                state.requested = false;
                state.response = Some(Err(error));
                drop(state);
                shared.activity.notify();
            }
            continue;
        }
        if state.shutdown {
            // Owned frames must be dropped before stopping/recreating capture.
            // Reject misuse without waiting indefinitely for the owner. The
            // final lease drop still releases once, safely after its texture.
            return if held.is_some() {
                Err(CaptureError::IoError(io::Error::other(
                    "DDA waiter stopped with a live frame",
                )))
            } else {
                Ok(())
            };
        }
        if !state.enabled || !state.requested || state.response.is_some() || held.is_some() {
            drop(shared.wake.wait(state).unwrap());
            continue;
        }
        let delay = state.request_at.saturating_duration_since(Instant::now());
        if !delay.is_zero() {
            drop(shared.wake.wait_timeout(state, delay).unwrap());
            continue;
        }
        drop(state);
        let result = backend.lock().unwrap().acquire();
        let mut discarded_frame = None;
        let mut state = shared.state.lock().unwrap();
        match result {
            Ok(None) => {} // bounded blocking retry, only while still armed
            Ok(Some(frame)) => {
                let lease = Arc::new(Lease {
                    backend: backend.clone(),
                    acquired: AtomicBool::new(true),
                    live: AtomicBool::new(false),
                    shared: Arc::downgrade(&shared),
                });
                if state.enabled && state.requested && !state.shutdown {
                    lease.live.store(true, Ordering::Release);
                    state.live_frame = true;
                    state.response = Some(Ok(LeasedFrame {
                        frame: Some(frame),
                        lease: lease.clone(),
                    }));
                    state.requested = false;
                    shared.activity.notify();
                } else {
                    discarded_frame = Some(frame);
                }
                held = Some(lease);
            }
            Err(error) => {
                if state.shutdown {
                    return Err(error);
                }
                state.failed = true;
                state.requested = false;
                state.response = Some(Err(error));
                shared.activity.notify();
            }
        }
        drop(state);
        // Native frame destruction can enter the graphics driver. Keep the
        // producer's state accessible even if that cleanup stalls.
        drop(discarded_frame);
    }
}

/// Owns the single outstanding DDA frame until the consumer has finished.
pub struct RawDesktopFrame(LeasedFrame<NativeBackend>);

impl RawDesktopFrame {
    fn frame(&self) -> &NativeFrame {
        self.0.frame.as_ref().expect("live DDA frame")
    }
    pub fn width(&self) -> u32 {
        self.frame().desc.Width
    }
    pub fn height(&self) -> u32 {
        self.frame().desc.Height
    }
    pub fn device(&self) -> &ID3D11Device {
        &self.frame().device
    }
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.frame().texture
    }
    pub fn texture_desc(&self) -> &D3D11_TEXTURE2D_DESC {
        &self.frame().desc
    }
    pub fn frame_info(&self) -> &DXGI_OUTDUPL_FRAME_INFO {
        &self.frame().info
    }
    pub fn duplication(&self) -> &IDXGIOutputDuplication {
        &self.frame().duplication
    }
}

pub struct DdaWaiter(Waiter<NativeBackend>);

impl DdaWaiter {
    /// The original API must never acquire while this waiter owns duplication.
    pub fn new(
        duplication: IDXGIOutputDuplication,
        device: ID3D11Device,
        activity: Activity,
    ) -> Result<Self, CaptureError> {
        Waiter::new(
            NativeBackend {
                duplication,
                device,
            },
            activity,
        )
        .map(Self)
    }
    pub fn set_enabled(&self, enabled: bool) {
        self.0.set_enabled(enabled);
    }
    pub fn request_frame(&self) {
        self.0.request_frame();
    }
    /// Arm one future acquisition without waking the producer at admission.
    /// An earlier request advances the deadline; completed/live responses
    /// cannot be armed again until the consumer has dropped the frame.
    pub fn request_frame_at(&self, not_before: Instant) {
        self.0.request_frame_at(not_before);
    }
    pub fn poll(&self) -> Result<Option<RawDesktopFrame>, CaptureError> {
        self.0.poll().map(|frame| frame.map(RawDesktopFrame))
    }
    /// Drop all delivered frames first. Successful stop joins the bounded
    /// AcquireNextFrame call, releases pending ownership and drops all worker
    /// duplication references before the caller recreates the original API.
    pub fn stop(&mut self) -> Result<(), CaptureError> {
        self.0.stop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::time::{Duration, Instant};

    #[test]
    fn hidden_notification_window_wakes_owner_and_shuts_down() {
        let activity = Activity::new().unwrap();
        let observer = CaptureInvalidation::new(activity.clone()).unwrap();
        observer.take();
        let short_setting = DisplayPowerSettingHeader {
            setting: windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE,
            data_length: 0,
        };
        unsafe {
            // An unrelated/short notification has only the header. The
            // handler must not read a display-state DWORD beyond it.
            windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                windows::Win32::Foundation::HWND(observer.window as *mut _),
                windows::Win32::UI::WindowsAndMessaging::WM_POWERBROADCAST,
                Some(windows::Win32::Foundation::WPARAM(
                    windows::Win32::UI::WindowsAndMessaging::PBT_POWERSETTINGCHANGE as usize,
                )),
                Some(windows::Win32::Foundation::LPARAM(
                    &short_setting as *const _ as isize,
                )),
            );
            windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                windows::Win32::Foundation::HWND(observer.window as *mut _),
                windows::Win32::UI::WindowsAndMessaging::WM_DISPLAYCHANGE,
                None,
                None,
            );
        }
        assert!(observer.take());
        assert!(!observer.take());
        activity.wait(Duration::ZERO, &[]).unwrap();
        drop(observer);
    }

    #[test]
    fn display_power_notifications_coalesce_sleep_and_wake_without_polling() {
        let state = InvalidationState {
            invalidated: AtomicBool::new(false),
            display_state: std::sync::atomic::AtomicU32::new(u32::MAX),
            activity: Mutex::new(Activity::new().unwrap()),
        };
        state.record_display_state(1);
        state.record_display_state(2);
        assert!(!state.invalidated.load(Ordering::Acquire));
        state.record_display_state(0);
        state.record_display_state(0);
        assert!(state.invalidated.swap(false, Ordering::AcqRel));
        assert!(!state.invalidated.swap(false, Ordering::AcqRel));
        state.record_display_state(1);
        assert!(state.invalidated.swap(false, Ordering::AcqRel));
    }

    #[test]
    fn parked_waiter_preserves_release_errors_without_new_acquisition() {
        let mut h = Harness::new();
        h.frames.send(1).unwrap();
        h.waiter.request_frame();
        drop(h.frame());
        h.fail_release.store(true, Ordering::SeqCst);
        h.waiter.set_enabled(false);
        h.activity.wait(Duration::from_secs(2), &[]).unwrap();
        assert!(matches!(h.waiter.poll(), Err(CaptureError::AccessLost)));
        h.waiter.request_frame();
        h.activity.wait(Duration::from_millis(120), &[]).unwrap();
        assert_eq!(h.acquires.load(Ordering::SeqCst), 1);
        assert_eq!(h.releases.load(Ordering::SeqCst), 1);
        h.waiter.stop().unwrap();
    }

    struct MockBackend {
        frames: Receiver<u32>,
        attempts: Sender<()>,
        acquires: Arc<AtomicUsize>,
        releases: Arc<AtomicUsize>,
        fail_release: Arc<AtomicBool>,
    }

    impl Backend for MockBackend {
        type Frame = u32;
        fn acquire(&mut self) -> Result<Option<u32>, CaptureError> {
            self.acquires.fetch_add(1, Ordering::SeqCst);
            self.attempts.send(()).unwrap();
            Ok(self
                .frames
                .recv_timeout(Duration::from_millis(ACQUIRE_TIMEOUT_MS as u64))
                .ok())
        }
        fn release(&mut self) -> Result<(), CaptureError> {
            self.releases.fetch_add(1, Ordering::SeqCst);
            if self.fail_release.load(Ordering::SeqCst) {
                Err(CaptureError::AccessLost)
            } else {
                Ok(())
            }
        }
    }

    struct Harness {
        waiter: Waiter<MockBackend>,
        activity: Activity,
        frames: Sender<u32>,
        attempts: Receiver<()>,
        acquires: Arc<AtomicUsize>,
        releases: Arc<AtomicUsize>,
        fail_release: Arc<AtomicBool>,
    }

    impl Harness {
        fn new() -> Self {
            let (frames, receiver) = mpsc::channel();
            let (attempt_tx, attempts) = mpsc::channel();
            let acquires = Arc::new(AtomicUsize::new(0));
            let releases = Arc::new(AtomicUsize::new(0));
            let fail_release = Arc::new(AtomicBool::new(false));
            let activity = Activity::new().unwrap();
            let waiter = Waiter::new(
                MockBackend {
                    frames: receiver,
                    attempts: attempt_tx,
                    acquires: acquires.clone(),
                    releases: releases.clone(),
                    fail_release: fail_release.clone(),
                },
                activity.clone(),
            )
            .unwrap();
            Self {
                waiter,
                activity,
                frames,
                attempts,
                acquires,
                releases,
                fail_release,
            }
        }

        fn frame(&self) -> LeasedFrame<MockBackend> {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(frame) = self.waiter.poll().unwrap() {
                    self.activity.wait(Duration::ZERO, &[]).unwrap();
                    return frame;
                }
                assert!(Instant::now() < deadline, "frame response deadline");
                self.activity
                    .wait(deadline.saturating_duration_since(Instant::now()), &[])
                    .unwrap();
            }
        }
    }

    #[test]
    fn each_request_has_one_frame_and_release_waits_for_owner_and_next_request() {
        let mut h = Harness::new();
        h.frames.send(1).unwrap();
        // Merely enabling the source performs no acquisition.
        h.activity.wait(Duration::from_millis(20), &[]).unwrap();
        assert_eq!(h.acquires.load(Ordering::SeqCst), 0);
        h.waiter.request_frame();
        let first = h.frame();
        assert_eq!(first.frame, Some(1));
        h.waiter.request_frame();
        h.frames.send(2).unwrap();
        h.activity.wait(Duration::from_millis(20), &[]).unwrap();
        assert_eq!(h.acquires.load(Ordering::SeqCst), 1);
        assert_eq!(h.releases.load(Ordering::SeqCst), 0);
        drop(first);
        h.waiter.request_frame();
        let second = h.frame();
        assert_eq!(second.frame, Some(2));
        assert_eq!(h.releases.load(Ordering::SeqCst), 1);
        drop(second);
        h.activity.wait(Duration::from_millis(20), &[]).unwrap();
        assert_eq!(h.releases.load(Ordering::SeqCst), 1);
        h.waiter.stop().unwrap();
        assert_eq!(h.releases.load(Ordering::SeqCst), 2);
        assert_eq!(h.acquires.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn disabling_an_armed_idle_source_stops_timeout_retries_and_joins() {
        let mut h = Harness::new();
        h.waiter.request_frame();
        h.attempts.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(h.waiter.poll().unwrap().is_none());
        h.waiter.set_enabled(false);
        let started = Instant::now();
        h.waiter.stop().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(h.acquires.load(Ordering::SeqCst), 1);
        assert_eq!(h.releases.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn disable_drains_queued_frame_and_releases_it_before_join_returns() {
        let mut h = Harness::new();
        h.frames.send(1).unwrap();
        h.waiter.request_frame();
        h.activity.wait(Duration::from_secs(2), &[]).unwrap();
        h.waiter.set_enabled(false);
        h.waiter.stop().unwrap();
        assert!(h.waiter.poll().unwrap().is_none());
        assert_eq!(h.releases.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn discarded_frame_cleanup_does_not_hold_shared_state() {
        #[derive(Debug)]
        struct PausedFrame {
            started: Sender<()>,
            resume: Receiver<()>,
            finished: Arc<AtomicBool>,
        }
        impl Drop for PausedFrame {
            fn drop(&mut self) {
                let _ = self.started.send(());
                // Also bound cleanup if the test fails before sending resume.
                let _ = self.resume.recv_timeout(Duration::from_secs(3));
                self.finished.store(true, Ordering::SeqCst);
            }
        }
        struct PausedBackend {
            frames: Receiver<PausedFrame>,
            acquiring: Sender<()>,
            releases: Arc<AtomicUsize>,
        }
        impl Backend for PausedBackend {
            type Frame = PausedFrame;
            fn acquire(&mut self) -> Result<Option<Self::Frame>, CaptureError> {
                let _ = self.acquiring.send(());
                Ok(self.frames.recv_timeout(Duration::from_secs(2)).ok())
            }
            fn release(&mut self) -> Result<(), CaptureError> {
                self.releases.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let (frames_tx, frames_rx) = mpsc::channel();
        let (acquiring_tx, acquiring_rx) = mpsc::channel();
        let (drop_tx, drop_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let drop_finished = Arc::new(AtomicBool::new(false));
        let releases = Arc::new(AtomicUsize::new(0));
        let mut waiter = Waiter::new(
            PausedBackend {
                frames: frames_rx,
                acquiring: acquiring_tx,
                releases: releases.clone(),
            },
            Activity::new().unwrap(),
        )
        .unwrap();
        let shared = waiter.shared.clone();
        let (startup_tx, startup_rx) = mpsc::channel();
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let (locking_tx, locking_rx) = mpsc::channel();
        let (access_tx, access_rx) = mpsc::channel();
        let accessor = thread::spawn(move || {
            let _ = startup_tx.send(());
            if attempt_rx.recv_timeout(Duration::from_secs(10)).is_err() {
                return;
            }
            let _ = locking_tx.send(());
            let state = shared.state.lock().unwrap();
            let _ = access_tx.send(!state.enabled && state.response.is_none());
        });
        // Exclude accessor thread startup from the lock-availability check.
        startup_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        waiter.request_frame();
        acquiring_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        waiter.set_enabled(false);
        frames_tx
            .send(PausedFrame {
                started: drop_tx,
                resume: resume_rx,
                finished: drop_finished.clone(),
            })
            .unwrap();
        drop_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        attempt_tx.send(()).unwrap();
        let attempting_lock = locking_rx.recv_timeout(Duration::from_secs(1));
        let accessible_while_paused = access_rx.recv_timeout(Duration::from_secs(1));
        let still_paused = !drop_finished.load(Ordering::SeqCst);
        // Unpause before asserting so the old locking behavior also cleans up.
        let _ = resume_tx.send(());
        let (cleanup_tx, cleanup_rx) = mpsc::channel();
        let cleanup = thread::spawn(move || {
            let access_result = accessor.join();
            let stop_result = waiter.stop();
            let _ = cleanup_tx.send((access_result, stop_result));
        });
        let (access_result, stop_result) = cleanup_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("discarded frame cleanup deadline");
        drop(cleanup);
        access_result.unwrap();
        stop_result.unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        attempting_lock.expect("accessor lock-attempt deadline");
        assert!(
            matches!(accessible_while_paused, Ok(true)),
            "state access while frame cleanup was paused: {accessible_while_paused:?}"
        );
        assert!(
            still_paused,
            "state access must finish before frame cleanup"
        );
    }

    #[test]
    fn release_error_is_reported_once_without_reacquiring() {
        let mut h = Harness::new();
        h.frames.send(1).unwrap();
        h.waiter.request_frame();
        drop(h.frame());
        h.fail_release.store(true, Ordering::SeqCst);
        h.waiter.request_frame();
        h.activity.wait(Duration::from_secs(2), &[]).unwrap();
        assert!(matches!(h.waiter.poll(), Err(CaptureError::AccessLost)));
        h.waiter.request_frame(); // a failed duplication can never be rearmed
        h.waiter.stop().unwrap();
        assert_eq!(h.releases.load(Ordering::SeqCst), 1);
        assert_eq!(h.acquires.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn shutdown_rejects_live_owner_and_defers_release_until_its_drop() {
        let mut h = Harness::new();
        h.frames.send(1).unwrap();
        h.waiter.request_frame();
        let frame = h.frame();
        assert!(h.waiter.stop().is_err());
        assert_eq!(h.releases.load(Ordering::SeqCst), 0);
        drop(frame);
        assert_eq!(h.releases.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn future_admission_waits_on_worker_and_later_requests_cannot_postpone_it() {
        let mut h = Harness::new();
        h.frames.send(1).unwrap();
        let deadline = Instant::now() + Duration::from_millis(100);
        h.waiter.request_frame_at(deadline);
        h.waiter.request_frame_at(deadline + Duration::from_secs(1));
        assert!(h.attempts.recv_timeout(Duration::from_millis(30)).is_err());
        assert!(h.waiter.poll().unwrap().is_none());
        let frame = h.frame();
        assert!(Instant::now() >= deadline);
        assert_eq!(h.acquires.load(Ordering::SeqCst), 1);
        drop(frame);
        h.waiter.stop().unwrap();
    }

    #[test]
    fn urgent_request_advances_future_admission_and_live_frame_cannot_rearm() {
        let mut h = Harness::new();
        h.frames.send(1).unwrap();
        h.waiter
            .request_frame_at(Instant::now() + Duration::from_secs(10));
        h.waiter.request_frame();
        let frame = h.frame();
        h.waiter.request_frame();
        drop(frame);
        assert!(h.attempts.recv_timeout(Duration::from_millis(20)).is_ok()); // first acquire
        assert!(h.attempts.recv_timeout(Duration::from_millis(20)).is_err());
        assert_eq!(h.acquires.load(Ordering::SeqCst), 1);
        h.waiter.stop().unwrap();
    }
}
