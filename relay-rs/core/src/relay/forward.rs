//! Traffic on a live socket: relay-consumed messages, forwarding to the other
//! side and the pending buffer for viewers (Go's Relay.forward, server.go:
//! 612-765).

use serde_json::json;

use super::Relay;
use super::state::{Conn, Kind, Pending, Role};
use super::types::{Action, ConnId};
use crate::limits::PENDING_LIMIT;
use crate::protocol::{
    FileText, Invalid, MediaControl, SignalKind, validate_file_binary, validate_file_text,
    validate_media_binary, validate_media_text, validate_signal_binary, validate_signal_text,
};

impl Relay {
    /// A text frame on a live socket.
    pub(super) fn on_live_text(
        &mut self,
        id: ConnId,
        text: &str,
        now_ms: u64,
        out: &mut Vec<Action>,
    ) {
        let Some(conn) = self.open_conn(id) else {
            return;
        };
        if !self.data_epoch_current(&conn) {
            self.close(id, 1008, "invalid packet", out);
            return;
        }
        self.touch(id, now_ms);
        match conn.role.kind() {
            Kind::Signal => self.signal_text(id, &conn, text, out),
            Kind::Media => self.media_text(id, &conn, text, out),
            Kind::File => self.file_text(id, text, out),
        }
    }

    /// A binary frame on a live socket.
    pub(super) fn on_live_binary(
        &mut self,
        id: ConnId,
        bytes: &[u8],
        now_ms: u64,
        out: &mut Vec<Action>,
    ) {
        let Some(conn) = self.open_conn(id) else {
            return;
        };
        if !self.data_epoch_current(&conn) {
            self.close(id, 1008, "invalid packet", out);
            return;
        }
        self.touch(id, now_ms);
        let side = conn.role.side();
        match conn.role.kind() {
            // Go drops an invalid signal or media frame and keeps the socket.
            Kind::Signal => {
                if validate_signal_binary(bytes).is_ok() {
                    self.forward_binary(&conn, bytes, out);
                }
            }
            Kind::Media => {
                if validate_media_binary(side, bytes).is_ok() {
                    self.forward_binary(&conn, bytes, out);
                }
            }
            Kind::File => match validate_file_binary(side, bytes) {
                Ok(_) => self.forward_binary(&conn, bytes, out),
                Err(_) => {
                    // Go closes the other end of the pair, then this socket.
                    if let Some(other) = self.other_of(&conn) {
                        self.evict(other, 1001, "file-peer-left", out);
                    }
                    self.close(id, 1008, "invalid packet", out);
                }
            },
        }
    }

    /// Data sockets must carry the room's current epoch (Go's forward check).
    fn data_epoch_current(&self, conn: &Conn) -> bool {
        conn.role.kind() == Kind::Signal
            || self.current_epoch(&conn.pc) == Some(conn.epoch.as_str())
    }

    /// Text on a signalling socket: ping, bye, capability caching, forwarding,
    /// and buffering of viewer messages while the host is away.
    fn signal_text(&mut self, id: ConnId, conn: &Conn, text: &str, out: &mut Vec<Action>) {
        let msg = match validate_signal_text(conn.role.side(), text, &conn.epoch) {
            Ok(msg) => msg,
            Err(Invalid::Drop) => return,
            Err(Invalid::Close) => {
                self.close(id, 1008, "invalid packet", out);
                return;
            }
        };
        match msg.kind() {
            SignalKind::Ping => {
                self.send_text(id, json!({"type": "pong"}).to_string(), out);
            }
            SignalKind::Bye => {
                if let Some(other) = self.other_of(conn) {
                    self.send_text(other, json!({"type": "peer-left"}).to_string(), out);
                }
                self.close(id, 1000, "bye", out);
            }
            kind => {
                if conn.role == Role::Host && kind.cached_for_viewers() {
                    let room = self.rooms.entry(conn.pc.clone()).or_default();
                    room.cached.insert(kind.as_str(), text.to_owned());
                }
                if let Some(other) = self.other_of(conn) {
                    self.send_text(other, text.to_owned(), out);
                } else if conn.role == Role::Viewer && msg.bufferable() {
                    self.buffer(
                        &conn.pc,
                        Pending {
                            kind: kind.as_str().to_owned(),
                            action: msg.action().to_owned(),
                            raw: text.to_owned(),
                        },
                    );
                }
            }
        }
    }

    /// Text on a media socket: ping, or a viewer's media-control message.
    fn media_text(&mut self, id: ConnId, conn: &Conn, text: &str, out: &mut Vec<Action>) {
        match validate_media_text(conn.role.side(), text, &conn.epoch) {
            Ok(MediaControl::Ping) => {
                self.send_text(id, json!({"type": "pong"}).to_string(), out);
            }
            Ok(_) => {
                let viewer_only = conn.role == Role::MediaViewer;
                if let Some(other) = self.other_of(conn).filter(|_| viewer_only) {
                    self.send_text(other, text.to_owned(), out);
                }
            }
            Err(Invalid::Drop) => {}
            Err(Invalid::Close) => self.close(id, 1008, "invalid packet", out),
        }
    }

    /// Text on a file socket: only ping is allowed.
    fn file_text(&mut self, id: ConnId, text: &str, out: &mut Vec<Action>) {
        match validate_file_text(text) {
            Ok(FileText::Ping) => {
                self.send_text(id, json!({"type": "pong"}).to_string(), out);
            }
            Err(_) => self.close(id, 1008, "invalid packet", out),
        }
    }

    /// Forwards a binary frame to the other side of the same family, if present.
    fn forward_binary(&self, conn: &Conn, bytes: &[u8], out: &mut Vec<Action>) {
        if let Some(other) = self.other_of(conn) {
            self.send_binary(other, bytes, out);
        }
    }

    /// The socket that is the current peer on the other side of `conn`.
    fn other_of(&self, conn: &Conn) -> Option<ConnId> {
        let role = conn.role.other()?;
        let key = super::state::key_for(role, &conn.epoch, &conn.pc);
        self.peers.get(&key).copied().filter(|id| self.is_open(*id))
    }

    /// Keeps a viewer message for the next host. media-control and first-frame
    /// replace older messages of the same kind (same action for media-control).
    /// The buffer holds at most PENDING_LIMIT entries, oldest dropped first.
    fn buffer(&mut self, pc: &str, pending: Pending) {
        let room = self.rooms.entry(pc.to_owned()).or_default();
        if pending.kind == "media-control" || pending.kind == "first-frame" {
            room.pending.retain(|old| {
                !(old.kind == pending.kind
                    && (pending.kind == "first-frame" || old.action == pending.action))
            });
        }
        room.pending.push(pending);
        if room.pending.len() > PENDING_LIMIT {
            room.pending.remove(0);
        }
    }
}
