//! Closing, evicting and forgetting sockets, and sending to them.

use super::*;

impl Relay {
    /// Marks a socket closing and queues the Close action. Idempotent.
    /// `class` names why, for the log line: a short dotted word such as
    /// `pairing.wrong-code`. Every close is logged with its status, reason and class.
    pub(super) fn close(
        &mut self,
        id: ConnId,
        code: u16,
        reason: &str,
        class: &'static str,
        out: &mut Vec<Action>,
    ) {
        let seen = self.conns.get_mut(&id).filter(|c| !c.closing).map(|conn| {
            conn.closing = true;
            // A viewer that has not been admitted yet is known by the id it claimed.
            let device = match &conn.phase {
                Phase::ViewerProof { id, .. } if conn.device.is_empty() => id.to_string(),
                _ => conn.device.clone(),
            };
            (conn.role, device, conn.pc.clone())
        });
        if let Some((role, device, pc)) = seen {
            out.push(Action::Close(id, code, reason.to_owned()));
            let level = if code == 1008 {
                crate::log::Level::Warn
            } else {
                crate::log::Level::Info
            };
            let msg = if code == 1008 {
                "socket refused"
            } else {
                "socket closed"
            };
            let fields = serde_json::json!({
                "class": class,
                "status": code,
                "reason": reason,
                "role": role.name(),
                "peer": (!device.is_empty()).then(|| crate::log::short_id(&device)),
            });
            self.log(level, msg, (!pc.is_empty()).then_some(pc.as_str()), fields);
        }
    }

    /// Closes a socket and drops it from the peer table without telling its
    /// peer. Go does this when a socket is replaced or cleared as stale.
    pub(super) fn evict(
        &mut self,
        id: ConnId,
        code: u16,
        reason: &str,
        class: &'static str,
        out: &mut Vec<Action>,
    ) {
        self.close(id, code, reason, class, out);
        if let Some(conn) = self.conns.remove(&id) {
            let key = key_for(conn.role, &conn.epoch, &conn.pc);
            if self.peers.get(&key) == Some(&id) {
                self.peers.remove(&key);
            }
        }
    }

    /// Queues a text frame unless the socket is gone.
    pub(super) fn send_text(&self, id: ConnId, text: String, out: &mut Vec<Action>) {
        if self.is_open(id) {
            out.push(Action::SendText(id, text));
        }
    }

    /// Queues a binary frame unless the socket is gone.
    pub(super) fn send_binary(&self, id: ConnId, bytes: &[u8], out: &mut Vec<Action>) {
        if self.is_open(id) {
            out.push(Action::SendBinary(id, bytes.to_vec()));
        }
    }

    /// Go's removePeerLocked: forgets the socket and tells its peer. Safe to
    /// call for a socket that was already forgotten.
    pub(super) fn forget(&mut self, id: ConnId, out: &mut Vec<Action>) {
        let Some(conn) = self.conns.remove(&id) else {
            return;
        };
        let kind = conn.role.kind();
        let key = key_for(conn.role, &conn.epoch, &conn.pc);
        if self.peers.get(&key) == Some(&id) {
            self.peers.remove(&key);
            if conn.role == Role::Viewer {
                self.forget_viewer_state(&conn, out);
            }
            if conn.role == Role::Host {
                // The host's signalling socket closing removes its pairing window.
                self.pair_windows.remove(&conn.pc);
            }
            self.notify_peer_left(&conn, kind, out);
        }
        if !conn.pc.is_empty() && !self.conns.values().any(|c| c.pc == conn.pc) {
            self.rooms.remove(&conn.pc);
        }
    }

    /// A viewer left: its pending buffer goes, its epoch is cleared if current,
    /// and its data sockets for that epoch are closed.
    pub(super) fn forget_viewer_state(&mut self, conn: &Conn, out: &mut Vec<Action>) {
        if let Some(room) = self.rooms.get_mut(&conn.pc) {
            room.pending.clear();
            if room.current_epoch.as_deref() == Some(conn.epoch.as_str()) {
                room.current_epoch = None;
            }
        }
        let departed: Vec<ConnId> = self
            .peers
            .iter()
            .filter(|(k, _)| k.kind != Kind::Signal && k.pc == conn.pc && k.epoch == conn.epoch)
            .map(|(_, id)| *id)
            .collect();
        for id in departed {
            self.close(id, 1001, "viewer departed", "viewer.left", out);
        }
    }

    /// Tells the other side of a departed socket: peer-left for signalling,
    /// media-peer present=false for media, and file-peer-left (a close) for files.
    pub(super) fn notify_peer_left(&mut self, conn: &Conn, kind: Kind, out: &mut Vec<Action>) {
        let Some(other_role) = conn.role.other() else {
            return;
        };
        let other_key = key_for(other_role, &conn.epoch, &conn.pc);
        let Some(other) = self.peers.get(&other_key).copied() else {
            return;
        };
        match kind {
            Kind::Signal => self.send_text(
                other,
                serde_json::json!({"type": "peer-left"}).to_string(),
                out,
            ),
            Kind::Media => self.send_text(
                other,
                serde_json::json!({"type": Role::peer_event(kind), "present": false}).to_string(),
                out,
            ),
            Kind::File => {
                self.peers.remove(&other_key);
                self.close(other, 1001, "file-peer-left", "file.peer-left", out);
            }
        }
    }
}
