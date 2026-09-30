//! Host update probation and automatic rollback state machine (pure logic).
//!
//! `update-tray.ps1` records a new host as `pending`. While pending, the tray
//! watches the host: it must log "signaling registered as host" within
//! [`REGISTER_DEADLINE`] and must not crash-loop ([`CRASH_LOOP_EXITS`] exits
//! within [`CRASH_LOOP_WINDOW`]). Failure restores the previous host once;
//! success after [`GOOD_AFTER`] marks the build good. A rolled-back build is
//! never rolled back again.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::logic::write_atomic;

pub const REGISTER_DEADLINE_MS: u64 = 120_000;
pub const CRASH_LOOP_EXITS: usize = 3;
pub const CRASH_LOOP_WINDOW_MS: u64 = 5 * 60_000;
pub const GOOD_AFTER_MS: u64 = 10 * 60_000;
/// Line the host logs once the signaling Worker accepted its registration.
pub const REGISTERED_MARKER: &str = "signaling registered as host";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateStatus {
    Pending,
    Good,
    RolledBack,
    /// Probation failed but no rollback was possible.
    Failed,
}

/// `%LOCALAPPDATA%\BubblePlay\update-state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateState {
    pub status: UpdateStatus,
    /// SHA-256 (hex) of the host installed by the update.
    pub new_hash: String,
    #[serde(default)]
    pub previous_hash: Option<String>,
    #[serde(default)]
    pub updated_unix_ms: u64,
    #[serde(default)]
    pub reason: Option<String>,
}

impl UpdateState {
    pub fn load(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        // Tolerate a UTF-8 BOM written by Windows PowerShell.
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        serde_json::from_slice(bytes).ok()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn with_status(&self, status: UpdateStatus, reason: Option<String>, now_ms: u64) -> Self {
        Self {
            status,
            reason,
            updated_unix_ms: now_ms,
            ..self.clone()
        }
    }

    /// Probation applies only while pending and only to the host it describes.
    pub fn needs_probation(&self, installed_hash: &str) -> bool {
        self.status == UpdateStatus::Pending && self.new_hash.eq_ignore_ascii_case(installed_hash)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Wait,
    Good,
    RollBack(String),
}

/// Probation of a pending host build, driven by host events and a clock.
#[derive(Debug, Clone)]
pub struct Probation {
    started_ms: u64,
    registered_ms: Option<u64>,
    exits: Vec<u64>,
}

impl Probation {
    pub fn new(now_ms: u64) -> Self {
        Self {
            started_ms: now_ms,
            registered_ms: None,
            exits: Vec::new(),
        }
    }

    pub fn on_registered(&mut self, now_ms: u64) {
        self.registered_ms.get_or_insert(now_ms);
    }

    pub fn on_exit(&mut self, now_ms: u64) {
        self.exits.push(now_ms);
    }

    pub fn registered(&self) -> bool {
        self.registered_ms.is_some()
    }

    pub fn evaluate(&mut self, now_ms: u64) -> Verdict {
        self.exits
            .retain(|&at| now_ms.saturating_sub(at) < CRASH_LOOP_WINDOW_MS);
        if self.exits.len() >= CRASH_LOOP_EXITS {
            return Verdict::RollBack(format!(
                "crash loop: {} exits within 5 min",
                self.exits.len()
            ));
        }
        let elapsed = now_ms.saturating_sub(self.started_ms);
        if self.registered_ms.is_none() {
            if elapsed >= REGISTER_DEADLINE_MS {
                return Verdict::RollBack("did not register with signaling within 120 s".into());
            }
            return Verdict::Wait;
        }
        if elapsed >= GOOD_AFTER_MS {
            return Verdict::Good;
        }
        Verdict::Wait
    }
}

/// What to do when probation fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackPlan {
    /// Restore the previous host and mark the state rolled back.
    Restore,
    /// No usable previous build: keep restarting and report the error.
    GiveUp(String),
}

pub fn plan_rollback(state: &UpdateState, previous_hash: Option<&str>) -> RollbackPlan {
    if state.status != UpdateStatus::Pending {
        return RollbackPlan::GiveUp("not pending; never roll back twice".into());
    }
    match previous_hash {
        None => RollbackPlan::GiveUp("no previous host to restore".into()),
        Some(hash) if hash.eq_ignore_ascii_case(&state.new_hash) => {
            RollbackPlan::GiveUp("previous host is the same build".into())
        }
        Some(_) => RollbackPlan::Restore,
    }
}

/// Short build label for the menu.
pub fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> UpdateState {
        UpdateState {
            status: UpdateStatus::Pending,
            new_hash: "AA11".into(),
            previous_hash: Some("BB22".into()),
            updated_unix_ms: 1,
            reason: None,
        }
    }

