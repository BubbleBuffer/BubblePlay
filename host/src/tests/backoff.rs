//! Reconnect backoff tests (see `main.rs`).

#[test]
fn serve_backoff_resets_after_a_healthy_run_and_grows_on_quick_failures() {
    use std::time::Duration;
    let max = Duration::from_secs(30);
    assert_eq!(
        crate::next_serve_backoff(max, Some(Duration::from_secs(120))),
        Duration::from_secs(1)
    );
    assert_eq!(
        crate::next_serve_backoff(Duration::from_secs(4), Some(Duration::from_secs(5))),
        Duration::from_secs(8)
    );
    assert_eq!(
        crate::next_serve_backoff(Duration::from_secs(1), None),
        Duration::from_secs(2)
    );
    assert_eq!(crate::next_serve_backoff(max, None), max);
}

#[test]
fn a_healthy_run_that_fails_reconnects_at_once_and_quick_failures_still_wait() {
    use std::time::Duration;
    let (wait, next) = crate::serve_retry(Duration::from_secs(8), Some(Duration::from_secs(120)));
    assert_eq!(wait, Duration::ZERO);
    assert_eq!(next, Duration::from_secs(1));
    let (wait, next) = crate::serve_retry(Duration::from_secs(1), Some(Duration::from_secs(5)));
    assert_eq!(wait, Duration::from_secs(1));
    assert_eq!(next, Duration::from_secs(2));
    let (wait, next) = crate::serve_retry(Duration::from_secs(4), None);
    assert_eq!(
        (wait, next),
        (Duration::from_secs(4), Duration::from_secs(8))
    );
}
