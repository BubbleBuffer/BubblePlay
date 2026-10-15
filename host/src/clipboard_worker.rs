//! Clipboard reads and writes on a worker thread, off the session owner.
//!
//! Clipboard calls can wait on another process (the clipboard owner, a delayed
//! renderer, or a window that is not pumping messages). The session owner
//! submits a [`Command`] and collects a [`Completion`] later, so input and
//! capture never wait on that call.
//!
//! Bounds: both queues hold a few entries, at most `MAX_LIVE_WORKERS` workers
//! exist process-wide, and a worker stuck in a native call keeps its slot until
//! that call returns. Dropping a [`ClipboardWorker`] sets a stop flag and
//! disconnects its channels; it never joins, because the thread may still be
//! inside a foreign Win32 call.
//!
//! Sessions: [`ClipboardWorker::set_epoch`] publishes the current session epoch.
//! A queued command from an older epoch never reaches the native API, and a
//! completion from an older epoch is not reported. A call already running when
//! the epoch changes cannot be stopped; its side effect (for a write, the new
//! clipboard contents) still happens.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::clipboard::{self, ClipboardContent, ClipboardWatcher};

const COMMAND_CAPACITY: usize = 4;
const COMPLETION_CAPACITY: usize = 4;
pub(crate) const WRITE_LIFETIME: Duration = Duration::from_secs(2);
/// Process-wide cap on worker threads, including ones still inside a native call.
pub(crate) const MAX_LIVE_WORKERS: usize = 4;
static LIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);

// Neither enum derives `Debug`: a `Write` carries clipboard text and must never
// reach a log by accident.
pub(crate) enum Command {
    Poll {
        epoch: u64,
        reset: bool,
    },
    Write {
        epoch: u64,
        id: u64,
        text: String,
        deadline: Instant,
    },
}

impl Command {
    fn epoch(&self) -> u64 {
        match self {
            Self::Poll { epoch, .. } | Self::Write { epoch, .. } => *epoch,
        }
    }
}

pub(crate) enum Completion {
    Polled {
        epoch: u64,
        content: Option<ClipboardContent>,
    },
    Written {
        epoch: u64,
        id: u64,
        success: bool,
    },
}

pub(crate) struct ClipboardWorker {
    commands: SyncSender<Command>,
    completions: Receiver<Completion>,
    stop: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
}

/// Native clipboard operations, replaceable in tests.
trait ClipboardBackend: Send + 'static {
    fn poll(&mut self, watcher: &mut ClipboardWatcher, reset: bool) -> Option<ClipboardContent>;
    /// Returns the clipboard sequence number after a successful write.
    fn write(&mut self, text: &str) -> anyhow::Result<u32>;
}

struct SystemBackend;

impl ClipboardBackend for SystemBackend {
    fn poll(&mut self, watcher: &mut ClipboardWatcher, reset: bool) -> Option<ClipboardContent> {
        watcher.poll(reset)
    }

    fn write(&mut self, text: &str) -> anyhow::Result<u32> {
        clipboard::set_text(text)
    }
}

