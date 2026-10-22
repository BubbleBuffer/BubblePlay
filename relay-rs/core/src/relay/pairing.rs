//! Pairing windows and their rate limits (M2 section 3).
//!
//! The host opens a window with `pair-open`. A viewer whose ID is not admitted
//! but who presents the window's value is admitted in pairing mode. Windows
//! and failure counts live in memory only. Failures are counted per client
//! prefix (5 per window) and per PC window (20 before the window is dropped).

use serde_json::json;

use super::Relay;
use super::state::{Role, key_for};
use super::types::{Action, ConnId};
use super::util;
use crate::RelayRng;
use crate::gate::{MAX_TRACKED_PREFIXES, Prefix};
use crate::limits::{PAIR_PC_FAILURES, PAIR_PREFIX_FAILURES};

/// One open pairing window for a PC.
#[derive(Clone, Debug)]
pub(super) struct PairWindow {
    /// The value a viewer must present: the `admit` of the host's pair-open.
    pub admit: [u8; 32],
    /// Unix ms at which the window expires.
    pub expires_ms: u64,
    /// Failed attempts against this window.
    pub failures: u32,
}

/// Failed pairing attempts from one client prefix in its current window.
#[derive(Clone, Copy, Debug)]
pub(super) struct PairFailures {
    /// Unix ms at which the counting window began.
    pub start_ms: u64,
    /// Failures in this window.
    pub count: u32,
}

impl Relay {
    /// Opens (or replaces) the PC's pairing window. Only the host can call this.
    pub(super) fn pair_open(&mut self, pc: &str, admit: [u8; 32], now_ms: u64) {
        let expires_ms = now_ms + self.config.timings.pair_window_ms;
        self.pair_windows.insert(
            pc.to_owned(),
            PairWindow {
                admit,
                expires_ms,
                failures: 0,
            },
        );
    }

    /// Closes the PC's pairing window, if one is open.
    pub(super) fn pair_close(&mut self, pc: &str) {
        self.pair_windows.remove(pc);
    }

    /// Drops expired windows and expired failure counts. Called from `tick`.
    pub(super) fn expire_pairing(&mut self, now_ms: u64) {
        let fail_window = self.config.timings.pair_fail_window_ms;
        self.pair_windows
            .retain(|_, window| now_ms < window.expires_ms);
        self.pair_failures
            .retain(|_, failures| now_ms < failures.start_ms + fail_window);
    }

    /// A viewer with a valid proof whose ID is not admitted presents `pair`.
    /// Any refusal closes the socket with 1008 and an empty reason, and nothing
    /// is compared once the prefix is over its limit.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn pair_attempt(
        &mut self,
        id: ConnId,
        device: String,
        pair: [u8; 32],
        turn: bool,
        now_ms: u64,
        rng: &mut dyn RelayRng,
        out: &mut Vec<Action>,
    ) {
        let Some(conn) = self.open_conn(id) else {
            return;
        };
        let prefix = conn.prefix;
        let pc = conn.pc.clone();
        let fail_window = self.config.timings.pair_fail_window_ms;

        // A prefix with no counted failures is new. While the map is full, new
        // prefixes are refused, as the gate refuses them.
        let tracked = self.pair_failures.get(&prefix).copied();
        if tracked.is_none() && self.pair_failures.len() >= MAX_TRACKED_PREFIXES {
            self.hello_refused(id, out);
            return;
        }
        if let Some(failures) = tracked {
            let live = now_ms < failures.start_ms + fail_window;
            if live && failures.count >= PAIR_PREFIX_FAILURES {
                self.hello_refused(id, out);
                return;
            }
        }

        let matched = self
            .pair_windows
            .get(&pc)
            .filter(|window| now_ms < window.expires_ms)
            .is_some_and(|window| util::ct_eq(&pair, &window.admit));
        if matched {
            self.pair_windows.remove(&pc);
            self.viewer_admit(id, device, turn, true, now_ms, rng, out);
            return;
        }

        // A failed attempt counts against the prefix and against the PC window.
        self.count_pair_failure(prefix, now_ms);
        let failures = self
            .pair_windows
            .get_mut(&pc)
            .filter(|window| now_ms < window.expires_ms)
            .map(|window| {
                window.failures += 1;
                window.failures
            });
        if failures.is_some_and(|count| count >= PAIR_PC_FAILURES) {
            self.pair_windows.remove(&pc);
            if let Some(host) = self.peers.get(&key_for(Role::Host, "", &pc)).copied() {
                self.send_text(host, json!({"type": "pair-closed"}).to_string(), out);
            }
        }
        self.hello_refused(id, out);
    }

    /// Counts one failed attempt from `prefix`, starting a new window when the
    /// old one has run out.
    fn count_pair_failure(&mut self, prefix: Prefix, now_ms: u64) {
        let fail_window = self.config.timings.pair_fail_window_ms;
        let entry = self.pair_failures.entry(prefix).or_insert(PairFailures {
            start_ms: now_ms,
            count: 0,
        });
        if now_ms >= entry.start_ms + fail_window {
            entry.start_ms = now_ms;
            entry.count = 0;
        }
        entry.count += 1;
    }
}
