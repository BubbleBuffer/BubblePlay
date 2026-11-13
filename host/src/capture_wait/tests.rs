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
    // An acquire attempt or a frame is only legal once the deadline has
    // passed, however late the scheduler runs this thread: assert the order
    // of events, not a wall-clock window.
    let wait = deadline.saturating_duration_since(Instant::now());
    if h.attempts.recv_timeout(wait).is_ok() {
        assert!(Instant::now() >= deadline, "attempt before the deadline");
    }
    let early = h.waiter.poll().unwrap();
    if early.is_some() {
        assert!(Instant::now() >= deadline, "frame before the deadline");
    }
    let frame = early.unwrap_or_else(|| h.frame());
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
    h.attempts.recv_timeout(Duration::from_secs(2)).unwrap(); // first acquire
    assert!(h.attempts.recv_timeout(Duration::from_millis(20)).is_err());
    assert_eq!(h.acquires.load(Ordering::SeqCst), 1);
    h.waiter.stop().unwrap();
}
