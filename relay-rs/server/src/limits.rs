//! The per-client counters of the unauthenticated HTTP side: `GET /presence` and held navigations.
//! Memory only.

use std::sync::{Mutex, MutexGuard, PoisonError};

use bp_relay_core::gate::Prefix;
use bp_relay_core::holds::HoldCounter;
use bp_relay_core::presence::PresenceLimiter;

/// The counters. The presence limiter is locked on its own, before the relay.
#[derive(Default)]
pub struct Limits {
    presence: Mutex<PresenceLimiter>,
    holds: Mutex<HoldCounter>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Limits {
    /// The presence limiter, locked for one call.
    pub fn presence(&self) -> MutexGuard<'_, PresenceLimiter> {
        lock(&self.presence)
    }

    /// Counts one held navigation for `prefix`. False when a limit is reached.
    pub fn acquire_hold(&self, prefix: Prefix) -> bool {
        lock(&self.holds).acquire(prefix)
    }

    /// Ends one held navigation for `prefix`.
    pub fn release_hold(&self, prefix: Prefix) {
        lock(&self.holds).release(prefix);
    }
}
