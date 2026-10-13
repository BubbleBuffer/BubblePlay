//! Host-owned current viewer demand, sampled by the tray independently of logs.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, OpenEventW, ResetEvent, SetEvent, WaitForSingleObject,
    SYNCHRONIZATION_SYNCHRONIZE,
};

use super::protocol::scoped_name;

const DEMAND_EVENT: &str = r"Local\BubblePlay.AdminAgent.Demand";
const STATUS_FILE: &str = "admin-agent-status.json";
const MAX_REASON_CHARS: usize = 4096;
const FAILURE_MAX_AGE_MS: u128 = 15_000;
const START_RETRY_INTERVAL: Duration = Duration::from_secs(5);

fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(Some(0)).collect()
}

fn name_from_env() -> Result<String> {
    let mut session = 0;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }
        .context("find current demand session")?;
    let scope = std::env::var_os("BUBBLEPLAY_TRAY_DATA_DIR");
    Ok(scoped_name(
        &format!("{DEMAND_EVENT}.session-{session}"),
        scope.as_deref(),
    ))
}

/// The host keeps this handle alive for its lifetime. Queries only borrow the
/// kernel event briefly, so a vanished host cannot leave persistent demand.
pub struct DemandSignal {
    handle: HANDLE,
    observed: Mutex<Option<bool>>,
}

// SAFETY: events support concurrent SetEvent/ResetEvent and the handle remains
// owned until all shared references to this signal are gone.
unsafe impl Send for DemandSignal {}
unsafe impl Sync for DemandSignal {}

impl DemandSignal {
    pub fn from_env() -> Result<Self> {
        Self::named(&name_from_env()?)
    }

    fn named(name: &str) -> Result<Self> {
        let name = wide(name);
        let handle = unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }
            .context("create host demand event")?;
        Ok(Self {
            handle,
            observed: Mutex::new(None),
        })
    }

    /// Publish current need rather than only demand edges.
    pub fn observe(&self, needed: bool) -> Result<()> {
        let mut observed = self
            .observed
            .lock()
            .map_err(|_| anyhow::anyhow!("host demand observation lock poisoned"))?;
        if *observed == Some(needed) {
            return Ok(());
        }
        if needed {
            unsafe { SetEvent(self.handle) }.context("set host demand")?;
        } else {
            unsafe { ResetEvent(self.handle) }.context("reset host demand")?;
        }
        // Failed Win32 calls leave the cached observation untouched, so the
        // next host iteration retries publishing it.
        *observed = Some(needed);
        Ok(())
    }
}

impl Drop for DemandSignal {
    fn drop(&mut self) {
        unsafe {
            let _ = ResetEvent(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

fn active_named(name: &str) -> bool {
    let name = wide(name);
    let Ok(handle) =
        (unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) })
    else {
        return false;
    };
    let active = unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0;
    unsafe {
        let _ = CloseHandle(handle);
    }
    active
}

/// A tray query never creates the event or keeps it alive between polls.
pub fn active_from_env() -> bool {
    name_from_env().is_ok_and(|name| active_named(&name))
}

/// Reserves a start attempt when permitted. Failed launches and demand toggles
/// share the same cooldown; a newly started tray can react immediately.
#[derive(Debug, Default)]
pub struct StartRetry {
    last_attempt: Option<Instant>,
}

impl StartRetry {
    /// A slow scheduler call must still leave a full cooldown after returning.
    pub fn attempt_finished(&mut self, now: Instant) {
        self.last_attempt = Some(now);
    }

    pub fn should_start(&mut self, now: Instant, needed: bool, agent_running: bool) -> bool {
        if !needed || agent_running {
            return false;
        }
        if self
            .last_attempt
            .is_some_and(|last| now.saturating_duration_since(last) < START_RETRY_INTERVAL)
        {
            return false;
        }
        self.last_attempt = Some(now);
        true
    }
}

#[derive(Serialize, Deserialize)]
struct FailureStatus {
    timestamp_unix_ms: u128,
    reason: String,
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn data_from_env() -> Option<PathBuf> {
    std::env::var_os("BUBBLEPLAY_TRAY_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA").map(|path| PathBuf::from(path).join("BubblePlay"))
        })
}

/// Store a short-lived diagnostic or clear it after a successful hello.
pub fn publish_failure_in(data: &Path, reason: Option<&str>) -> Result<()> {
    let path = data.join(STATUS_FILE);
    let Some(reason) = reason else {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("clear admin agent status"),
        };
    };
    std::fs::create_dir_all(data).context("create admin agent status directory")?;
    let status = FailureStatus {
        timestamp_unix_ms: unix_millis(),
        reason: reason.chars().take(MAX_REASON_CHARS).collect(),
    };
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, serde_json::to_vec(&status)?).context("write admin agent status")?;
    std::fs::rename(temporary, path).context("publish admin agent status")
}

fn failure_in(data: &Path, now_unix_ms: u128) -> Option<String> {
    let status: FailureStatus =
        serde_json::from_slice(&std::fs::read(data.join(STATUS_FILE)).ok()?).ok()?;
    let age = now_unix_ms.checked_sub(status.timestamp_unix_ms)?;
    (age <= FAILURE_MAX_AGE_MS).then(|| status.reason.chars().take(MAX_REASON_CHARS).collect())
}

