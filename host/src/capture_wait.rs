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
                    drop(frame);
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
