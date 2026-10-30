//! Local capture for the live session, with a Work/Game mode that can change.
//!
//! The shell owns one [`InputCapture`]. [`crate::input::SessionInput`] has no
//! way to change mode, so [`LiveInput`] keeps one of them and replaces it when
//! the mode changes. The replacement is staged in three steps:
//!
//! 1. A request drops the installed capture at once: its keyboard hook,
//!    ClipCursor and held input go before the network releases input.
//! 2. The network's `media-control` `mode` command is written, which releases
//!    input again on the net side. The write bumps `mode_revision`. A new
//!    capture queues its cursor sync at once, so building it earlier would let
//!    that release clear the sync.
//! 3. Only then is the new capture built.
//!
//! A `Stored` mode (no live socket) and a socket that drops while the write is
//! waited for need no write, so the new capture is built at once in those cases.
//! A manually released capture keeps its existing recapture hotkey; replacement
//! waits for the user to recapture, preserving their release intent.
//!
//! The UI thread asks through [`LiveInputControl::set_mode`] from any thread.
//! Steps run on the window thread in [`InputCapture::on_frame`], never inside a
//! message handler.

use std::sync::{atomic::AtomicBool, Arc, Mutex, MutexGuard};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};

use crate::{
    input::{Mode, SessionInput},
    model::StreamMode,
    net::{InputLink, ModeOutcome, SessionControls},
    surface::{InputCapture, Rect},
};

use super::SessionInputState as CaptureStatus;

/// Builds the capture for a mode, with its status handle when it has one.
/// The live client passes [`crate::input::SessionInput`]; tests pass a fake.
pub type CaptureFactory = Box<dyn FnMut(Mode) -> (Box<dyn InputCapture>, Option<CaptureStatus>)>;

/// The window-thread half of the adapter, as the shell sees it.
pub struct LiveInput {
    make: CaptureFactory,
    controls: SessionControls,
    /// None while the capture is dropped and waiting for the mode write.
    current: Option<Box<dyn InputCapture>>,
    /// The request whose drop has already been applied.
    seen_seq: u64,
    control: LiveInputControl,
    /// Read the installed capture's manual release intent without owning its policy.
    released: Box<dyn Fn() -> bool>,
}

/// The latest mode request. Each accepted request gets a new `seq`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Target {
    mode: Mode,
    seq: u64,
    /// The net's number for this request. The new capture waits until the net
    /// has acknowledged it: written, or covered by the start mode of a new
    /// socket. `None` when no write is coming (stored or unchanged).
    ack: Option<u64>,
}

/// The cloneable handle the UI thread and the final report keep. Cheap to clone.
#[derive(Clone)]
pub struct LiveInputControl {
    controls: SessionControls,
    target: Arc<Mutex<Target>>,
    status: Arc<Mutex<Option<CaptureStatus>>>,
}

impl LiveInputControl {
    /// Asks for a pointer mode and sends the matching network mode. The
    /// installed capture changes on a later shell frame, once the write is done.
    pub fn set_mode(&self, mode: Mode) {
        let (outcome, request) = self.controls.request_mode(stream_mode(mode));
        // Already the network's mode, so nothing is sent. A write queued earlier
        // may still be pending, and the current target stays as it is.
        if outcome == ModeOutcome::Unchanged {
            return;
        }
        let mut target = lock(&self.target);
        *target = Target {
            mode,
            seq: target.seq + 1,
            // Queued on a live socket: its own write must be acknowledged.
            // Stored: nothing is written, so the capture may build at once.
            ack: (outcome == ModeOutcome::Queued).then_some(request),
        };
    }

    /// The mode most recently asked for. It is the installed mode once the
    /// next frame has run after the write. Tests read it; the UI does not need it.
    #[cfg(test)]
    pub fn desired_mode(&self) -> Mode {
        lock(&self.target).mode
    }

    /// Status of the capture installed now. It changes with each mode switch,
    /// so read it when needed rather than keeping it.
    pub fn status(&self) -> Option<CaptureStatus> {
        lock(&self.status).clone()
    }
}

