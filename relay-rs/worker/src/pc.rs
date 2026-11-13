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

use bp_relay_core::relay::{
    Action, AdminError, ConnId, ConnSnapshot, Relay as CoreRelay, UpgradeRequest, Upgraded, reasons,
};
use serde_json::json;
use worker::js_sys;
use worker::wasm_bindgen::JsValue;
use worker::worker_sys::WebSocketRequestResponsePair;
use worker::{
    Error, Method, Request, Response, Result, ScheduledTime, Storage, WebSocket,
    WebSocketIncomingMessage, WebSocketPair,
};

use crate::config::{self, Settings};
use crate::http;
use crate::live::{PcLive, auto_response_ms, now_ms};
use crate::pc_store::PCS_KEY;
use crate::rng::WebCrypto;
use crate::routes::{client_ip, query_values, route_for_path};
use crate::store;
use crate::{PING, PONG};

const TAG_PREFIX: &str = "conn:";
const DELETE_PATH: &str = "/__internal/pc/delete";

/// The PC store could not be written (or the directory refused the PC). The batch stops and the
/// origin socket was closed 1011.
pub(crate) struct StoreFailed;

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
        let Ok(Upgraded { id, evicted }) = accepted else {
            return http::status(404);
        };
        self.run_sync(evicted);

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
    let pcs_stored = !pcs.records.is_empty();
    let (core, actions) = CoreRelay::rehydrate(settings.core, pcs, snapshots, now_ms());
    Ok(Woken {
        live: PcLive {
            core,
            sockets,
            pcs_healthy: true,
            reported: None,
            report_pending: false,
            report_failures: 0,
            stored: pcs_stored,
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
