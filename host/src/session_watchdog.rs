//! Independent observation of the session owner, including a thread stuck in
//! native I/O or a mutex. Publishing progress never takes another thread's lock.
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const STALL_AFTER: Duration = Duration::from_secs(10);
const CHECK_EVERY: Duration = Duration::from_secs(1);
const TICK_MASK: u64 = (1 << 40) - 1;

#[derive(Clone, Copy)]
#[repr(u8)]
pub(super) enum Stage {
    Startup = 1,
    CaptureDemand,
    ActivityWait,
    Housekeeping,
    SecureHelper,
    TransportEvents,
    Input,
    Foreground,
    Files,
    Signaling,
    Media,
    Capture,
    Encode,
    Publish,
    Cleanup,
    HousekeepingLease,
    HousekeepingStatus,
}

fn stage_name(id: u8) -> &'static str {
    match id {
        1 => "startup",
        2 => "capture-demand",
        3 => "activity-wait",
        4 => "housekeeping",
        5 => "secure-helper",
        6 => "transport-events",
        7 => "input",
        8 => "foreground",
        9 => "files",
        10 => "signaling",
        11 => "media",
        12 => "capture",
        13 => "encode",
        14 => "publish",
        15 => "cleanup",
        16 => "housekeeping-lease",
        17 => "housekeeping-status",
        _ => "unknown",
    }
}

struct Progress {
    started: Instant,
    // One coherent snapshot: stage (8 bits), main.rs line (16), uptime ms (40).
    value: AtomicU64,
}

impl Progress {
    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(TICK_MASK as u128) as u64
    }

    fn mark_at(&self, stage: Stage, line: u32, elapsed_ms: u64) {
        let value = ((stage as u64) << 56)
            | (u64::from(line.min(u16::MAX as u32)) << 40)
            | elapsed_ms.min(TICK_MASK);
        self.value.store(value, Ordering::Release);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Stall {
    stage: &'static str,
    line: u16,
    elapsed_ms: u64,
}

#[derive(Default)]
struct Observer {
    reported: Option<u64>,
}

impl Observer {
    fn inspect(&mut self, value: u64, now_ms: u64, threshold_ms: u64) -> Option<Stall> {
        if self.reported.is_some_and(|previous| previous != value) {
            self.reported = None;
        }
        let age = now_ms.saturating_sub(value & TICK_MASK);
        if value == 0 || age < threshold_ms || self.reported == Some(value) {
            return None;
        }
        self.reported = Some(value);
        Some(Stall {
            stage: stage_name((value >> 56) as u8),
            line: (value >> 40) as u16,
            elapsed_ms: age,
        })
    }
}

pub(super) struct SessionWatchdog {
    progress: Arc<Progress>,
    stop: mpsc::SyncSender<()>,
    _native_calls: Option<bubbleplay_host::encode::NativeCallObserver>,
}

pub(super) struct StageGuard<'a> {
    progress: &'a Progress,
    previous: u64,
}

impl Drop for StageGuard<'_> {
    fn drop(&mut self) {
        self.progress.value.store(
            if self.previous == 0 {
                0
            } else {
                (self.previous & !TICK_MASK) | self.progress.elapsed_ms()
            },
            Ordering::Release,
        );
    }
}

impl SessionWatchdog {
    pub(super) fn start() -> io::Result<Self> {
        let progress = Arc::new(Progress {
            started: Instant::now(),
            value: AtomicU64::new(0),
        });
        let worker = progress.clone();
        let (stop, stopped) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("session-watchdog".into())
            .spawn(move || {
                let mut observer = Observer::default();
                while matches!(
                    stopped.recv_timeout(CHECK_EVERY),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    if let Some(stall) = observer.inspect(
                        worker.value.load(Ordering::Acquire),
                        worker.elapsed_ms(),
                        STALL_AFTER.as_millis() as u64,
                    ) {
                        let native = bubbleplay_host::encode::active_native_call();
                        tracing::error!(
                            stage = stall.stage,
                            source = "main.rs",
                            line = stall.line,
                            elapsed_ms = stall.elapsed_ms,
                            native_call = native.map(|(name, _)| name),
                            native_source = native.map(|_| "encode.rs"),
                            native_line = native.map(|(_, line)| line),
                            "host session stalled"
                        );
                    }
                }
            })?;
        Ok(Self {
            progress,
            stop,
            _native_calls: bubbleplay_host::encode::NativeCallObserver::attach(),
        })
    }

    pub(super) fn mark(&self, stage: Stage, line: u32) {
        self.progress
            .mark_at(stage, line, self.progress.elapsed_ms());
    }

    pub(super) fn enter(&self, stage: Stage, line: u32) -> StageGuard<'_> {
        let previous = self.progress.value.load(Ordering::Acquire);
        self.mark(stage, line);
        StageGuard {
            progress: &self.progress,
            previous,
        }
    }
}

impl Drop for SessionWatchdog {
    fn drop(&mut self) {
        // A stuck session cannot drop this owner. A healthy exit only signals
        // the observer; it never waits for a logging thread's shutdown.
        let _ = self.stop.try_send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_input_is_reported_once_without_needing_the_session_thread() {
        let progress = Progress {
            started: Instant::now(),
            value: AtomicU64::new(0),
        };
        let mut observer = Observer::default();
        assert_eq!(observer.inspect(0, 30_000, 10_000), None);
        progress.mark_at(Stage::Input, 2858, 100);
        let value = progress.value.load(Ordering::Acquire);
        assert_eq!(observer.inspect(value, 10_099, 10_000), None);
        assert_eq!(
            observer.inspect(value, 10_100, 10_000),
            Some(Stall {
                stage: "input",
                line: 2858,
                elapsed_ms: 10_000,
            })
        );
        assert_eq!(observer.inspect(value, 60_100, 10_000), None);
        progress.mark_at(Stage::Capture, 3941, 60_100);
        let resumed = progress.value.load(Ordering::Acquire);
        assert_eq!(observer.inspect(resumed, 60_100, 10_000), None);
        assert_eq!(
            observer.inspect(resumed, 70_100, 10_000),
            Some(Stall {
                stage: "capture",
                line: 3941,
                elapsed_ms: 10_000,
            })
        );
    }

    #[test]
    fn idle_housekeeping_and_repeated_same_stage_progress_do_not_report_a_stall() {
        let progress = Progress {
            started: Instant::now(),
            value: AtomicU64::new(0),
        };
        let mut observer = Observer::default();
        for now in (0..60_000).step_by(100) {
            progress.mark_at(Stage::ActivityWait, 1409, now);
            assert_eq!(
                observer.inspect(progress.value.load(Ordering::Acquire), now + 100, 10_000),
                None
            );
        }
    }

    #[test]
    fn completed_nested_call_restores_the_enclosing_stage() {
        let watchdog = SessionWatchdog::start().unwrap();
        watchdog.mark(Stage::Signaling, 2502);
        {
            let _call = watchdog.enter(Stage::Input, 2858);
            let value = watchdog.progress.value.load(Ordering::Acquire);
            assert_eq!(stage_name((value >> 56) as u8), "input");
            assert_eq!((value >> 40) as u16, 2858);
        }
        let value = watchdog.progress.value.load(Ordering::Acquire);
        assert_eq!(stage_name((value >> 56) as u8), "signaling");
        assert_eq!((value >> 40) as u16, 2502);
    }
}
