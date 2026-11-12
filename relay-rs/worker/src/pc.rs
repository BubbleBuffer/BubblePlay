//! A PC's Durable Object (`PC_RELAY`, one per host device id): the core relay for one PC, with the
//! signalling, media, control and file sockets of that PC's host and viewers. Every event first
//! makes sure the in-memory state exists (`ready`), runs the core, applies the actions it returns
//! and re-arms the alarm. Sockets use the hibernation API: each socket's snapshot lives in its
//! attachment and its ConnId in a tag, so a socket survives eviction of the object.
//!
//! The object keeps at most one PC record (the host's). It tells the front object's directory
//! about the record, the admitted count and the host's presence (`sync_directory`), because the
//! presence query and the admin listing look across PCs.

use std::collections::BTreeMap;

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
pub(crate) struct StoreFailed;

/// Why a persist failed.
pub(crate) enum PersistError {
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
        self.claim_legacy(pc.as_deref()).await;
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
        // A batch of plain sends (every forwarded media frame) changes no deadline that is not
        // later than the armed alarm: activity only moves an idle deadline forward. So it needs no
        // re-arm, and an alarm that fires early only re-arms itself.
        let sends_only = actions
            .iter()
            .all(|action| matches!(action, Action::SendText(..) | Action::SendBinary(..)));
        // apply closes the origin itself when a write fails, so the result needs no handling.
        let _ = self.apply(Some(id), actions).await;
        if text {
            // Only text can change the record or the host's presence.
            self.sync_directory().await;
        }
        if text || !sends_only {
            self.rearm().await?;
        }
        Ok(())
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

    /// Once per object start: when this object holds no PC and the upgrade names a device id, asks
    /// the front object for that PC record from the single-object relay table, adopts it and
    /// saves it. The directory report that follows makes the front object drop it from the old
    /// table, so an existing PC keeps working after the split without registering again. The
    /// record is saved here before the old copy goes.
    async fn claim_legacy(&self, pc: Option<&str>) {
        let name = bp_relay_core::directory::session_object_name(pc);
        let Some(id) = name.strip_prefix("pc-") else {
            return;
        };
        let wanted = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return;
            };
            let wanted = !live.legacy_checked && live.core.pcs().is_empty();
            live.legacy_checked = true;
            wanted
        };
        if !wanted {
            return;
        }
        let Some(text) = self.fetch_legacy(id).await else {
            return;
        };
        let Ok(table) = store::decode_pcs(Some(&text)) else {
            worker::console_error!("a legacy PC record could not be read");
            return;
        };
        let saved = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return;
            };
            live.core.adopt_pcs(table);
            store::encode_pcs(live.core.pcs())
        };
        let written = match saved {
            Ok(text) => self.write_pcs(text).await.is_ok(),
            Err(_) => false,
        };
        if written {
            self.sync_directory().await;
        } else {
            self.set_store_health(false);
        }
    }

    /// The old record of one PC from the front object: its table text, or None (not there, or the
    /// front object did not answer 200).
    async fn fetch_legacy(&self, id: &str) -> Option<String> {
        let stub = self
            .env
            .durable_object(RELAY_BINDING)
            .ok()?
            .id_from_name(RELAY_NAME)
            .ok()?
            .get_stub()
            .ok()?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_body(Some(JsValue::from_str(id)));
        let request = Request::new_with_init(
            &format!("https://relay.internal{INTERNAL_PREFIX}dir/claim"),
            &init,
        )
        .ok()?;
        let mut response = stub.fetch_with_request(request).await.ok()?;
        if response.status_code() != 200 {
            return None;
        }
        response.text().await.ok()
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
    pub(crate) async fn persist_current_pcs(&self) -> std::result::Result<(), PersistError> {
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

    pub(crate) fn set_store_health(&self, healthy: bool) {
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
            legacy_checked: false,
            armed: None,
        },
        actions,
    })
}

pub(crate) fn conn_id_of(tags: &[String]) -> Option<ConnId> {
    tags.iter()
        .find_map(|tag| tag.strip_prefix(TAG_PREFIX)?.parse::<u64>().ok())
        .map(ConnId)
}