/// One slot of the process-wide worker budget, released when dropped.
struct LiveSlot(&'static AtomicUsize);

impl LiveSlot {
    fn acquire(counter: &'static AtomicUsize, limit: usize) -> Option<Self> {
        let mut current = counter.load(Ordering::Acquire);
        loop {
            if current >= limit {
                return None;
            }
            match counter.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self(counter)),
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for LiveSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ClipboardWorker {
    pub(crate) fn spawn() -> io::Result<Self> {
        Self::spawn_with_budget(SystemBackend, &LIVE_WORKERS, MAX_LIVE_WORKERS)
    }

    /// Starts at epoch 0; the owner calls [`Self::set_epoch`] with its session epoch.
    fn spawn_with_budget<B: ClipboardBackend>(
        backend: B,
        budget: &'static AtomicUsize,
        limit: usize,
    ) -> io::Result<Self> {
        let slot = LiveSlot::acquire(budget, limit)
            .ok_or_else(|| io::Error::other("clipboard worker limit reached"))?;
        let (commands, command_rx) = mpsc::sync_channel(COMMAND_CAPACITY);
        let (completion_tx, completions) = mpsc::sync_channel(COMPLETION_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let epoch = Arc::new(AtomicU64::new(0));
        let worker_stop = Arc::clone(&stop);
        let worker_epoch = Arc::clone(&epoch);
        // Dropping the handle detaches the thread; Drop never joins it.
        thread::Builder::new()
            .name("bubbleplay-clipboard".into())
            .spawn(move || {
                run(
                    backend,
                    command_rx,
                    completion_tx,
                    worker_stop,
                    worker_epoch,
                    slot,
                )
            })?;
        Ok(Self {
            commands,
            completions,
            stop,
            epoch,
        })
    }

    /// Queue a command without waiting. `false` means the queue is full or the
    /// worker has exited; the caller decides what to do with the request.
    pub(crate) fn submit(&self, command: Command) -> bool {
        self.commands.try_send(command).is_ok()
    }

    pub(crate) fn try_recv(&self) -> Option<Completion> {
        self.completions.try_recv().ok()
    }

    /// Publish the current session epoch. Commands and completions stamped with
    /// any other epoch are dropped.
    pub(crate) fn set_epoch(&self, epoch: u64) {
        self.epoch.store(epoch, Ordering::Release);
    }

    #[cfg(test)]
    fn spawn_with<B: ClipboardBackend>(backend: B) -> io::Result<Self> {
        // Native fixtures must not consume the slots used by concurrently
        // running InputSink/proxy tests. LiveSlot has its own cap regression.
        static TEST_WORKERS: AtomicUsize = AtomicUsize::new(0);
        Self::spawn_with_budget(backend, &TEST_WORKERS, usize::MAX)
    }

    #[cfg(test)]
    pub(crate) fn test_channels() -> (Self, Receiver<Command>, SyncSender<Completion>) {
        let (commands, command_rx) = mpsc::sync_channel(COMMAND_CAPACITY);
        let (completion_tx, completions) = mpsc::sync_channel(COMPLETION_CAPACITY);
        (
            Self {
                commands,
                completions,
                stop: Arc::new(AtomicBool::new(false)),
                epoch: Arc::new(AtomicU64::new(0)),
            },
            command_rx,
            completion_tx,
        )
    }
}

impl Drop for ClipboardWorker {
    fn drop(&mut self) {
        // Never join: a native call may stay blocked. The thread exits after
        // that call returns, when it sees the flag or the disconnected channels.
        self.stop.store(true, Ordering::Release);
    }
}

fn run<B: ClipboardBackend>(
    mut backend: B,
    commands: Receiver<Command>,
    completions: SyncSender<Completion>,
    stop: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    _slot: LiveSlot,
) {
    let mut watcher = ClipboardWatcher::default();
    while let Ok(command) = commands.recv() {
        let command_epoch = command.epoch();
        if stop.load(Ordering::Acquire) {
            break;
        }
        // Queued before a session change: never reaches the native API.
        if epoch.load(Ordering::Acquire) != command_epoch {
            continue;
        }
        let completion = match command {
            Command::Poll { reset, .. } => Completion::Polled {
                epoch: command_epoch,
                content: backend.poll(&mut watcher, reset),
            },
            Command::Write {
                id, text, deadline, ..
            } => {
                // A write queued behind a hung renderer must not replace the
                // clipboard long after its paste was abandoned.
                let success = if Instant::now() >= deadline {
                    false
                } else {
                    match backend.write(&text) {
                        Ok(sequence) => {
                            // Our own write must not be reported back as a copy.
                            watcher.note_own_change(sequence);
                            true
                        }
                        Err(error) => {
                            tracing::debug!(?error, "host clipboard write failed");
                            false
                        }
                    }
                };
                Completion::Written {
                    epoch: command_epoch,
                    id,
                    success,
                }
            }
        };
        if stop.load(Ordering::Acquire) {
            break;
        }
        // The session moved on during the native call: do not report it.
        if epoch.load(Ordering::Acquire) != command_epoch {
            continue;
        }
        // Blocks only while the owner has not drained the completion queue.
        // Dropping the worker drops the receiver, which ends this wait.
        if completions.send(completion).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{RecvTimeoutError, Sender, TryRecvError};
    use std::time::{Duration, Instant};
    use windows::core::w;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
        PostQuitMessage, RegisterClassW, SendMessageW, HWND_MESSAGE, MSG, PM_REMOVE,
        WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_QUIT, WNDCLASSW,
    };

    const WM_PROBE: u32 = WM_APP + 0x31;
    /// Stands in for the owner's per-input tolerance in these tests.
    const INPUT_BUDGET: Duration = Duration::from_millis(100);
    /// Upper bound for any single wait in these tests.
    const WAIT: Duration = Duration::from_secs(5);

    /// Blocks in `poll` until released (finite), records written text, and
    /// signals `entered` before each poll's native stand-in.
    // `exited` is only dropped: its disconnect tells the test the thread ended.
    struct ScriptedBackend {
        entered: Sender<()>,
        release: Receiver<()>,
        writes: Sender<String>,
        #[allow(dead_code)]
        exited: Sender<()>,
    }

    impl ClipboardBackend for ScriptedBackend {
        fn poll(&mut self, _: &mut ClipboardWatcher, _: bool) -> Option<ClipboardContent> {
            let _ = self.entered.send(());
            let _ = self.release.recv_timeout(WAIT);
            None
        }

        fn write(&mut self, text: &str) -> anyhow::Result<u32> {
            let _ = self.writes.send(text.to_owned());
            Ok(2)
        }
    }

    /// Writes by sending a message to a probe window; `entered` is signalled
    /// before the send so a test can wait for the foreign call to start.
    // `exited` is only dropped: its disconnect tells the test the thread ended.
    struct WindowWrite {
        window: isize,
        entered: Sender<()>,
        #[allow(dead_code)]
        exited: Sender<()>,
    }

    impl ClipboardBackend for WindowWrite {
        fn poll(&mut self, _: &mut ClipboardWatcher, _: bool) -> Option<ClipboardContent> {
            None
        }

        fn write(&mut self, _: &str) -> anyhow::Result<u32> {
            let _ = self.entered.send(());
            // Blocks until the probe thread pumps, like a foreign window that
            // is not answering messages.
            unsafe { SendMessageW(HWND(self.window as *mut _), WM_PROBE, None, None) };
            Ok(1)
        }
    }

    unsafe extern "system" fn probe_window(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_PROBE {
            // Ends the probe thread's pump after this sent message is handled.
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    /// A message-only window (`HWND_MESSAGE`): never visible, shown, focused or
    /// listed as a top-level window.
    fn create_message_window() -> windows::core::Result<HWND> {
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None)?.0);
            let class = w!("BubblePlayClipboardWorkerProbe");
            let definition = WNDCLASSW {
                lpfnWndProc: Some(probe_window),
                hInstance: instance,
                lpszClassName: class,
                ..Default::default()
            };
            // Zero here (already registered by an earlier fixture) is harmless;
            // a real failure surfaces at CreateWindowExW.
            let _ = RegisterClassW(&definition);
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                w!(""),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance),
                None,
            )
        }
    }

    /// Dispatch every queued message (including sent ones) until `stop` is set,
    /// a WM_QUIT arrives, or `deadline` passes.
    fn pump_until(stop: &AtomicBool, deadline: Instant) {
        loop {
            let mut message = MSG::default();
            unsafe {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    if message.message == WM_QUIT {
                        return;
                    }
                    DispatchMessageW(&message);
                }
            }
            if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// A message-only window owned by a fixture thread. The thread withholds
    /// dispatch for `hold`, then pumps until a probe ends it, `pump_budget`
    /// expires, or the fixture is dropped. Drop always joins, so the thread is
    /// finished on every path, including a failing assertion.
    struct ProbeWindow {
        hwnd: isize,
        stop: Arc<AtomicBool>,
        hold_release: Option<Sender<()>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl ProbeWindow {
        fn spawn(hold: Duration, pump_budget: Duration) -> Self {
            let (hold_release, hold_rx) = mpsc::channel::<()>();
            let (ready_tx, ready_rx) = mpsc::channel::<isize>();
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let thread = thread::Builder::new()
                .name("clipboard-probe-window".into())
                .spawn(move || {
                    let Ok(window) = create_message_window() else {
                        return;
                    };
                    let _ = ready_tx.send(window.0 as isize);
                    // Dropping `hold_release` ends the hold early.
                    let _ = hold_rx.recv_timeout(hold);
                    pump_until(&thread_stop, Instant::now() + pump_budget);
                    unsafe {
                        let _ = DestroyWindow(window);
                    }
                })
                .expect("spawn probe window thread");
            // Build the guard first so a failed wait below still joins the thread.
            let mut probe = Self {
                hwnd: 0,
                stop,
                hold_release: Some(hold_release),
                thread: Some(thread),
            };
            probe.hwnd = ready_rx.recv_timeout(WAIT).expect("probe window ready");
            probe
        }
    }

    impl Drop for ProbeWindow {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            // Ends the hold if it is still running.
            self.hold_release.take();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn wait_for_completion(worker: &ClipboardWorker) -> Option<Completion> {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if let Some(completion) = worker.try_recv() {
                return Some(completion);
            }
            thread::sleep(Duration::from_millis(5));
        }
        None
    }

    #[test]
    fn foreign_window_block_does_not_hold_the_caller() {
        let probe = ProbeWindow::spawn(Duration::from_millis(1000), WAIT);
        let (entered, entered_rx) = mpsc::channel();
        let (exited_tx, exited_rx) = mpsc::channel::<()>();
        let worker = ClipboardWorker::spawn_with(WindowWrite {
            window: probe.hwnd,
            entered,
            exited: exited_tx,
        })
        .expect("spawn");
        worker.set_epoch(1);

        let started = Instant::now();
        assert!(worker.submit(Command::Write {
            epoch: 1,
            id: 7,
            text: String::from("probe"),
            deadline: Instant::now() + WAIT,
        }));
        assert!(
            started.elapsed() < INPUT_BUDGET,
            "submit must not wait for the window"
        );
        entered_rx
            .recv_timeout(WAIT)
            .expect("backend entered the foreign call");
        // The probe thread withholds dispatch for a full second, so the call
        // is still blocked here and no completion can exist yet.
        assert!(worker.try_recv().is_none(), "the call is still blocked");

        let completion =
            wait_for_completion(&worker).expect("write completes after the window pumps");
        assert!(matches!(
            completion,
            Completion::Written {
                epoch: 1,
                id: 7,
                success: true
            }
        ));

        drop(worker);
        assert!(matches!(
            exited_rx.recv_timeout(WAIT),
            Err(RecvTimeoutError::Disconnected)
        ));
        drop(probe);
    }

    #[test]
    fn inline_send_to_an_unpumped_window_exceeds_the_input_budget() {
        // Negative control: the same SendMessageW without the worker blocks the
        // caller for the whole hold.
        let probe = ProbeWindow::spawn(Duration::from_millis(500), WAIT);
        let started = Instant::now();
        unsafe { SendMessageW(HWND(probe.hwnd as *mut _), WM_PROBE, None, None) };
        assert!(
            started.elapsed() > INPUT_BUDGET,
            "an inline send must exceed the budget (took {:?})",
            started.elapsed()
        );
        drop(probe);
    }

    #[test]
    fn saturated_queue_refuses_and_drop_does_not_join_a_blocked_call() {
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (writes, _written) = mpsc::channel();
        let (exited_tx, exited_rx) = mpsc::channel::<()>();
        let worker = ClipboardWorker::spawn_with(ScriptedBackend {
            entered,
            release: release_rx,
            writes,
            exited: exited_tx,
        })
        .expect("spawn");
        worker.set_epoch(1);
        let poll = || Command::Poll {
            epoch: 1,
            reset: false,
        };

        assert!(worker.submit(poll()));
        entered_rx
            .recv_timeout(WAIT)
            .expect("worker entered its native call");
        for _ in 0..COMMAND_CAPACITY {
            assert!(worker.submit(poll()));
        }
        let started = Instant::now();
        assert!(!worker.submit(poll()), "a full queue refuses the command");
        assert!(started.elapsed() < INPUT_BUDGET);

        // One release per poll; each completion is drained before the next,
        // so the completion queue never fills in this test.
        for _ in 0..=COMMAND_CAPACITY {
            release.send(()).unwrap();
            assert!(matches!(
                wait_for_completion(&worker),
                Some(Completion::Polled {
                    epoch: 1,
                    content: None
                })
            ));
        }

        // Every earlier poll has completed, so its entry marks are stale.
        while entered_rx.try_recv().is_ok() {}
        assert!(worker.submit(poll()));
        entered_rx
            .recv_timeout(WAIT)
            .expect("worker entered its native call again");
        let dropped = Instant::now();
        drop(worker);
        assert!(dropped.elapsed() < INPUT_BUDGET, "drop never joins");
        assert!(
            matches!(exited_rx.try_recv(), Err(TryRecvError::Empty)),
            "the worker is still inside its native call"
        );
        release.send(()).unwrap();
        assert!(matches!(
            exited_rx.recv_timeout(WAIT),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn queued_write_from_an_old_epoch_never_reaches_the_clipboard() {
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (writes, written_rx) = mpsc::channel();
        let (exited_tx, exited_rx) = mpsc::channel::<()>();
        let worker = ClipboardWorker::spawn_with(ScriptedBackend {
            entered,
            release: release_rx,
            writes,
            exited: exited_tx,
        })
        .expect("spawn");
        worker.set_epoch(1);

        assert!(worker.submit(Command::Poll {
            epoch: 1,
            reset: false,
        }));
        entered_rx
            .recv_timeout(WAIT)
            .expect("worker entered its native call");
        // Queued behind the held poll, while the worker is still in epoch 1.
        assert!(worker.submit(Command::Write {
            epoch: 1,
            id: 1,
            text: String::from("old"),
            deadline: Instant::now() + WAIT,
        }));
        worker.set_epoch(2);
        release.send(()).unwrap();

        // Commands run in order, so the first completion after this write is
        // the proof that the held poll's and the old write's completions were
        // suppressed.
        assert!(worker.submit(Command::Write {
            epoch: 2,
            id: 2,
            text: String::from("new"),
            deadline: Instant::now() + WAIT,
        }));
        assert!(matches!(
            wait_for_completion(&worker),
            Some(Completion::Written {
                epoch: 2,
                id: 2,
                success: true
            })
        ));
        assert_eq!(written_rx.try_recv(), Ok(String::from("new")));
        assert!(matches!(written_rx.try_recv(), Err(TryRecvError::Empty)));

        drop(worker);
        assert!(matches!(
            exited_rx.recv_timeout(WAIT),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn expired_queued_write_does_not_replace_the_clipboard() {
        let (entered, _entered_rx) = mpsc::channel();
        let (_release, release_rx) = mpsc::channel();
        let (writes, written_rx) = mpsc::channel();
        let (exited, exited_rx) = mpsc::channel::<()>();
        let worker = ClipboardWorker::spawn_with(ScriptedBackend {
            entered,
            release: release_rx,
            writes,
            exited,
        })
        .expect("spawn");
        worker.set_epoch(1);
        for (id, text, deadline) in [
            (1, "expired", Instant::now() - Duration::from_secs(1)),
            (2, "fresh", Instant::now() + WAIT),
        ] {
            assert!(worker.submit(Command::Write {
                epoch: 1,
                id,
                text: text.into(),
                deadline,
            }));
        }
        assert!(matches!(
            wait_for_completion(&worker),
            Some(Completion::Written {
                id: 1,
                success: false,
                ..
            })
        ));
        assert!(matches!(
            wait_for_completion(&worker),
            Some(Completion::Written {
                id: 2,
                success: true,
                ..
            })
        ));
        assert_eq!(written_rx.try_recv(), Ok("fresh".into()));
        assert!(matches!(written_rx.try_recv(), Err(TryRecvError::Empty)));
        drop(worker);
        assert!(matches!(
            exited_rx.recv_timeout(WAIT),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn live_slots_cap_and_release_on_drop() {
        static SLOTS: AtomicUsize = AtomicUsize::new(0);
        let first = LiveSlot::acquire(&SLOTS, 2).expect("first slot");
        let second = LiveSlot::acquire(&SLOTS, 2).expect("second slot");
        assert!(LiveSlot::acquire(&SLOTS, 2).is_none());
        drop(first);
        assert!(LiveSlot::acquire(&SLOTS, 2).is_some());
        drop(second);
    }
}