    #[test]
    fn registering_then_ten_minutes_is_good() {
        let mut probation = Probation::new(0);
        assert_eq!(probation.evaluate(1_000), Verdict::Wait);
        probation.on_registered(5_000);
        assert_eq!(probation.evaluate(GOOD_AFTER_MS - 1), Verdict::Wait);
        assert_eq!(probation.evaluate(GOOD_AFTER_MS), Verdict::Good);
    }

    #[test]
    fn missing_registration_rolls_back_at_deadline() {
        let mut probation = Probation::new(10_000);
        assert_eq!(
            probation.evaluate(10_000 + REGISTER_DEADLINE_MS - 1),
            Verdict::Wait
        );
        assert!(matches!(
            probation.evaluate(10_000 + REGISTER_DEADLINE_MS),
            Verdict::RollBack(reason) if reason.contains("register")
        ));
    }

    #[test]
    fn crash_loop_rolls_back_even_after_registration() {
        let mut probation = Probation::new(0);
        probation.on_registered(1_000);
        probation.on_exit(2_000);
        probation.on_exit(3_000);
        assert_eq!(probation.evaluate(4_000), Verdict::Wait);
        probation.on_exit(5_000);
        assert!(
            matches!(probation.evaluate(6_000), Verdict::RollBack(r) if r.contains("crash loop"))
        );
    }

    #[test]
    fn exits_outside_window_are_forgotten() {
        let mut probation = Probation::new(0);
        probation.on_registered(0);
        probation.on_exit(0);
        probation.on_exit(1_000);
        probation.on_exit(CRASH_LOOP_WINDOW_MS + 500);
        assert_eq!(
            probation.evaluate(CRASH_LOOP_WINDOW_MS + 600),
            Verdict::Wait
        );
        // Registration is remembered from the first time only.
        probation.on_registered(9_999_999);
        assert_eq!(probation.evaluate(GOOD_AFTER_MS), Verdict::Good);
    }

    #[test]
    fn rollback_happens_at_most_once() {
        let state = pending();
        assert_eq!(plan_rollback(&state, Some("BB22")), RollbackPlan::Restore);
        assert!(matches!(
            plan_rollback(&state, None),
            RollbackPlan::GiveUp(_)
        ));
        assert!(matches!(
            plan_rollback(&state, Some("aa11")),
            RollbackPlan::GiveUp(_)
        ));
        let rolled = state.with_status(UpdateStatus::RolledBack, Some("x".into()), 5);
        assert!(matches!(
            plan_rollback(&rolled, Some("BB22")),
            RollbackPlan::GiveUp(_)
        ));
        assert!(!rolled.needs_probation("AA11"));
        assert_eq!(rolled.updated_unix_ms, 5);
        assert_eq!(rolled.new_hash, "AA11");
    }

    #[test]
    fn probation_only_for_the_recorded_host() {
        let state = pending();
        assert!(state.needs_probation("aa11"));
        assert!(!state.needs_probation("CC33"));
        assert!(!state
            .with_status(UpdateStatus::Good, None, 2)
            .needs_probation("AA11"));
    }

    #[test]
    fn state_file_roundtrip_and_powershell_shape() {
        let dir = std::env::temp_dir().join(format!(
            "bubbleplay-update-test-{}-{}",
            std::process::id(),
            super::super::logic::unix_ms_now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("update-state.json");
        std::fs::write(
            &path,
            b"\xEF\xBB\xBF{\"status\":\"pending\",\"newHash\":\"AA11\",\"previousHash\":null,\"updatedUnixMs\":7}",
        )
        .unwrap();
        let loaded = UpdateState::load(&path).unwrap();
        assert_eq!(loaded.status, UpdateStatus::Pending);
        assert_eq!(loaded.previous_hash, None);
        let rolled = loaded.with_status(UpdateStatus::RolledBack, Some("why".into()), 9);
        rolled.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"rolled-back\""));
        assert_eq!(UpdateState::load(&path).unwrap(), rolled);
        std::fs::write(&path, b"garbage").unwrap();
        assert!(UpdateState::load(&path).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn short_hash_truncates() {
        assert_eq!(short_hash("0123456789abcdef"), "0123456789ab");
        assert_eq!(short_hash("abc"), "abc");
    }
}
