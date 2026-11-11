//! Names a frozen foreground app while a viewer is attached.
//!
//! When the foreground window stops responding (`IsHungAppWindow`, or DWM's `Ghost` stand-in is
//! foreground) or the cursor stays `Wait` for [`WAIT_LIMIT`], the host logs one warn line per
//! episode with what it was doing itself: whether a native file drop holds an attached input
//! queue, how long each of its own hook threads has been away from its message wait, and the
//! last native file drop and clipboard steps. Sampling reuses the foreground tick; nothing here
//! runs per frame.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a `Wait` cursor must persist before it counts as a stall.
pub const WAIT_LIMIT: Duration = Duration::from_secs(5);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Milliseconds since the process's diagnostic epoch; never 0, so 0 can mean "unset".
fn stamp(now: Instant) -> u64 {
    now.saturating_duration_since(epoch()).as_millis() as u64 + 1
}

fn age_ms(stamp_ms: u64, now: Instant) -> Option<u64> {
    (stamp_ms != 0).then(|| self::stamp(now).saturating_sub(stamp_ms))
}

/// Liveness of a thread that owns a low-level hook. The owner marks itself busy when it leaves
/// its message wait (or enters a hook callback) and idle when it returns, so a stalled hook
/// thread shows as a growing busy time while a parked one shows 0.
pub struct PumpClock {
    busy_since: AtomicU64,
    last_event: AtomicU64,
}

impl PumpClock {
    pub const fn new() -> Self {
        Self {
            busy_since: AtomicU64::new(0),
            last_event: AtomicU64::new(0),
        }
    }

    /// The thread left its message wait or entered a hook callback.
    pub fn busy(&self) {
        let now = stamp(Instant::now());
        let _ = self
            .busy_since
            .compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire);
        self.last_event.store(now, Ordering::Release);
    }

    /// The thread is back in its message wait.
    pub fn idle(&self) {
        self.busy_since.store(0, Ordering::Release);
    }

    /// Mark the thread busy now and idle when the returned guard drops, so an early return
    /// from the pump never leaves a stale busy mark.
    pub fn running(&self) -> PumpRun<'_> {
        self.busy();
        PumpRun(self)
    }

    /// How long the thread has been out of its message wait; 0 while it waits.
    pub fn busy_ms(&self, now: Instant) -> u64 {
        age_ms(self.busy_since.load(Ordering::Acquire), now).unwrap_or(0)
    }

    /// Time since the thread last woke for a message or hook event; `None` if it never did.
    pub fn last_event_ms(&self, now: Instant) -> Option<u64> {
        age_ms(self.last_event.load(Ordering::Acquire), now)
    }
}

/// Marks its [`PumpClock`] idle when dropped.
pub struct PumpRun<'a>(&'a PumpClock);

impl Drop for PumpRun<'_> {
    fn drop(&mut self) {
        self.0.idle();
    }
}

impl Default for PumpClock {
    fn default() -> Self {
        Self::new()
    }
}

/// The cursor-wake thread's `WH_MOUSE_LL` hook (the binary's `producer_wait`).
pub static CURSOR_HOOK: PumpClock = PumpClock::new();
/// The prepared native drop input monitor's `WH_MOUSE_LL`/`WH_KEYBOARD_LL` hooks.
pub static DROP_HOOK: PumpClock = PumpClock::new();

/// Which subsystem a recorded step belongs to.
#[derive(Clone, Copy)]
pub enum StepArea {
    FileDrop = 0,
    Clipboard = 1,
}

/// A recorded step: when it started, its name, and whether it has finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub at: Instant,
    pub name: &'static str,
    pub done: bool,
}

static STEPS: Mutex<[Option<Step>; 2]> = Mutex::new([None, None]);

/// Record that `area` started `name`. Content-free: names are fixed strings.
pub fn step(area: StepArea, name: &'static str) {
    if let Ok(mut steps) = STEPS.lock() {
        steps[area as usize] = Some(Step {
            at: Instant::now(),
            name,
            done: false,
        });
    }
}

