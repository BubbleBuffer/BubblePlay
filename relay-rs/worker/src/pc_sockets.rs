//! The sockets of a PC object: collecting them after a wake, applying the core's actions to them
//! and writing their attachments.

use std::collections::{BTreeMap, VecDeque};

use bp_relay_core::relay::{Action, ConnId, ConnSnapshot};
use worker::{Result, WebSocket};

use crate::pc::{PersistError, StoreFailed, conn_id_of};

impl crate::PcRelay {
    /// Every socket the object still holds. A socket whose attachment is missing, unreadable or
    /// not the snapshot its tag names is stale, and gets closed.
    pub(crate) fn collect_sockets(
        &self,
    ) -> (
        BTreeMap<ConnId, WebSocket>,
        Vec<ConnSnapshot>,
        Vec<WebSocket>,
    ) {
        let mut sockets = BTreeMap::new();
        let mut snapshots = Vec::new();
        let mut stale = Vec::new();
        for ws in self.state.get_websockets() {
            let tagged = conn_id_of(&self.state.get_tags(&ws));
            match (ws.deserialize_attachment::<ConnSnapshot>(), tagged) {
                (Ok(Some(snapshot)), Some(id)) if snapshot.id == id => {
                    sockets.insert(id, ws);
                    snapshots.push(snapshot);
                }
                _ => stale.push(ws),
            }
        }
        (sockets, snapshots, stale)
    }

    pub(crate) fn add_socket(&self, id: ConnId, ws: WebSocket) {
        if let Some(live) = self.live.borrow_mut().as_mut() {
            live.sockets.insert(id, ws);
        }
    }

    /// Runs actions in order. A PersistPcs that cannot be written closes the origin with 1011,
    /// and the rest of the batch does not run.
    pub(crate) async fn apply(
        &self,
        origin: Option<ConnId>,
        actions: Vec<Action>,
    ) -> std::result::Result<(), StoreFailed> {
        // The common batch is one forwarded frame: send it without building a queue.
        if actions
            .iter()
            .all(|action| matches!(action, Action::SendText(..) | Action::SendBinary(..)))
        {
            for action in actions {
                match action {
                    Action::SendText(id, text) => self.send(id, |ws| ws.send_with_str(&text)),
                    Action::SendBinary(id, bytes) => self.send(id, |ws| ws.send_with_bytes(&bytes)),
                    _ => {}
                }
            }
            return Ok(());
        }
        let mut queue: VecDeque<Action> = actions.into();
        while let Some(action) = queue.pop_front() {
            match action {
                Action::PersistPcs(_) => {
                    if let Err(error) = self.persist_current_pcs().await {
                        // A full directory refuses this PC; the store itself is fine.
                        if matches!(error, PersistError::Store) {
                            self.set_store_health(false);
                        }
                        if let Some(id) = origin {
                            self.close_conn(id, 1011, "");
                            let nested = self.forget(id);
                            self.run_sync(nested);
                        }
                        return Err(StoreFailed);
                    }
                }
                other => self.run_action(other, &mut queue),
            }
        }
        Ok(())
    }

    /// Runs actions on a close path, where a PC write is not allowed. The native server does
    /// the same: it logs and skips a PersistPcs there, and the next persist writes the table.
    pub(crate) fn run_sync(&self, actions: Vec<Action>) {
        let mut queue: VecDeque<Action> = actions.into();
        while let Some(action) = queue.pop_front() {
            self.run_action(action, &mut queue);
        }
    }

    /// Runs one non-PC action. Close pushes the forgotten socket's own actions to the front of
    /// the queue, so they run before the rest of the batch, as native does.
    ///
    /// The native 1013 "slow peer" close does not exist here: Workers give no view of a socket's
    /// send buffer, so a slow peer is never closed for backpressure.
    pub(crate) fn run_action(&self, action: Action, queue: &mut VecDeque<Action>) {
        match action {
            Action::PersistPcs(_) => {
                worker::console_error!("skipped a PersistPcs on a close path");
            }
            Action::Snapshot(id, snapshot) => {
                if self.write_attachment(id, &snapshot).is_err() {
                    queue.push_front(Action::Close(id, 1011, String::new()));
                }
            }
            Action::SendText(id, text) => self.send(id, |ws| ws.send_with_str(&text)),
            Action::SendBinary(id, bytes) => self.send(id, |ws| ws.send_with_bytes(&bytes)),
            Action::Close(id, code, reason) => {
                self.close_conn(id, code, &reason);
                let nested = self.forget(id);
                push_front(queue, nested);
            }
        }
    }

    pub(crate) fn send(&self, id: ConnId, send: impl FnOnce(&WebSocket) -> Result<()>) {
        let guard = self.live.borrow();
        let Some(ws) = guard.as_ref().and_then(|live| live.sockets.get(&id)) else {
            return;
        };
        if send(ws).is_err() {
            worker::console_error!("relay send failed");
        }
    }

    pub(crate) fn close_conn(&self, id: ConnId, code: u16, reason: &str) {
        let guard = self.live.borrow();
        let Some(ws) = guard.as_ref().and_then(|live| live.sockets.get(&id)) else {
            return;
        };
        // A closed socket stays in getWebSockets() until its peer answers the close, which a dead
        // peer never does. Drop its snapshot first, so a wake finds it stale and never restores
        // it in place of the socket that replaced it.
        if ws
            .serialize_attachment(Option::<ConnSnapshot>::None)
            .is_err()
        {
            worker::console_error!("relay could not clear a closed socket's attachment");
        }
        if ws.close(Some(code), Some(reason)).is_err() {
            worker::console_error!("relay close failed");
        }
    }

    /// Removes a socket from the live map and tells the core it is gone. Idempotent: the core
    /// ignores an id it no longer knows, so a second close event returns nothing.
    pub(crate) fn forget(&self, id: ConnId) -> Vec<Action> {
        let mut guard = self.live.borrow_mut();
        let Some(live) = guard.as_mut() else {
            return Vec::new();
        };
        live.sockets.remove(&id);
        live.core.on_close(id)
    }

    pub(crate) fn write_attachment(&self, id: ConnId, snapshot: &ConnSnapshot) -> Result<()> {
        let guard = self.live.borrow();
        match guard.as_ref().and_then(|live| live.sockets.get(&id)) {
            Some(ws) => ws.serialize_attachment(snapshot),
            None => Ok(()),
        }
    }
}

fn push_front(queue: &mut VecDeque<Action>, actions: Vec<Action>) {
    for action in actions.into_iter().rev() {
        queue.push_front(action);
    }
}
