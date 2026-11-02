//! Hibernation support: socket snapshots, rehydration and the next deadline.
//!
//! The adapter keeps each socket's latest `Action::Snapshot` in that socket's
//! attachment. After a hibernation it passes all the attachments back to
//! `Relay::rehydrate`, and it asks `Relay::next_deadline` when to wake for `tick`.

use super::pairing::PairWindow;
use super::pcs::PcTable;
use super::state::{Conn, Kind, Phase, Role, Ticket, key_for};
use super::types::{
    Action, ConnId, ConnPhase, ConnSnapshot, CoreConfig, Route, SnapshotPairWindow, SnapshotTicket,
};
use super::util;
use super::{Relay, reasons};
use crate::gate::Prefix;
use crate::limits::PAIR_PC_FAILURES;
use crate::protocol::validate_epoch;

fn phase_of(phase: &Phase) -> ConnPhase {
    match phase {
        Phase::Hello => ConnPhase::Hello,
        Phase::Proof { .. } => ConnPhase::HostProof,
        Phase::ViewerProof { .. } => ConnPhase::ViewerProof,
        Phase::Live => ConnPhase::Live,
    }
}

impl Relay {
    /// The snapshot of an open socket. None once the socket is closing or gone.
    /// The adapter calls this right after `upgrade` to attach the first snapshot.
    pub fn snapshot_of(&self, id: ConnId) -> Option<ConnSnapshot> {
        let conn = self.conns.get(&id).filter(|conn| !conn.closing)?;
        let host = conn.role == Role::Host;
        let pair_window = self
            .pair_windows
            .get(&conn.pc)
            .filter(|_| host)
            .map(|window| SnapshotPairWindow {
                admit: window.admit,
                expires_ms: window.expires_ms,
                failures: window.failures,
            });
        Some(ConnSnapshot {
            id,
            route: conn.route,
            pc: conn.pc.clone(),
            epoch: conn.epoch.clone(),
            phase: phase_of(&conn.phase),
            device: conn.device.clone(),
            pairing: conn.pairing,
            activity_ms: conn.activity_ms,
            ticket: conn.ticket.as_ref().map(|ticket| SnapshotTicket {
                hash: ticket.hash,
                expires_ms: ticket.expires_ms,
            }),
            pair_window,
        })
    }