/// Record that `area`'s current step finished.
pub fn step_done(area: StepArea) {
    if let Ok(mut steps) = STEPS.lock() {
        if let Some(step) = steps[area as usize].as_mut() {
            step.done = true;
        }
    }
}

/// Marks `area`'s current step finished when dropped. A step sequence holds one for its whole
/// run, so every exit (early return, error, normal end) ends the last step it recorded and a
/// finished native drop never reads as "running" long after it returned.
pub struct StepsEnd(StepArea);

impl Drop for StepsEnd {
    fn drop(&mut self) {
        step_done(self.0);
    }
}

pub fn steps_end_on_exit(area: StepArea) -> StepsEnd {
    StepsEnd(area)
}

pub fn last_step(area: StepArea) -> Option<Step> {
    STEPS.lock().ok().and_then(|steps| steps[area as usize])
}

/// One foreground observation on the foreground tick.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForegroundObservation {
    pub window: isize,
    pub pid: u32,
    pub class: String,
    /// `IsHungAppWindow` for the foreground window.
    pub hung: bool,
    /// The host's last classified cursor was `Wait`.
    pub cursor_wait: bool,
}

impl ForegroundObservation {
    pub fn ghost(&self) -> bool {
        self.class == crate::follow_window::GHOST_CLASS
    }
}

/// Why an episode was reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StallReason {
    Hung,
    Ghost,
    WaitCursor,
}

impl StallReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hung => "hung",
            Self::Ghost => "ghost",
            Self::WaitCursor => "wait-cursor",
        }
    }
}

/// A stall episode to report once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StallEpisode {
    pub reason: StallReason,
    pub foreground: ForegroundObservation,
    /// The last foreground that was not a ghost, when the foreground is one: the app it stands in for.
    pub behind_ghost: Option<(u32, String)>,
    pub wait_ms: u64,
}

/// Turns foreground observations into at most one report per stall episode. A new episode
/// starts only after an observation with neither a hung window, a ghost nor a `Wait` cursor.
#[derive(Default)]
pub struct StallWatch {
    wait_since: Option<Instant>,
    reported: bool,
    last_app: Option<(u32, String)>,
}

impl StallWatch {
    /// Forget the episode state, for example when the viewer leaves.
    pub fn reset(&mut self) {
        self.wait_since = None;
        self.reported = false;
    }

    pub fn observe(&mut self, now: Instant, sample: ForegroundObservation) -> Option<StallEpisode> {
        let ghost = sample.ghost();
        if !ghost && sample.pid != 0 {
            self.last_app = Some((sample.pid, sample.class.clone()));
        }
        if sample.cursor_wait {
            self.wait_since.get_or_insert(now);
        } else {
            self.wait_since = None;
        }
        let wait = self
            .wait_since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        let reason = if sample.hung {
            Some(StallReason::Hung)
        } else if ghost {
            Some(StallReason::Ghost)
        } else if wait >= WAIT_LIMIT {
            Some(StallReason::WaitCursor)
        } else {
            None
        };
        let Some(reason) = reason else {
            if !sample.cursor_wait {
                self.reported = false;
            }
            return None;
        };
        if std::mem::replace(&mut self.reported, true) {
            return None;
        }
        Some(StallEpisode {
            reason,
            behind_ghost: if ghost { self.last_app.clone() } else { None },
            foreground: sample,
            wait_ms: wait.as_millis() as u64,
        })
    }
}

/// Read the foreground window (Win32; interactive session only).
pub fn observe_foreground(cursor_wait: bool) -> ForegroundObservation {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId, IsHungAppWindow,
    };
    // SAFETY: plain Win32 queries with valid out-pointers; none sends a message to the window.
    unsafe {
        let window = GetForegroundWindow();
        if window.is_invalid() {
            return ForegroundObservation {
                cursor_wait,
                ..Default::default()
            };
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, Some(&raw mut pid));
        let mut class = [0u16; 64];
        let len = GetClassNameW(window, &mut class).max(0) as usize;
        ForegroundObservation {
            window: window.0 as isize,
            pid,
            class: String::from_utf16_lossy(&class[..len]),
            hung: IsHungAppWindow(window).as_bool(),
            cursor_wait,
        }
    }
}