/// Ignore absent, malformed, future-dated and expired diagnostics.
pub fn failure_from_env() -> Option<String> {
    failure_in(&data_from_env()?, unix_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_scope() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "bubbleplay-demand-tests-{}-{}-{}",
            std::process::id(),
            unix_millis(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn needed_missing_agent_starts_immediately() {
        assert!(StartRetry::default().should_start(Instant::now(), true, false));
    }

    #[test]
    fn running_agent_does_not_start_or_reserve_attempt() {
        let mut retry = StartRetry::default();
        let now = Instant::now();
        assert!(!retry.should_start(now, true, true));
        assert!(retry.should_start(now, true, false));
    }

    #[test]
    fn failed_start_retries_only_after_five_seconds() {
        let mut retry = StartRetry::default();
        let now = Instant::now();
        assert!(retry.should_start(now, true, false));
        assert!(!retry.should_start(now + Duration::from_millis(4999), true, false));
        assert!(retry.should_start(now + Duration::from_secs(5), true, false));
        assert!(!retry.should_start(now + Duration::from_secs(5), true, false));
    }

    #[test]
    fn slow_task_call_leaves_a_full_cooldown_after_completion() {
        let mut retry = StartRetry::default();
        let now = Instant::now();
        assert!(retry.should_start(now, true, false));
        retry.attempt_finished(now + Duration::from_secs(10));
        assert!(!retry.should_start(now + Duration::from_secs(14), true, false));
        assert!(retry.should_start(now + Duration::from_secs(15), true, false));
    }

    #[test]
    fn demand_gone_stops_attempts_without_resetting_cooldown() {
        let mut retry = StartRetry::default();
        let now = Instant::now();
        assert!(retry.should_start(now, true, false));
        assert!(!retry.should_start(now + Duration::from_secs(1), false, false));
        assert!(!retry.should_start(now + Duration::from_secs(2), true, false));
        assert!(!retry.should_start(now + Duration::from_secs(5), false, false));
        assert!(retry.should_start(now + Duration::from_secs(5), true, false));
    }

    #[test]
    fn tray_restart_reads_surviving_host_demand_and_retries_immediately() {
        let scope = unique_scope();
        let name = scoped_name(
            &format!("{DEMAND_EVENT}.test-session"),
            Some(scope.as_os_str()),
        );
        assert!(!active_named(&name));
        let host = DemandSignal::named(&name).unwrap();
        assert!(!active_named(&name));
        host.observe(true).unwrap();
        let now = Instant::now();
        let mut tray = StartRetry::default();
        assert!(tray.should_start(now, active_named(&name), false));
        assert!(!tray.should_start(now, active_named(&name), false));
        // Each query has closed its handle. Recreating the tray still sees the
        // host's level, and an entirely new cooldown starts immediately.
        let mut restarted_tray = StartRetry::default();
        assert!(restarted_tray.should_start(now, active_named(&name), false));
        host.observe(false).unwrap();
        assert!(!active_named(&name));
        host.observe(true).unwrap();
        assert!(active_named(&name));
        drop(host);
        assert!(!active_named(&name));
        // Verify the event disappeared, rather than merely being unsignaled.
        let name = wide(&name);
        assert!(
            unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) }
                .is_err()
        );
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            Self(unique_scope())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn failure_publishes_and_clears_without_environment_changes() {
        let scratch = Scratch::new();
        assert!(failure_in(&scratch.0, unix_millis()).is_none());
        publish_failure_in(&scratch.0, Some("agent pipe unavailable")).unwrap();
        assert_eq!(
            failure_in(&scratch.0, unix_millis()).as_deref(),
            Some("agent pipe unavailable")
        );
        publish_failure_in(&scratch.0, Some("task result 2; hello timeout")).unwrap();
        assert_eq!(
            failure_in(&scratch.0, unix_millis()).as_deref(),
            Some("task result 2; hello timeout")
        );
        publish_failure_in(&scratch.0, None).unwrap();
        assert!(failure_in(&scratch.0, unix_millis()).is_none());
        publish_failure_in(&scratch.0, None).unwrap();
    }

    #[test]
    fn failure_expires_at_fifteen_seconds_and_rejects_future_or_malformed() {
        let scratch = Scratch::new();
        publish_failure_in(&scratch.0, Some("failed")).unwrap();
        let path = scratch.0.join(STATUS_FILE);
        let status: FailureStatus = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            failure_in(&scratch.0, status.timestamp_unix_ms + 15_000).as_deref(),
            Some("failed")
        );
        assert!(failure_in(&scratch.0, status.timestamp_unix_ms + 15_001).is_none());
        assert!(failure_in(&scratch.0, status.timestamp_unix_ms - 1).is_none());
        std::fs::write(path, b"not json").unwrap();
        assert!(failure_in(&scratch.0, unix_millis()).is_none());
    }

    #[test]
    fn failure_reason_is_bounded_in_characters_on_write_and_read() {
        let scratch = Scratch::new();
        let reason = "\u{1f43e}".repeat(MAX_REASON_CHARS + 1);
        publish_failure_in(&scratch.0, Some(&reason)).unwrap();
        let path = scratch.0.join(STATUS_FILE);
        let mut status: FailureStatus =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(status.reason.chars().count(), MAX_REASON_CHARS);
        status.reason = reason;
        std::fs::write(path, serde_json::to_vec(&status).unwrap()).unwrap();
        assert_eq!(
            failure_in(&scratch.0, unix_millis())
                .unwrap()
                .chars()
                .count(),
            MAX_REASON_CHARS
        );
    }
}