impl LiveInput {
    /// Installs the capture for `mode` straight away, as the first shell
    /// input capture. `controls` must be the handle `NetConfig.controls` uses.
    pub fn new(
        mode: Mode,
        controls: SessionControls,
        mut make: CaptureFactory,
    ) -> (Self, LiveInputControl) {
        let (capture, status) = make(mode);
        let control = LiveInputControl {
            controls: controls.clone(),
            target: Arc::new(Mutex::new(Target {
                mode,
                seq: 0,
                ack: None,
            })),
            status: Arc::new(Mutex::new(status)),
        };
        let input = Self {
            make,
            controls,
            current: Some(capture),
            seen_seq: 0,
            control: control.clone(),
            released: Box::new({
                let control = control.clone();
                move || {
                    control
                        .status()
                        .is_some_and(|status| status.snapshot().released)
                }
            }),
        };
        (input, control)
    }

    /// Moves the adapter toward the latest request. Does nothing when the
    /// installed capture is still right and no new request came in.
    fn advance(&mut self) {
        let target = *lock(&self.control.target);
        if target.seq != self.seen_seq {
            // Keep a manually released instance and its existing recapture
            // hotkey. Replacing it would reset SessionInput.requested to true
            // and take input again without a user recapture. Once recaptured,
            // the next frame installs the latest requested mode below.
            if self.current.is_some() && (self.released)() {
                return;
            }
            self.seen_seq = target.seq;
            // A new request: the installed capture goes now, before the write.
            self.current = None;
        }
        if self.current.is_some() {
            return;
        }
        if let Some(request) = target.ack {
            // Still waiting for this request's own write, which is where the net
            // releases input for it. A dropped socket sends no write, so it
            // builds at once.
            if self.controls.mode_acked() < request && self.controls.snapshot().connected {
                return;
            }
        }
        let (capture, status) = (self.make)(target.mode);
        self.current = Some(capture);
        *lock(&self.control.status) = status;
    }
}

impl InputCapture for LiveInput {
    fn on_message(
        &mut self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        self.current.as_mut()?.on_message(hwnd, msg, wparam, lparam)
    }

    fn captured(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|capture| capture.captured())
    }

    fn on_frame(&mut self, hwnd: HWND, video_px: Option<Rect>) {
        self.advance();
        if let Some(capture) = self.current.as_mut() {
            capture.on_frame(hwnd, video_px);
        }
    }
}

