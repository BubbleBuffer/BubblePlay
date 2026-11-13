//! Limits on sockets that have not finished their hello or proof.
//!
//! The device id of a PC is public, so anyone can open sockets to it. A pending socket costs
//! the relay a connection slot until its deadline. Pending sockets are therefore capped on their
//! own, per client prefix and in total, and a full relay drops a pending socket instead of
//! refusing a new one. Authenticated sockets are never dropped for this.

use std::collections::BTreeMap;

use super::state::Phase;
use super::types::{Action, ConnId};
use super::{Relay, reasons};
use crate::gate::Prefix;

/// Pending sockets one client prefix may hold. A newer one from that prefix drops its oldest.
pub const PRE_AUTH_MAX_PER_PREFIX: usize = 4;

/// Pending sockets the relay holds in total. Past it, the oldest socket of the busiest prefix
/// is dropped.
pub const PRE_AUTH_MAX_TOTAL: usize = 8;

/// Deadline for the first hello on a socket, in milliseconds.
pub const HELLO_TIMEOUT_MS: u64 = 5_000;

/// Deadline for the register-proof after the challenge, in milliseconds.
pub const PROOF_TIMEOUT_MS: u64 = 10_000;

/// Close code of a pending socket dropped to make room (try again later).
pub const EVICTED_CLOSE_CODE: u16 = 1013;

/// Log class of a pending socket dropped to make room.
const EVICT_CLASS: &str = "upgrade.pre-auth-evicted";

impl Relay {
    /// Makes room for one more socket from `prefix`, closing pending sockets where needed and
    /// appending their `Close` actions to `out`. False when the relay is full of sockets that
    /// finished their handshake.
    pub(super) fn make_room(&mut self, prefix: Prefix, out: &mut Vec<Action>) -> bool {
        while let Some(victim) = self.eviction_victim(prefix) {
            self.close(
                victim,
                EVICTED_CLOSE_CODE,
                reasons::TOO_MANY_PENDING,
                EVICT_CLASS,
                out,
            );
        }
        self.open_count() < self.config.max_connections
    }

    fn open_count(&self) -> usize {
        self.conns.values().filter(|conn| !conn.closing).count()
    }

    /// The pending socket to drop before `prefix` may open another, if one has to go. Ids grow
    /// with age, so the smallest id is the oldest socket.
    fn eviction_victim(&self, prefix: Prefix) -> Option<ConnId> {
        let mut by_prefix: BTreeMap<Prefix, Vec<ConnId>> = BTreeMap::new();
        for (id, conn) in &self.conns {
            if !conn.closing && conn.phase != Phase::Live {
                by_prefix.entry(conn.prefix).or_default().push(*id);
            }
        }
        let own = by_prefix.get(&prefix).map_or(0, Vec::len);
        if own >= PRE_AUTH_MAX_PER_PREFIX {
            return by_prefix.get(&prefix).and_then(|ids| ids.first().copied());
        }
        let total: usize = by_prefix.values().map(Vec::len).sum();
        if total >= PRE_AUTH_MAX_TOTAL || self.open_count() >= self.config.max_connections {
            // The busiest prefix pays; among equals, the one holding the oldest socket.
            return by_prefix
                .values()
                .min_by_key(|ids| (usize::MAX - ids.len(), ids.first().copied()))
                .and_then(|ids| ids.first().copied());
        }
        None
    }
}