    /// Emits the socket's snapshot and marks its activity as saved. Does nothing
    /// for a socket that is closing or gone.
    pub(super) fn persist(&mut self, id: ConnId, out: &mut Vec<Action>) {
        let Some(snapshot) = self.snapshot_of(id) else {
            return;
        };
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.saved_activity_ms = conn.activity_ms;
        }
        out.push(Action::Snapshot(id, snapshot));
    }

    /// Emits the snapshot of the PC's current host socket, if it is open.
    pub(super) fn persist_host(&mut self, pc: &str, out: &mut Vec<Action>) {
        if let Some(host) = self.peers.get(&key_for(Role::Host, "", pc)).copied() {
            self.persist(host, out);
        }
    }

    /// The activity step. A snapshot is written only once the activity has moved
    /// this far past the last one written, so a restored activity time is never
    /// more than one step behind the real one.
    pub(super) fn step_ms(&self) -> u64 {
        (self.config.timings.idle_ms / 4).max(1)
    }

    /// Records activity on a live socket at `at_ms`: moves its idle deadline and
    /// emits a snapshot once the activity has moved a step past the last one.
    pub(super) fn record_activity(&mut self, id: ConnId, at_ms: u64, out: &mut Vec<Action>) {
        let idle = self.config.timings.idle_ms;
        let step = self.step_ms();
        let Some(conn) = self
            .conns
            .get_mut(&id)
            .filter(|conn| !conn.closing && conn.phase == Phase::Live)
        else {
            return;
        };
        conn.activity_ms = conn.activity_ms.max(at_ms);
        conn.deadline_ms = conn.activity_ms.saturating_add(idle);
        let due = conn.activity_ms.saturating_sub(conn.saved_activity_ms) >= step;
        if due {
            self.persist(id, out);
        }
    }

    /// Records activity on a live socket that the core did not see as a
    /// message, such as a ping the runtime answered itself. `at_ms` is when that
    /// activity happened. Moves the idle deadline and emits a snapshot once the
    /// activity has moved a step past the last one.
    pub fn note_activity(&mut self, id: ConnId, at_ms: u64) -> Vec<Action> {
        let mut out = Vec::new();
        self.record_activity(id, at_ms, &mut out);
        out
    }

    /// Drops tickets that have expired, emitting a snapshot for each socket that
    /// held one. `valid_ticket` already refuses an expired ticket, so this only
    /// changes what is stored.
    pub(super) fn expire_tickets(&mut self, now_ms: u64, out: &mut Vec<Action>) {
        let expired: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, conn)| {
                !conn.closing
                    && conn
                        .ticket
                        .as_ref()
                        .is_some_and(|ticket| now_ms >= ticket.expires_ms)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(conn) = self.conns.get_mut(&id) {
                conn.ticket = None;
            }
            self.persist(id, out);
        }
    }

    /// The earliest time `tick` has work to do: a socket's hello, challenge or
    /// idle deadline, a ticket's expiry, a pairing window's expiry, or the end
    /// of a prefix's failure count window. None when nothing is pending. `tick`
    /// at this time, and not before, does what is due.
    pub fn next_deadline(&self) -> Option<u64> {
        let fail_window = self.config.timings.pair_fail_window_ms;
        let sockets = self
            .conns
            .values()
            .filter(|conn| !conn.closing)
            .flat_map(|conn| {
                [
                    Some(conn.deadline_ms),
                    conn.ticket.as_ref().map(|ticket| ticket.expires_ms),
                ]
            })
            .flatten();
        let windows = self.pair_windows.values().map(|window| window.expires_ms);
        let failures = self
            .pair_failures
            .values()
            .map(|failures| failures.start_ms.saturating_add(fail_window));
        sockets.chain(windows).chain(failures).min()
    }

    /// Rebuilds a relay from the snapshots of the sockets still open after a
    /// hibernation: one snapshot per socket, in any order. Returns the relay and
    /// one `Close` per socket that cannot be restored; the adapter carries those
    /// out and then calls `on_close`.
    ///
    /// Restored: live sockets, their epochs and tickets, the admitted viewers,
    /// the host's pairing window, and the rooms they belong to. A live socket's
    /// activity is the saved value plus one step (at most `now_ms`), so it is
    /// never closed early and an idle one closes within one step of its real
    /// deadline.
    ///
    /// Lost on hibernation, and so not restored: viewer messages buffered while
    /// the host is away, the per-prefix pairing failure counts (the PC-wide count
    /// in the host's pairing window does survive), and the challenge nonces.
    /// A restored socket's `prefix` is a placeholder that a live socket never reads.
    ///
    /// Closed with 1008 and an empty reason: any socket that is not live; a route
    /// or identity that does not check out; the lower-id duplicate of a live
    /// host, viewer or data socket (ids only grow, so the higher id is the one
    /// that replaced it, as the live relay's "peer replaced" decided); a host
    /// with no PC record; a viewer that is not
    /// admitted and not in pairing mode; a data socket whose epoch is not its
    /// room's current epoch; and a second snapshot for the same socket id.
    pub fn rehydrate(
        config: CoreConfig,
        pcs: PcTable,
        mut snapshots: Vec<ConnSnapshot>,
        now_ms: u64,
    ) -> (Relay, Vec<Action>) {
        snapshots.sort_by_key(|snapshot| snapshot.id);
        let mut relay = Relay::new(config, pcs);
        relay.clock_ms = now_ms;
        relay.next_id = snapshots
            .last()
            .map_or(1, |snapshot| snapshot.id.0.saturating_add(1));
        let mut out = Vec::new();
        let mut signalling = Vec::new();
        let mut data = Vec::new();
        let mut previous: Option<ConnId> = None;
        for snapshot in snapshots {
            let id = snapshot.id;
            if previous == Some(id) {
                relay.restore_refused(id, &snapshot, "restore.duplicate-id", &mut out);
                continue;
            }
            previous = Some(id);
            match relay.restore_conn(&snapshot, now_ms) {
                Some(conn) if conn.role.kind() == Kind::Signal => {
                    signalling.push((snapshot, conn));
                }
                Some(conn) => data.push((snapshot, conn)),
                None => relay.restore_refused(id, &snapshot, "restore.invalid", &mut out),
            }
        }
        // Signalling first: a data socket's epoch is checked against its room,
        // and the room's epoch comes from the viewer. Newest first in each
        // group, so a replacement wins over the socket it replaced.
        for (snapshot, conn) in signalling.into_iter().rev() {
            let id = snapshot.id;
            let role = conn.role;
            let adopted = match role {
                Role::Host => relay.adopt_host(id, &snapshot, conn, now_ms),
                Role::Viewer => relay.adopt_viewer(id, conn),
                _ => false,
            };
            if !adopted {
                relay.restore_refused(id, &snapshot, "restore.not-adopted", &mut out);
            }
        }
        for (snapshot, conn) in data.into_iter().rev() {
            let id = snapshot.id;
            if !relay.adopt_data(id, conn) {
                relay.restore_refused(id, &snapshot, "restore.data-stale", &mut out);
            }
        }
        (relay, out)
    }

    /// Closes a socket that cannot be restored after hibernation and logs why.
    fn restore_refused(
        &self,
        id: ConnId,
        snapshot: &ConnSnapshot,
        class: &'static str,
        out: &mut Vec<Action>,
    ) {
        out.push(Action::Close(
            id,
            1008,
            reasons::SESSION_NOT_RESTORED.to_owned(),
        ));
        let pc = util::canonical_pc(&snapshot.pc);
        self.log(
            crate::log::Level::Warn,
            "socket refused",
            pc.as_deref(),
            serde_json::json!({
                "class": class,
                "status": 1008,
                "reason": reasons::SESSION_NOT_RESTORED,
                "role": snapshot.route.role().name(),
            }),
        );
    }

    /// Checks one snapshot and builds its live socket, or None when it cannot be
    /// live: not in the live phase, a version probe, a route whose identity or
    /// epoch is malformed, or fields that belong to another role.
    fn restore_conn(&self, snapshot: &ConnSnapshot, now_ms: u64) -> Option<Conn> {
        if snapshot.phase != ConnPhase::Live || snapshot.route == Route::HostVersionWs {
            return None;
        }
        let role = snapshot.route.role();
        let pc = util::canonical_pc(&snapshot.pc).filter(|pc| *pc == snapshot.pc)?;
        let device_ok = match role {
            Role::Viewer => {
                util::canonical_pc(&snapshot.device).is_some_and(|device| device == snapshot.device)
            }
            _ => snapshot.device.is_empty(),
        };
        let epoch_ok = match role {
            Role::Host => snapshot.epoch.is_empty(),
            Role::VersionProbe => false,
            _ => validate_epoch(&snapshot.epoch),
        };
        let host = role == Role::Host;
        if !device_ok
            || !epoch_ok
            || (snapshot.pairing && role != Role::Viewer)
            || (!host && snapshot.pair_window.is_some())
        {
            return None;
        }
        let ticket = snapshot
            .ticket
            .as_ref()
            .filter(|ticket| now_ms < ticket.expires_ms)
            .map(|ticket| Ticket {
                hash: ticket.hash,
                expires_ms: ticket.expires_ms,
            });
        let saved = snapshot.activity_ms;
        let activity = saved.saturating_add(self.step_ms()).min(now_ms).max(saved);
        Some(Conn {
            route: snapshot.route,
            role,
            pc,
            epoch: snapshot.epoch.clone(),
            phase: Phase::Live,
            deadline_ms: activity.saturating_add(self.config.timings.idle_ms),
            closing: false,
            ticket,
            prefix: Prefix::V4([0; 3]),
            device: snapshot.device.clone(),
            pairing: snapshot.pairing,
            activity_ms: activity,
            saved_activity_ms: saved,
        })
    }

    /// Restores a host. Refused when the PC has no record or already has a live
    /// host.
    fn adopt_host(&mut self, id: ConnId, snapshot: &ConnSnapshot, conn: Conn, now_ms: u64) -> bool {
        let pc = conn.pc.clone();
        let key = key_for(Role::Host, "", &pc);
        if !self.pcs.records.contains_key(&pc) || self.peers.contains_key(&key) {
            return false;
        }
        // An expired window is gone; a window at the failure limit was dropped
        // by the live relay, so it is not restored either.
        let window = snapshot.pair_window.as_ref().and_then(|window| {
            (now_ms < window.expires_ms && window.failures < PAIR_PC_FAILURES).then_some(
                PairWindow {
                    admit: window.admit,
                    expires_ms: window.expires_ms,
                    failures: window.failures,
                },
            )
        });
        self.peers.insert(key, id);
        // The room holds the PC's current epoch and pending list. Create it
        // when the host comes back, as the live host join does.
        self.rooms.entry(pc.clone()).or_default();
        if let Some(window) = window {
            self.pair_windows.insert(pc, window);
        }
        self.conns.insert(id, conn);
        true
    }

    /// Restores an admitted viewer, or a viewer in pairing mode. Refused when
    /// the PC already has a live viewer, or the device is not admitted.
    fn adopt_viewer(&mut self, id: ConnId, conn: Conn) -> bool {
        let pc = conn.pc.clone();
        let key = key_for(Role::Viewer, "", &pc);
        if self.peers.contains_key(&key) {
            return false;
        }
        let admitted = self
            .pcs
            .records
            .get(&pc)
            .is_some_and(|record| record.admitted.contains(&conn.device));
        if !conn.pairing && !admitted {
            return false;
        }
        let epoch = conn.epoch.clone();
        self.peers.insert(key, id);
        self.rooms.entry(pc).or_default().current_epoch = Some(epoch);
        self.conns.insert(id, conn);
        true
    }

    /// Restores a media or file socket. Refused when its epoch is not the
    /// room's current epoch, or its peer slot is taken.
    fn adopt_data(&mut self, id: ConnId, conn: Conn) -> bool {
        let current = self
            .rooms
            .get(&conn.pc)
            .and_then(|room| room.current_epoch.as_deref());
        if current != Some(conn.epoch.as_str()) {
            return false;
        }
        let key = key_for(conn.role, &conn.epoch, &conn.pc);
        if self.peers.contains_key(&key) {
            return false;
        }
        self.peers.insert(key, id);
        self.conns.insert(id, conn);
        true
    }
}