/// The live factory: a real [`SessionInput`] per mode, on the session's input
/// mailbox and stop flag, as the windowed session built it before.
pub fn session_input_factory(
    link: InputLink,
    stop: Arc<AtomicBool>,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> CaptureFactory {
    Box::new(move |mode: Mode| {
        // SessionInput's private allocator starts at zero. Release-all does
        // not reset the host watermark, so preserve the per-epoch wire space.
        link.restart_producer();
        let input = SessionInput::new(mode, link.capture_view(), stop.clone(), wake.clone());
        let status = input.status();
        (Box::new(input) as Box<dyn InputCapture>, Some(status))
    })
}

/// The input mode a stream mode (the one the host and UI share) selects.
pub fn input_mode(mode: StreamMode) -> Mode {
    match mode {
        StreamMode::Work => Mode::Work,
        StreamMode::Game => Mode::Game,
    }
}

/// The stream mode an input mode corresponds to, for the network and the UI.
pub fn stream_mode(mode: Mode) -> StreamMode {
    match mode {
        Mode::Work => StreamMode::Work,
        Mode::Game => StreamMode::Game,
    }
}

/// Poisoned locks still hold a valid value here, so the adapter keeps working.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_change_preserves_manual_release_until_the_user_recaptures() {
        use std::sync::atomic::Ordering;
        let log: Log = Arc::default();
        let controls = connected();
        let (mut input, control) = LiveInput::new(Mode::Work, controls.clone(), fake(&log));
        take(&log);
        let released = Arc::new(AtomicBool::new(true));
        input.released = Box::new({
            let released = released.clone();
            move || released.load(Ordering::Relaxed)
        });

        control.set_mode(Mode::Game);
        input.on_frame(no_hwnd(), None);
        assert_eq!(
            take(&log),
            ["frame work"],
            "keep the released capture and its hotkey"
        );
        net_writes(&controls, &log);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["release", "write", "frame work"]);
        assert!(!input.captured());

        released.store(false, Ordering::Relaxed);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["drop work", "make game", "frame game"]);
        assert!(input.captured());
    }

    /// Shared, ordered record of what the adapter and the fake net did.
    type Log = Arc<Mutex<Vec<String>>>;

    const EPOCH: &str = "epoch-1";

    /// Records what the adapter does to it, in order, into the log.
    struct FakeCapture {
        id: &'static str,
        mode: Mode,
        log: Log,
    }

    impl InputCapture for FakeCapture {
        fn on_message(&mut self, _: HWND, _: u32, _: WPARAM, _: LPARAM) -> Option<LRESULT> {
            None
        }
        fn captured(&self) -> bool {
            self.mode == Mode::Game
        }
        fn on_frame(&mut self, _: HWND, _: Option<Rect>) {
            push(&self.log, format!("frame {}", self.id));
        }
    }

    impl Drop for FakeCapture {
        fn drop(&mut self) {
            push(&self.log, format!("drop {}", self.id));
        }
    }

    fn push(log: &Log, entry: String) {
        log.lock().unwrap().push(entry);
    }

    fn take(log: &Log) -> Vec<String> {
        std::mem::take(&mut *log.lock().unwrap())
    }

    fn fake(log: &Log) -> CaptureFactory {
        let log = log.clone();
        Box::new(move |mode: Mode| {
            let id = if mode == Mode::Game { "game" } else { "work" };
            push(&log, format!("make {id}"));
            let capture = FakeCapture {
                id,
                mode,
                log: log.clone(),
            };
            (Box::new(capture) as Box<dyn InputCapture>, None)
        })
    }

    /// A `/ws` socket on an epoch, as after `hello-ok`. Mode writes are queued.
    fn connected() -> SessionControls {
        let controls = SessionControls::new();
        controls.epoch_registered(EPOCH);
        controls
    }

    /// What the net thread does with a queued mode: take it, release input
    /// (clearing the queue), write it, then bump the revision.
    fn net_writes(controls: &SessionControls, log: &Log) {
        let outbound = controls.take_outbound(EPOCH);
        if outbound.mode.is_some() {
            push(log, "release".into());
            push(log, "write".into());
            controls.mark_mode_written(outbound.mode_request);
        }
    }

    fn no_hwnd() -> HWND {
        HWND(std::ptr::null_mut())
    }

    #[test]
    fn the_old_capture_drops_now_and_the_new_one_waits_for_the_write() {
        let log: Log = Arc::default();
        let controls = connected();
        let (mut input, control) = LiveInput::new(Mode::Work, controls.clone(), fake(&log));
        assert_eq!(take(&log), ["make work"]);

        control.set_mode(Mode::Game);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["drop work"], "the old capture goes at once");

        input.on_frame(no_hwnd(), None);
        assert_eq!(
            take(&log),
            Vec::<String>::new(),
            "no build before the write"
        );

        net_writes(&controls, &log);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["release", "write", "make game", "frame game"]);
        assert_eq!(control.desired_mode(), Mode::Game);
    }

    #[test]
    fn a_stored_mode_builds_at_once_because_nothing_is_written() {
        let log: Log = Arc::default();
        let controls = SessionControls::new();
        let (mut input, control) = LiveInput::new(Mode::Work, controls.clone(), fake(&log));
        take(&log);

        control.set_mode(Mode::Game);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["drop work", "make game", "frame game"]);
    }

    #[test]
    fn a_socket_that_drops_while_waiting_builds_without_a_write() {
        let log: Log = Arc::default();
        let controls = connected();
        let (mut input, control) = LiveInput::new(Mode::Work, controls.clone(), fake(&log));
        take(&log);

        control.set_mode(Mode::Game);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["drop work"]);

        controls.epoch_restart();
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["make game", "frame game"]);
    }

    #[test]
    fn an_older_write_that_lands_after_a_newer_request_does_not_build_early() {
        let log: Log = Arc::default();
        let controls = connected();
        let (mut input, control) = LiveInput::new(Mode::Work, controls.clone(), fake(&log));
        take(&log);

        control.set_mode(Mode::Game);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["drop work"]);

        // The net has taken the Game write and is still writing it when the
        // user toggles back to Work.
        let older = controls.take_outbound(EPOCH);
        assert_eq!(older.mode, Some(StreamMode::Game));
        control.set_mode(Mode::Work);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), Vec::<String>::new());

        // The older write lands. The revision moves, but the Work request is not
        // on the wire, so no capture may be built yet.
        push(&log, "release".into());
        push(&log, "write".into());
        controls.mark_mode_written(older.mode_request);
        input.on_frame(no_hwnd(), None);
        assert_eq!(
            take(&log),
            ["release", "write"],
            "no capture before the newer write"
        );

        net_writes(&controls, &log);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["release", "write", "make work", "frame work"]);
    }

    #[test]
    fn a_socket_that_reconnects_between_frames_builds_once_its_start_mode_is_sent() {
        let log: Log = Arc::default();
        let controls = connected();
        let (mut input, control) = LiveInput::new(Mode::Work, controls.clone(), fake(&log));
        take(&log);

        control.set_mode(Mode::Game);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["drop work"]);

        // The socket drops and registers again before any frame sees it
        // disconnected. Its signaling-start carries the Game mode.
        controls.epoch_restart();
        controls.epoch_registered(EPOCH);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["make game", "frame game"]);
    }

    #[test]
    fn a_write_that_lands_before_the_first_frame_still_drops_then_builds() {
        let log: Log = Arc::default();
        let controls = connected();
        let (mut input, control) = LiveInput::new(Mode::Work, controls.clone(), fake(&log));
        take(&log);

        control.set_mode(Mode::Game);
        net_writes(&controls, &log);
        input.on_frame(no_hwnd(), None);
        assert_eq!(
            take(&log),
            ["release", "write", "drop work", "make game", "frame game"]
        );
    }

    #[test]
    fn the_same_mode_asked_again_keeps_waiting_for_the_pending_write() {
        let log: Log = Arc::default();
        let controls = connected();
        let (mut input, control) = LiveInput::new(Mode::Work, controls.clone(), fake(&log));
        take(&log);

        control.set_mode(Mode::Game);
        input.on_frame(no_hwnd(), None);
        control.set_mode(Mode::Game);
        input.on_frame(no_hwnd(), None);
        assert_eq!(
            take(&log),
            ["drop work"],
            "the pending write is still outstanding"
        );

        net_writes(&controls, &log);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["release", "write", "make game", "frame game"]);
    }

    #[test]
    fn rapid_toggles_build_only_the_latest_written_mode() {
        let log: Log = Arc::default();
        let controls = connected();
        let (mut input, control) = LiveInput::new(Mode::Work, controls.clone(), fake(&log));
        take(&log);

        control.set_mode(Mode::Game);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["drop work"]);

        // The outbox holds one mode, so the Game write is replaced by Work.
        control.set_mode(Mode::Work);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), Vec::<String>::new());

        net_writes(&controls, &log);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["release", "write", "make work", "frame work"]);
    }

    #[test]
    fn toggling_back_to_the_installed_mode_still_rebuilds_after_the_write() {
        let log: Log = Arc::default();
        let controls = connected();
        let (mut input, control) = LiveInput::new(Mode::Work, controls.clone(), fake(&log));
        take(&log);

        // Both requests land before any frame. The net still writes the Work
        // command and releases input for it, so the installed Work is rebuilt.
        control.set_mode(Mode::Game);
        control.set_mode(Mode::Work);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["drop work"]);

        net_writes(&controls, &log);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["release", "write", "make work", "frame work"]);
    }

    #[test]
    fn a_request_for_the_installed_mode_with_no_write_keeps_the_capture() {
        let log: Log = Arc::default();
        let controls = connected();
        let (mut input, control) = LiveInput::new(Mode::Work, controls.clone(), fake(&log));
        take(&log);

        control.set_mode(Mode::Work);
        input.on_frame(no_hwnd(), None);
        input.on_frame(no_hwnd(), None);
        assert_eq!(take(&log), ["frame work", "frame work"]);
    }

    #[test]
    fn captured_and_messages_follow_the_installed_capture() {
        let log: Log = Arc::default();
        let controls = connected();
        let (mut input, control) = LiveInput::new(Mode::Work, controls.clone(), fake(&log));
        assert!(!input.captured());
        assert!(input
            .on_message(no_hwnd(), 0, WPARAM(0), LPARAM(0))
            .is_none());

        control.set_mode(Mode::Game);
        input.on_frame(no_hwnd(), None);
        assert!(
            !input.captured(),
            "nothing is installed while the write is awaited"
        );
        net_writes(&controls, &log);
        input.on_frame(no_hwnd(), None);
        assert!(
            input.captured(),
            "the Game capture is the installed one now"
        );
    }

    #[test]
    fn stream_modes_map_to_input_policy() {
        assert_eq!(input_mode(StreamMode::Work), Mode::Work);
        assert_eq!(input_mode(StreamMode::Game), Mode::Game);
        assert_eq!(stream_mode(Mode::Work), StreamMode::Work);
        assert_eq!(stream_mode(Mode::Game), StreamMode::Game);
    }
}
