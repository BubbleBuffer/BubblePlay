//! A PC's Durable Object (`PC_RELAY`, one per host device id): the core relay for one PC, with the
//! signalling, media, control and file sockets of that PC's host and viewers. Every event first
//! makes sure the in-memory state exists (`ready`), runs the core, applies the actions it returns
//! and re-arms the alarm. Sockets use the hibernation API: each socket's snapshot lives in its
//! attachment and its ConnId in a tag, so a socket survives eviction of the object.
//!
//! The object keeps at most one PC record (the host's). It tells the front object's directory
//! about the record, the admitted count and the host's presence (`sync_directory`), because the
//! presence query and the admin listing look across PCs.

use std::collections::{BTreeMap, VecDeque};

use bp_relay_core::directory::{PcReport, REPORT_EVERY_MS};
use bp_relay_core::relay::{
    Action, AdminError, ConnId, ConnSnapshot, Relay as CoreRelay, UpgradeRequest, reasons,
};
use serde_json::json;
use worker::js_sys;
use worker::wasm_bindgen::JsValue;
use worker::worker_sys::WebSocketRequestResponsePair;
use worker::{
    Error, Method, Request, RequestInit, Response, Result, ScheduledTime, Storage, WebSocket,
    WebSocketIncomingMessage, WebSocketPair,
};

use crate::config::{self, Settings};
use crate::http;
use crate::live::{PcLive, Reported, auto_response_ms, now_ms};
use crate::rng::WebCrypto;
use crate::routes::{client_ip, query_values, route_for_path};
use crate::store;
use crate::{INTERNAL_PREFIX, PING, PONG, RELAY_BINDING, RELAY_NAME};

const PCS_KEY: &str = "pcs";
const TAG_PREFIX: &str = "conn:";
const DELETE_PATH: &str = "/__internal/pc/delete";

/// The PC store could not be written (or the directory refused the PC). The batch stops and the
/// origin socket was closed 1011.
struct StoreFailed;

/// Why a persist failed.
enum PersistError {
    /// The directory holds its maximum of PCs and this one is new.
    Cap,
    /// The directory or the object's storage could not be written.
    Store,
}

/// What a wake produces: the live state, and the actions rehydrate returned for it.
struct Woken {
    live: PcLive,
    actions: Vec<Action>,
}

impl crate::PcRelay {
    /// Every request the Worker forwards here: a WebSocket upgrade on one of the seven routes, and
    /// the front object's internal delete. The gate ran in the front object or the Worker.
    pub(crate) async fn on_fetch(&self, req: Request) -> Result<Response> {
        let response = self.route(req).await?;
        http::finish(response)
    }

