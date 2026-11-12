//! The host's signalling hello and its registration after the proof.

use std::collections::BTreeSet;

use bp_device_key::DeviceId;
use serde_json::{Map, Value, json};

use super::Relay;
use super::pcs::PcRecord;
use super::state::{Phase, Role, key_for};
use super::types::{Action, ConnId};
use super::util;
use crate::RelayRng;

impl Relay {
    /// The host's signalling hello: checks the identity, then sends the
    /// register-challenge (Go server.go:475-504).
    pub(super) fn host_hello(
        &mut self,
        id: ConnId,
        m: &Map<String, Value>,
        now_ms: u64,
        rng: &mut dyn RelayRng,
        out: &mut Vec<Action>,
    ) {
        let Some(pc) = self.conns.get(&id).map(|c| c.pc.clone()) else {
            return;
        };
        let Ok(claimed) = DeviceId::parse(util::text_of(m, "id")) else {
            self.hello_refused(id, "hello.bad-identity", out);
            return;
        };
        let public = match util::decode_b64url(util::text_of(m, "public"), 32) {
            Some(bytes) => {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                key
            }
            None => {
                self.hello_refused(id, "hello.bad-identity", out);
                return;
            }
        };
        if claimed.to_string() != pc || DeviceId::from_public_key(&public) != claimed {
            self.hello_refused(id, "hello.bad-identity", out);
            return;
        }
        // A supplied token must match TOKEN, even for a known PC.
        let token = util::text_of(m, "token");
        let token_valid = !token.is_empty() && self.token_matches(token);
        if !token.is_empty() && !token_valid {
            self.hello_refused(id, "hello.bad-token", out);
            return;
        }
        if !self.can_register(&pc, &public, token_valid) {
            self.hello_refused(id, "hello.cannot-register", out);
            return;
        }
        let nonce: [u8; 32] = util::random(rng);
        let deadline = now_ms + self.config.timings.challenge_ms;
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.phase = Phase::Proof {
                nonce,
                id: claimed,
                public,
                token_valid,
            };
            conn.deadline_ms = deadline;
        }
        self.persist(id, out);
        self.send_text(
            id,
            json!({"type": "register-challenge", "nonce": util::b64url(&nonce)}).to_string(),
            out,
        );
    }

    /// Registration after a valid proof: persists the record first, then makes
    /// the host the current signalling peer and sends hello-ok with a ticket.
    pub(super) fn register_host(
        &mut self,
        id: ConnId,
        pc: &str,
        public: [u8; 32],
        now_ms: u64,
        rng: &mut dyn RelayRng,
        out: &mut Vec<Action>,
    ) {
        // Re-registration keeps the admission list and the remote-pairing flag.
        let (first_seen, admitted, remote_pairing) = match self.pcs.records.get(pc) {
            Some(record) => (
                record.first_seen,
                record.admitted.clone(),
                record.remote_pairing,
            ),
            None => (now_ms, BTreeSet::new(), false),
        };
        self.pcs.records.insert(
            pc.to_owned(),
            PcRecord {
                public,
                first_seen,
                last_seen: now_ms,
                admitted,
                remote_pairing,
            },
        );
        // A pairing window belongs to the host session that opened it.
        self.pair_windows.remove(pc);
        // The adapter must make this durable before it sends the hello-ok below.
        // If the write fails it calls set_store_healthy(false) and closes.
        out.push(Action::PersistPcs(self.pcs.clone()));

        let key = key_for(Role::Host, "", pc);
        if let Some(old) = self.peers.get(&key).copied().filter(|old| *old != id) {
            self.evict(old, 1008, "peer replaced", "peer.replaced", out);
            if let Some(viewer) = self.peers.get(&key_for(Role::Viewer, "", pc)).copied() {
                self.send_text(viewer, json!({"type": "peer-left"}).to_string(), out);
            }
        }
        self.peers.insert(key, id);
        let room = self.rooms.entry(pc.to_owned()).or_default();
        let pending = std::mem::take(&mut room.pending);
        let current_epoch = room.current_epoch.clone();
        self.make_live(id, now_ms);
        let ticket = self.issue_ticket(id, now_ms, rng);
        self.persist(id, out);
        self.send_text(
            id,
            json!({"type": "hello-ok", "ticket": ticket, "pc": pc}).to_string(),
            out,
        );
        // A viewer admitted while this host was away has not been announced to
        // it. Announce it now, before any buffered message, with the epoch the
        // viewer holds.
        let viewer = self.peers.get(&key_for(Role::Viewer, "", pc)).copied();
        let viewer_conn = viewer.and_then(|v| self.conns.get(&v));
        let announced = match (viewer_conn, current_epoch) {
            (Some(v), Some(epoch)) if v.epoch == epoch && !v.closing => Some((epoch, v.pairing)),
            _ => None,
        };
        if let Some((epoch, pairing)) = &announced {
            self.send_text(
                id,
                json!({"type": "peer-joined", "mediaEpoch": epoch, "pairing": pairing}).to_string(),
                out,
            );
        }
        for buffered in pending {
            let current = announced
                .as_ref()
                .is_some_and(|(epoch, _)| *epoch == buffered.epoch);
            if current {
                self.send_text(id, buffered.raw, out);
            }
        }
    }
}
