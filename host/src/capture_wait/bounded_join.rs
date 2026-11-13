//! Joins a worker thread with a deadline. A thread stuck in a native call
//! never returns; the caller gets control back and the thread is detached.

use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::warn;

/// How often a pending join checks whether the thread has finished.
const JOIN_POLL_INTERVAL_MS: u64 = 5;

/// Waits up to `timeout` for `thread` to finish. `None` means the deadline
/// passed: the thread is detached and a line names it.
pub(crate) fn join_bounded<T>(
    thread: JoinHandle<T>,
    timeout: Duration,
    name: &'static str,
) -> Option<std::thread::Result<T>> {
    let deadline = Instant::now() + timeout;
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            warn!(
                thread = name,
                timeout_ms = timeout.as_millis() as u64,
                "worker thread did not stop in time; detached"
            );
            return None;
        }
        std::thread::sleep(Duration::from_millis(JOIN_POLL_INTERVAL_MS));
    }
    Some(thread.join())
}

#[cfg(test)]
mod tests {
    use super::join_bounded;
    use std::sync::mpsc;
    use std::time::Duration;

    const SHORT: Duration = Duration::from_millis(50);
    const LONG: Duration = Duration::from_secs(5);

    #[test]
    fn a_blocked_thread_is_detached_at_the_deadline_and_a_finished_one_is_joined() {
        let (release, blocked) = mpsc::channel::<()>();
        let stuck = std::thread::spawn(move || blocked.recv_timeout(LONG).is_ok());
        assert!(join_bounded(stuck, SHORT, "test-stuck").is_none());
        // The detached thread ends once released; it is not joined again.
        release.send(()).unwrap();
        let done = std::thread::spawn(|| 7);
        assert_eq!(join_bounded(done, LONG, "test-done").unwrap().unwrap(), 7);
    }
}