/// Log `episode` with the host's own input state. One warn line.
pub fn report(episode: &StallEpisode) {
    let now = Instant::now();
    let exe = crate::follow_window::exe_file_name(episode.foreground.pid);
    let (behind_pid, behind_class) = episode.behind_ghost.clone().unzip();
    let behind_exe = behind_pid.map(crate::follow_window::exe_file_name);
    let describe = |area| {
        last_step(area).map(|step: Step| {
            format!(
                "{} {} {} ms ago",
                step.name,
                if step.done { "done" } else { "running" },
                now.saturating_duration_since(step.at).as_millis()
            )
        })
    };
    tracing::warn!(
        reason = episode.reason.as_str(),
        foreground_exe = %exe,
        foreground_class = %episode.foreground.class,
        foreground_pid = episode.foreground.pid,
        behind_ghost_exe = behind_exe.as_deref(),
        behind_ghost_class = behind_class.as_deref(),
        behind_ghost_pid = behind_pid,
        wait_ms = episode.wait_ms,
        native_queue_attached = crate::native_file_drop::input_queue_attached(),
        cursor_hook_busy_ms = CURSOR_HOOK.busy_ms(now),
        cursor_hook_last_event_ms = CURSOR_HOOK.last_event_ms(now),
        drop_hook_busy_ms = DROP_HOOK.busy_ms(now),
        last_file_drop_step = describe(StepArea::FileDrop).as_deref(),
        last_clipboard_step = describe(StepArea::Clipboard).as_deref(),
        "foreground app stopped responding while a viewer is attached"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(pid: u32, class: &str) -> ForegroundObservation {
        ForegroundObservation {
            window: pid as isize,
            pid,
            class: class.into(),
            ..Default::default()
        }
    }

    #[test]
    fn hung_foreground_reports_once_per_episode() {
        let mut watch = StallWatch::default();
        let t = Instant::now();
        assert_eq!(watch.observe(t, app(10, "Chrome_WidgetWin_1")), None);
        let hung = ForegroundObservation {
            hung: true,
            ..app(10, "Chrome_WidgetWin_1")
        };
        let episode = watch
            .observe(t, hung.clone())
            .expect("hung window reported");
        assert_eq!(episode.reason, StallReason::Hung);
        assert_eq!(episode.behind_ghost, None);
        assert_eq!(
            watch.observe(t + Duration::from_secs(1), hung.clone()),
            None
        );
        // Recovery re-arms; the next hang is a new episode.
        assert_eq!(
            watch.observe(t + Duration::from_secs(2), app(10, "Chrome_WidgetWin_1")),
            None
        );
        assert!(watch.observe(t + Duration::from_secs(3), hung).is_some());
    }

    #[test]
    fn ghost_foreground_names_the_app_behind_it() {
        let mut watch = StallWatch::default();
        let t = Instant::now();
        watch.observe(t, app(16544, "WinUIDesktopWin32WindowClass"));
        let episode = watch
            .observe(t + Duration::from_secs(1), app(1552, "Ghost"))
            .expect("ghost reported");
        assert_eq!(episode.reason, StallReason::Ghost);
        assert_eq!(
            episode.behind_ghost,
            Some((16544, "WinUIDesktopWin32WindowClass".into()))
        );
    }

    #[test]
    fn wait_cursor_reports_only_after_the_limit() {
        let mut watch = StallWatch::default();
        let t = Instant::now();
        let waiting = ForegroundObservation {
            cursor_wait: true,
            ..app(7, "Notepad")
        };
        assert_eq!(watch.observe(t, waiting.clone()), None);
        assert_eq!(
            watch.observe(t + Duration::from_millis(4900), waiting.clone()),
            None
        );
        let episode = watch
            .observe(t + WAIT_LIMIT, waiting.clone())
            .expect("long wait reported");
        assert_eq!(episode.reason, StallReason::WaitCursor);
        assert_eq!(episode.wait_ms, WAIT_LIMIT.as_millis() as u64);
        assert_eq!(
            watch.observe(t + Duration::from_secs(9), waiting.clone()),
            None
        );
        // A short busy cursor after recovery restarts the clock and does not report.
        watch.observe(t + Duration::from_secs(10), app(7, "Notepad"));
        assert_eq!(watch.observe(t + Duration::from_secs(11), waiting), None);
    }

    #[test]
    fn pump_clock_reports_busy_time_only_while_out_of_the_wait() {
        let clock = PumpClock::new();
        let now = Instant::now();
        assert_eq!(clock.busy_ms(now), 0);
        assert_eq!(clock.last_event_ms(now), None);
        clock.busy();
        std::thread::sleep(Duration::from_millis(20));
        clock.busy(); // Nested marks keep the first start.
        assert!(clock.busy_ms(Instant::now()) >= 20);
        clock.idle();
        assert_eq!(clock.busy_ms(Instant::now()), 0);
        assert!(clock.last_event_ms(Instant::now()).is_some());
    }

    #[test]
    fn a_step_sequence_that_returned_reads_done() {
        fn sequence() -> Option<()> {
            let _end = steps_end_on_exit(StepArea::FileDrop);
            step(StepArea::FileDrop, "test sequence step");
            let cancelled: Option<()> = None;
            cancelled?; // Early exit, as a cancelled or failed drop takes.
            Some(())
        }
        sequence();
        let last = last_step(StepArea::FileDrop).expect("step recorded");
        // Another test may record a newer step meanwhile; ours must not still read running.
        assert!(last.name != "test sequence step" || last.done, "{last:?}");
    }

    #[test]
    fn a_hook_stuck_mid_callback_reads_busy_from_another_thread() {
        // The stall report samples hook clocks on the session thread while the hook thread
        // may be inside a callback that has not returned. In-progress time must show.
        static CLOCK: PumpClock = PumpClock::new();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let hook = std::thread::spawn(move || {
            let _run = CLOCK.running(); // As the hook callbacks do on entry.
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv(); // Stuck mid-callback.
        });
        entered_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let stuck = CLOCK.busy_ms(Instant::now());
        release_tx.send(()).unwrap();
        hook.join().unwrap();
        assert!(stuck >= 300, "stuck callback reported {stuck} ms busy");
        assert_eq!(CLOCK.busy_ms(Instant::now()), 0);
    }

    #[test]
    fn hung_window_on_a_stalled_test_thread_is_observed() {
        // A real window whose thread stops pumping: IsHungAppWindow turns true after Windows'
        // 5 s timeout. Headless: the window is never shown and receives no input.
        use windows::core::w;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::*;
        let (tx, rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || unsafe {
            let module = GetModuleHandleW(None).unwrap();
            let window = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("bubbleplay stall probe"),
                WS_OVERLAPPED,
                0,
                0,
                10,
                10,
                None,
                None,
                Some(module.into()),
                None,
            )
            .unwrap();
            // One message retrieval ends startup processing; then a message it will not read.
            let mut message = MSG::default();
            let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
            let _ = PostMessageW(
                Some(window),
                WM_USER,
                Default::default(),
                Default::default(),
            );
            tx.send(window.0 as isize).unwrap();
            let _ = release_rx.recv(); // Stalled: no message retrieval.
            let _ = DestroyWindow(window);
        });
        let window = HWND(rx.recv().unwrap() as *mut _);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut hung = false;
        while Instant::now() < deadline && !hung {
            std::thread::sleep(Duration::from_millis(250));
            hung = unsafe { IsHungAppWindow(window) }.as_bool();
        }
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(hung, "a stalled window thread is reported hung");
    }
}
