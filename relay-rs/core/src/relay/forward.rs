//! Traffic on a live socket: relay-consumed messages, forwarding to the other
//! side and the pending buffer for viewers (Go's Relay.forward, server.go:
//! 612-765).

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::state::{Conn, Kind, Pending, Phase, Role};
use super::types::{Action, ConnId};
use super::util;
use super::{Relay, reasons};
use crate::limits::PENDING_LIMIT;
use crate::protocol::{
    FileText, Invalid, MediaControl, SignalKind, validate_file_binary, validate_file_text,
    validate_media_binary, validate_media_text, validate_signal_text,
};

/// Dropped client-log messages are reported at most this often.
const CLIENT_LOG_LINE_EVERY_MS: u64 = 60_000;

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
            self.close(id, 1008, reasons::INVALID_PACKET, "data.stale-epoch", out);
            return;
        }
        self.record_activity(id, now_ms, out);
        match conn.role.kind() {
            Kind::Signal => self.signal_text(id, &conn, text, now_ms, out),
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
            self.close(id, 1008, reasons::INVALID_PACKET, "data.stale-epoch", out);
            return;
        }
        self.record_activity(id, now_ms, out);
        let side = conn.role.side();
        match conn.role.kind() {
            // Binary frames are never forwarded on a signalling socket (decision
            // 15: the /ws allowlist is text only). Dropped in both directions,
            // socket kept, the same as a frame in a disallowed direction.
            Kind::Signal => {}
            // Go drops an invalid media frame and keeps the socket.
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
                        self.evict(other, 1001, "file-peer-left", "file.peer-left", out);
                    }
                    self.close(
                        id,
                        1008,
                        reasons::INVALID_PACKET,
                        "packet.invalid-file",
                        out,
                    );
                }
            },
        }
    }

    /// Data sockets must carry the room's current epoch (Go's forward check).
    fn data_epoch_current(&self, conn: &Conn) -> bool {
        conn.role.kind() == Kind::Signal
            || self.current_epoch(&conn.pc) == Some(conn.epoch.as_str())
    }

    /// Text on a signalling socket: ping, bye, the messages the relay consumes
    /// (admit, pair-open, pair-close), forwarding, and buffering of viewer
    /// messages while the host is away.
    fn signal_text(
        &mut self,
        id: ConnId,
        conn: &Conn,
        text: &str,
        now_ms: u64,
        out: &mut Vec<Action>,
    ) {
        let msg = match validate_signal_text(conn.role.side(), text, &conn.epoch) {
            Ok(msg) => msg,
            Err(Invalid::Drop) => {
                self.note_dropped_signal(text, now_ms);
                return;
            }
            Err(Invalid::Close) => {
                self.close(
                    id,
                    1008,
                    reasons::INVALID_PACKET,
                    "packet.invalid-signal",
                    out,
                );
                return;
            }
        };
        match msg.kind() {
            SignalKind::Ping => {
                self.send_text(id, json!({"type": "pong"}).to_string(), out);
            }
            SignalKind::Bye => {
                // A pairing-mode viewer's bye is dropped (M2 section 3).
                if conn.pairing {
                    return;
                }
                if let Some(other) = self.other_of(conn) {
                    self.send_text(other, json!({"type": "peer-left"}).to_string(), out);
                }
                self.close(id, 1000, "bye", "bye", out);
            }
            SignalKind::Admit => self.admit_list(conn, text, out),
            SignalKind::PairOpen => {
                let admit: Option<[u8; 32]> = util::parse_object(text)
                    .and_then(|m| util::decode_b64url(util::text_of(&m, "admit"), 32))
                    .and_then(|bytes| bytes.try_into().ok());
                if let Some(admit) = admit {
                    self.pair_open(&conn.pc, admit, now_ms);
                    self.persist(id, out);
                }
            }
            SignalKind::PairClose => {
                if self.pair_close(&conn.pc) {
                    self.persist(id, out);
                }
            }
            kind => {
                if !self.pairing_allows(conn, kind) {
                    return;
                }
                if let Some(other) = self.other_of(conn) {
                    self.send_text(other, text.to_owned(), out);
                } else if conn.role == Role::Viewer && !conn.pairing && msg.bufferable() {
                    self.buffer(
                        &conn.pc,
                        Pending {
                            epoch: conn.epoch.clone(),
                            kind: kind.as_str().to_owned(),
                            action: msg.action().to_owned(),
                            raw: text.to_owned(),
                        },
                    );
                }
            }
        }
    }

    /// A signalling message the relay drops. A client-log message is counted, and
    /// at most once a minute a debug line says how many were dropped since the last
    /// line. The line carries the count only, never the message.
    fn note_dropped_signal(&mut self, text: &str, now_ms: u64) {
        let is_client_log =
            util::parse_object(text).is_some_and(|m| util::text_of(&m, "type") == "client-log");
        if !is_client_log {
            return;
        }
        self.client_log_dropped += 1;
        let due = self
            .client_log_logged_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= CLIENT_LOG_LINE_EVERY_MS);
        if due {
            let count = std::mem::take(&mut self.client_log_dropped);
            self.client_log_logged_ms = Some(now_ms);
            self.log(
                crate::log::Level::Debug,
                "client-log messages dropped",
                None,
                json!({"class": "client-log.dropped", "count": count}),
            );
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
            Err(Invalid::Close) => self.close(
                id,
                1008,
                reasons::INVALID_PACKET,
                "packet.invalid-media",
                out,
            ),
        }
    }

    /// Text on a file socket: only ping is allowed.
    fn file_text(&mut self, id: ConnId, text: &str, out: &mut Vec<Action>) {
        match validate_file_text(text) {
            Ok(FileText::Ping) => {
                self.send_text(id, json!({"type": "pong"}).to_string(), out);
            }
            Err(_) => self.close(
                id,
                1008,
                reasons::INVALID_PACKET,
                "packet.invalid-file",
                out,
            ),
        }
    }

    /// The host's `admit` (M2 section 2). Each message replaces the PC's
    /// admitted list and remote-pairing flag. The table is persisted first when
    /// either value changes. Then every connected viewer that is no longer listed
    /// and is not in pairing mode closes 1008 with an empty reason. Its data
    /// sockets go through the normal viewer-leave path.
    fn admit_list(&mut self, conn: &Conn, text: &str, out: &mut Vec<Action>) {
        let Some(m) = util::parse_object(text) else {
            return;
        };
        let Some(items) = m.get("viewers").and_then(Value::as_array) else {
            return;
        };
        let admitted: BTreeSet<String> = items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let remote_pairing = util::bool_of(&m, "remotePairing");
        let Some(record) = self.pcs.records.get_mut(&conn.pc) else {
            return;
        };
        if record.admitted != admitted || record.remote_pairing != remote_pairing {
            record.admitted = admitted.clone();
            record.remote_pairing = remote_pairing;
            out.push(Action::PersistPcs(self.pcs.clone()));
        }
        let dropped: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| {
                c.role == Role::Viewer
                    && c.phase == Phase::Live
                    && !c.closing
                    && c.pc == conn.pc
                    && !c.pairing
                    && !admitted.contains(&c.device)
            })
            .map(|(id, _)| *id)
            .collect();
        for viewer in dropped {
            self.close(viewer, 1008, reasons::ACCESS_REMOVED, "admit.removed", out);
            self.forget(viewer, out);
        }
    }

    /// The pairing-mode direction rule (M2 section 3). A pairing-mode viewer
    /// sends only the auth and pair-proof messages to its host. The host sends
    /// only the auth, pair-proof and pair-result messages to a pairing-mode
    /// viewer. Every other message is dropped in that direction.
    fn pairing_allows(&self, conn: &Conn, kind: SignalKind) -> bool {
        match conn.role {
            Role::Viewer => {
                !conn.pairing
                    || matches!(
                        kind,
                        SignalKind::AuthHello
                            | SignalKind::AuthFinish
                            | SignalKind::AuthBind
                            | SignalKind::PairProof
                    )
            }
            Role::Host => match self.other_of(conn) {
                Some(viewer) => {
                    !self.is_pairing(viewer)
                        || matches!(
                            kind,
                            SignalKind::AuthHost
                                | SignalKind::AuthBind
                                | SignalKind::PairProof
                                | SignalKind::PairDone
                                | SignalKind::PairDenied
                        )
                }
                None => true,
            },
            _ => true,
        }
    }

    /// True when the socket is a viewer in pairing mode.
    fn is_pairing(&self, id: ConnId) -> bool {
        self.conns.get(&id).is_some_and(|c| c.pairing)
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
    /// and auth-hello replace older messages of the same kind (same action for
    /// media-control).
    /// The buffer holds at most PENDING_LIMIT entries, oldest dropped first.
    fn buffer(&mut self, pc: &str, pending: Pending) {
        let room = self.rooms.entry(pc.to_owned()).or_default();
        if pending.kind == "media-control"
            || pending.kind == "first-frame"
            || pending.kind == "auth-hello"
        {
            room.pending.retain(|old| {
                !(old.kind == pending.kind
                    && (pending.kind != "media-control" || old.action == pending.action))
            });
        }
        room.pending.push(pending);
        if room.pending.len() > PENDING_LIMIT {
            room.pending.remove(0);
        }
    }
}