    async fn route(&self, req: Request) -> Result<Response> {
        if self.ready().await.is_err() {
            return http::status(500);
        }
        let url = req.url()?;
        let path = url.path().to_owned();
        if path == DELETE_PATH {
            return if req.method() == Method::Post {
                self.delete_pc().await
            } else {
                http::status(404)
            };
        }
        let websocket = req
            .headers()
            .get("Upgrade")?
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
        let Some(client) = client_ip(&req) else {
            return http::status(404);
        };
        if !websocket {
            return http::status(404);
        }
        let Some(route) = route_for_path(&path) else {
            return http::status(404);
        };
        let (pc, epoch) = query_values(&url);
        let upgrade = UpgradeRequest {
            route,
            pc,
            epoch,
            // The gate (front object or Worker) lets a WebSocket through only when it carries no
            // Origin or a valid cookie, so the value is true here.
            viewer_authorized: true,
        };
        let accepted = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return http::status(500);
            };
            live.core.upgrade(upgrade, client, now_ms())
        };
        let Ok(id) = accepted else {
            return http::status(404);
        };

        let pair = WebSocketPair::new()?;
        let tag = format!("{TAG_PREFIX}{}", id.0);
        self.state
            .accept_websocket_with_tags(&pair.server, &[tag.as_str()]);
        let snapshot = {
            let guard = self.live.borrow();
            guard.as_ref().and_then(|live| live.core.snapshot_of(id))
        };
        let attached =
            snapshot.is_some_and(|snapshot| pair.server.serialize_attachment(&snapshot).is_ok());
        if attached {
            self.add_socket(id, pair.server.clone());
        } else {
            // The snapshot would not fit the attachment, so the socket cannot hibernate.
            let _ = pair.server.close(Some(1011), Some(""));
            let actions = self.forget(id);
            let _ = self.apply(None, actions).await;
        }
        self.rearm().await?;
        Response::from_websocket(pair.client)
    }

    /// The front object's admin delete: forgets the PC's record and closes its host.
    async fn delete_pc(&self) -> Result<Response> {
        let result = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return http::status(500);
            };
            let Some(id) = live.core.pcs().records.keys().next().cloned() else {
                return http::status(404);
            };
            live.core.delete_pc(&id)
        };
        match result {
            Ok(actions) => match self.apply(None, actions).await {
                Ok(()) => {
                    if let Some(live) = self.live.borrow_mut().as_mut() {
                        live.reported = None;
                        live.report_pending = false;
                    }
                    self.rearm().await?;
                    http::status(204)
                }
                Err(_) => http::status(503),
            },
            Err(AdminError::NotFound) => http::status(404),
            Err(AdminError::Unavailable) => http::status(503),
        }
    }

    pub(crate) async fn on_message(
        &self,
        ws: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        if self.ready().await.is_err() {
            let _ = ws.close(Some(1011), Some(""));
            return Ok(());
        }
        let Some(id) = conn_id_of(&self.state.get_tags(&ws)) else {
            return Ok(());
        };
        let text = matches!(message, WebSocketIncomingMessage::String(_));
        let actions = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return Ok(());
            };
            if !live.sockets.contains_key(&id) {
                return Ok(());
            }
            let now = now_ms();
            match message {
                WebSocketIncomingMessage::String(text) => {
                    live.core.on_text(id, &text, now, &mut WebCrypto)
                }
                WebSocketIncomingMessage::Binary(bytes) => live.core.on_binary(id, &bytes, now),
            }
        };
        // apply closes the origin itself when a write fails, so the result needs no handling.
        let _ = self.apply(Some(id), actions).await;
        if text {
            // Only text can change the record or the host's presence.
            self.sync_directory().await;
        }
        self.rearm().await
    }

    /// A socket closed or errored. Removes it from the relay and tells its peer.
    pub(crate) async fn on_socket_gone(&self, ws: WebSocket) -> Result<()> {
        if self.ready().await.is_err() {
            return Ok(());
        }
        let Some(id) = conn_id_of(&self.state.get_tags(&ws)) else {
            return Ok(());
        };
        let actions = self.forget(id);
        let _ = self.apply(None, actions).await;
        self.sync_directory().await;
        self.rearm().await
    }

    /// The alarm: idle and deadline work, the keepalive activity, the PC store retry and the
    /// repeated presence report. A failed wake makes the platform retry the alarm.
    pub(crate) async fn on_alarm(&self) -> Result<Response> {
        self.ready().await.map_err(Error::RustError)?;
        let now = now_ms();
        let actions = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return http::status(500);
            };
            // The alarm that fired is gone, so the next re-arm must set one.
            live.armed = None;
            let mut actions = Vec::new();
            for (id, ws) in &live.sockets {
                if let Some(at) = auto_response_ms(&self.state, ws) {
                    actions.extend(live.core.note_activity(*id, at));
                }
            }
            actions.extend(live.core.tick(now));
            actions
        };
        if self.apply(None, actions).await.is_err() {
            worker::console_error!("the PC store could not be written on the alarm");
        }
        self.retry_store().await;
        self.sync_directory().await;
        self.rearm().await?;
        Response::empty()
    }

    /// Makes sure the in-memory state exists. The first event after the object starts wakes it
    /// inside a concurrency block, so no other event runs until the wake is done.
    async fn ready(&self) -> std::result::Result<(), String> {
        if self.live.borrow().is_some() {
            return Ok(());
        }
        let settings = match config::load(&self.env) {
            Ok(settings) => settings,
            Err(reason) => {
                worker::console_error!("relay configuration rejected: {}", reason);
                return Err(reason);
            }
        };
        let keepalive = WebSocketRequestResponsePair::new(PING, PONG)
            .map_err(|_| "the keepalive could not be set".to_owned())?;
        self.state.set_websocket_auto_response(&keepalive);
        let (sockets, snapshots, stale) = self.collect_sockets();
        let storage = self.state.storage();
        let woken = self
            .state
            .block_concurrency_while(async move {
                Ok(wake_state(storage, settings, sockets, snapshots).await)
            })
            .await;
        let woken = match woken {
            Ok(Ok(woken)) => woken,
            Ok(Err(reason)) => {
                worker::console_error!("relay wake failed: {}", reason);
                return Err(reason);
            }
            Err(_) => {
                worker::console_error!("relay wake failed");
                return Err("relay wake failed".to_owned());
            }
        };
        for ws in stale {
            bp_relay_core::log::emit(
                bp_relay_core::log::Level::Warn,
                now_ms(),
                "relay",
                "socket refused",
                None,
                json!({
                    "class": "restore.stale-attachment",
                    "status": 1008,
                    "reason": reasons::SESSION_NOT_RESTORED,
                }),
            );
            let _ = ws.close(Some(1008), Some(reasons::SESSION_NOT_RESTORED));
        }
        *self.live.borrow_mut() = Some(woken.live);
        // Rehydrate's actions close the sockets that cannot come back. They write nothing.
        let _ = self.apply(None, woken.actions).await;
        // The directory may hold a stale view of this PC after an eviction.
        self.sync_directory().await;
        Ok(())
    }

    /// Every socket the object still holds. A socket whose attachment is missing, unreadable or
    /// not the snapshot its tag names is stale, and gets closed.
    fn collect_sockets(
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

    fn add_socket(&self, id: ConnId, ws: WebSocket) {
        if let Some(live) = self.live.borrow_mut().as_mut() {
            live.sockets.insert(id, ws);
        }
    }

    /// Runs actions in order. A PersistPcs that cannot be written closes the origin with 1011,
    /// and the rest of the batch does not run.
    async fn apply(
        &self,
        origin: Option<ConnId>,
        actions: Vec<Action>,
    ) -> std::result::Result<(), StoreFailed> {
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
    fn run_sync(&self, actions: Vec<Action>) {
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
    fn run_action(&self, action: Action, queue: &mut VecDeque<Action>) {
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

    fn send(&self, id: ConnId, send: impl FnOnce(&WebSocket) -> Result<()>) {
        let guard = self.live.borrow();
        let Some(ws) = guard.as_ref().and_then(|live| live.sockets.get(&id)) else {
            return;
        };
        if send(ws).is_err() {
            worker::console_error!("relay send failed");
        }
    }

    fn close_conn(&self, id: ConnId, code: u16, reason: &str) {
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
    fn forget(&self, id: ConnId) -> Vec<Action> {
        let mut guard = self.live.borrow_mut();
        let Some(live) = guard.as_mut() else {
            return Vec::new();
        };
        live.sockets.remove(&id);
        live.core.on_close(id)
    }

    fn write_attachment(&self, id: ConnId, snapshot: &ConnSnapshot) -> Result<()> {
        let guard = self.live.borrow();
        match guard.as_ref().and_then(|live| live.sockets.get(&id)) {
            Some(ws) => ws.serialize_attachment(snapshot),
            None => Ok(()),
        }
    }

    /// The report for this object's PC, if it holds a record. `online` is the host's presence now.
    fn current_report(&self) -> Option<PcReport> {
        let guard = self.live.borrow();
        let live = guard.as_ref()?;
        let row = live.core.listing().into_iter().next()?;
        Some(PcReport {
            id: row.id,
            public: row.public,
            first_seen: row.first_seen,
            last_seen: row.last_seen,
            admitted: row.admitted,
            online: row.online,
        })
    }

    /// Posts a report to the front object. The status is the answer's: 204 stored, 409 the
    /// directory is full, 400 or 5xx otherwise.
    async fn post_report(&self, report: &PcReport) -> Result<u16> {
        let body = serde_json::to_string(report).map_err(|e| Error::RustError(e.to_string()))?;
        let stub = self
            .env
            .durable_object(RELAY_BINDING)?
            .id_from_name(RELAY_NAME)?
            .get_stub()?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_body(Some(JsValue::from_str(&body)));
        let request = Request::new_with_init(
            &format!("https://relay.internal{INTERNAL_PREFIX}dir/report"),
            &init,
        )?;
        Ok(stub.fetch_with_request(request).await?.status_code())
    }

    /// Tells the directory about this PC when its record or presence changed since the last
    /// acknowledged report, and repeats the online report every REPORT_EVERY_MS. A failed report
    /// is retried by the alarm.
    async fn sync_directory(&self) {
        let Some(report) = self.current_report() else {
            return;
        };
        let now = now_ms();
        let due = {
            let guard = self.live.borrow();
            let Some(live) = guard.as_ref() else {
                return;
            };
            match live.reported {
                None => true,
                Some(sent) => {
                    sent.online != report.online
                        || (report.online && now.saturating_sub(sent.at_ms) >= REPORT_EVERY_MS)
                }
            }
        };
        if !due {
            return;
        }
        let acknowledged = matches!(self.post_report(&report).await, Ok(204));
        if let Some(live) = self.live.borrow_mut().as_mut() {
            if acknowledged {
                live.reported = Some(Reported {
                    online: report.online,
                    at_ms: now,
                });
                live.report_pending = false;
            } else {
                live.report_pending = true;
            }
        }
    }

    /// Writes the table to this object's storage after the directory accepted it, so the
    /// directory never lags a record the object has promised to keep.
    async fn persist_current_pcs(&self) -> std::result::Result<(), PersistError> {
        let text = {
            let guard = self.live.borrow();
            match guard.as_ref() {
                Some(live) => store::encode_pcs(live.core.pcs()),
                None => Err("the relay is not running".to_owned()),
            }
        }
        .map_err(|_| PersistError::Store)?;
        if let Some(report) = self.current_report() {
            match self.post_report(&report).await {
                Ok(204) => {
                    if let Some(live) = self.live.borrow_mut().as_mut() {
                        live.reported = Some(Reported {
                            online: report.online,
                            at_ms: now_ms(),
                        });
                        live.report_pending = false;
                    }
                }
                Ok(409) => return Err(PersistError::Cap),
                _ => return Err(PersistError::Store),
            }
        }
        self.write_pcs(text).await.map_err(|_| PersistError::Store)
    }

    /// Retries the PC write while the store is unhealthy. The alarm asks for this every second.
    async fn retry_store(&self) {
        let healthy = self
            .live
            .borrow()
            .as_ref()
            .is_none_or(|live| live.pcs_healthy);
        if healthy {
            return;
        }
        if self.persist_current_pcs().await.is_ok() {
            self.set_store_health(true);
        }
    }

    fn set_store_health(&self, healthy: bool) {
        if let Some(live) = self.live.borrow_mut().as_mut() {
            live.pcs_healthy = healthy;
            live.core.set_store_healthy(healthy);
        }
    }

    async fn write_pcs(&self, text: String) -> Result<()> {
        self.state.storage().put(PCS_KEY, text).await
    }

    /// Makes the alarm match the earliest deadline. Only changes the alarm when it must.
    async fn rearm(&self) -> Result<()> {
        let change = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return Ok(());
            };
            live.alarm_change(now_ms())
        };
        let storage = self.state.storage();
        let result = match change {
            Some(Some(at)) => {
                let when = js_sys::Date::new(&JsValue::from_f64(at as f64));
                storage.set_alarm(ScheduledTime::new(when)).await
            }
            Some(None) => storage.delete_alarm().await,
            None => Ok(()),
        };
        if result.is_err() {
            // Forget what was armed, so the next re-arm tries again.
            if let Some(live) = self.live.borrow_mut().as_mut() {
                live.armed = None;
            }
        }
        result
    }
}

/// The in-memory state, built from the config, the stored PC table and the sockets' snapshots. It
/// owns everything it touches, so it can run inside the concurrency block.
async fn wake_state(
    storage: Storage,
    settings: Settings,
    sockets: BTreeMap<ConnId, WebSocket>,
    snapshots: Vec<ConnSnapshot>,
) -> std::result::Result<Woken, String> {
    let pcs_text = storage
        .get::<String>(PCS_KEY)
        .await
        .map_err(|_| "the PC store could not be read".to_owned())?;
    let pcs = store::decode_pcs(pcs_text.as_deref())?;
    let (core, actions) = CoreRelay::rehydrate(settings.core, pcs, snapshots, now_ms());
    Ok(Woken {
        live: PcLive {
            core,
            sockets,
            pcs_healthy: true,
            reported: None,
            report_pending: false,
            armed: None,
        },
        actions,
    })
}

fn conn_id_of(tags: &[String]) -> Option<ConnId> {
    tags.iter()
        .find_map(|tag| tag.strip_prefix(TAG_PREFIX)?.parse::<u64>().ok())
        .map(ConnId)
}

fn push_front(queue: &mut VecDeque<Action>, actions: Vec<Action>) {
    for action in actions.into_iter().rev() {
        queue.push_front(action);
    }
}
