//! The Durable Object's event handling. Every event first makes sure the in-memory state exists
//! (`ready`), runs the core or the gate, applies the actions they return, and re-arms the alarm.
//! Sockets use the hibernation API: each socket's snapshot lives in its attachment and its
//! ConnId in a tag, so a socket survives eviction of the object.

use std::collections::{BTreeMap, VecDeque};
use std::net::IpAddr;
use std::time::Duration;

use bp_relay_core::gate::{
    Gate, GateEffect, GateSnapshot, Hold, HoldCheck, Request as GateRequest, Verdict,
};
use bp_relay_core::presence::{self, PRESENCE_PATH, PresenceLimiter};
use bp_relay_core::relay::{
    Action, AdminError, ConnId, ConnSnapshot, Relay as CoreRelay, UpgradeRequest,
};
use serde_json::{Value, json};
use worker::js_sys;
use worker::wasm_bindgen::JsValue;
use worker::worker_sys::WebSocketRequestResponsePair;
use worker::{
    Delay, Error, Method, Request, Response, Result, ScheduledTime, State, Storage, Url, WebSocket,
    WebSocketIncomingMessage, WebSocketPair,
};

use crate::assets;
use crate::config::{self, Settings};
use crate::http;
use crate::live::{Live, auto_response_ms, now_ms};
use crate::ntfy;
use crate::rng::WebCrypto;
use crate::routes::{client_ip, query_values, route_for_path};
use crate::store;

const PCS_KEY: &str = "pcs";
const GATE_KEY: &str = "gate";
const TAG_PREFIX: &str = "conn:";
/// Answered by the runtime itself, so a keepalive does not wake the object.
const PING: &str = r#"{"type":"ping"}"#;
const PONG: &str = r#"{"type":"pong"}"#;
/// How often a held navigation looks at the gate's in-memory state.
const HOLD_CHECK_MS: u64 = 250;

/// The PC store could not be written. The batch stops and the origin socket was closed 1011.
struct StoreFailed;

/// What a wake produces: the live state, and the actions rehydrate returned for it.
struct Woken {
    live: Live,
    actions: Vec<Action>,
}

impl crate::Relay {
    /// Every HTTP request and WebSocket upgrade, with the security headers added last. The order
    /// is native's (app.rs): healthz, the body cap, the gate, then `GET /presence`, the admin API,
    /// a WebSocket route or the static assets.
    pub(crate) async fn on_fetch(&self, req: Request) -> Result<Response> {
        let response = self.route(req).await?;
        http::finish(response)
    }

    async fn route(&self, req: Request) -> Result<Response> {
        if self.ready().await.is_err() {
            return server_error();
        }
        let url = req.url()?;
        let path = url.path().to_owned();
        let method = req.method();
        let method_text = method.to_string();
        let headers = req.headers();
        let host = headers.get("Host")?;
        let authorization = headers.get("Authorization")?;

        if path == "/healthz" && (method == Method::Get || method == Method::Head) {
            let allowed = {
                let guard = self.live.borrow();
                let Some(live) = guard.as_ref() else {
                    return server_error();
                };
                http::host_allowed(host.as_deref(), &live.public_host)
                    && http::bearer_matches(authorization.as_deref(), &live.admin_token)
            };
            return if allowed {
                http::healthz(method == Method::Head)
            } else {
                not_found()
            };
        }

        let Some(body) = http::read_body(&req).await? else {
            return http::status(413);
        };
        let Some(client) = client_ip(&req) else {
            return not_found();
        };
        let origin = headers.get("Origin")?;
        let cookie = headers.get("Cookie")?;
        let sec_fetch_dest = headers.get("Sec-Fetch-Dest")?;
        let websocket = headers
            .get("Upgrade")?
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
        let gate_request = GateRequest {
            method: &method_text,
            path: &path,
            query: url.query(),
            websocket,
            origin: origin.as_deref(),
            cookie: cookie.as_deref(),
            authorization: authorization.as_deref(),
            sec_fetch_dest: sec_fetch_dest.as_deref(),
            body: &body,
            client_ip: client,
            now_ms: now_ms(),
        };
        let (verdict, public_origin) = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return server_error();
            };
            (
                live.gate.handle(&mut WebCrypto, &gate_request),
                live.origin.clone(),
            )
        };
        match verdict {
            Verdict::Refuse => {
                // A browser socket that names the public origin is accepted and closed 1008, as
                // native does. Any other refusal is a 404.
                let browser_socket = websocket
                    && origin.as_deref() == Some(public_origin.as_str())
                    && route_for_path(&path).is_some();
                return if browser_socket {
                    refused_socket(&self.state)
                } else {
                    not_found()
                };
            }
            Verdict::Handled(response, effects) => {
                self.run_gate_effects(effects).await;
                return http::gate_response(response);
            }
            Verdict::Hold(hold, effects) => {
                self.run_gate_effects(effects).await;
                return self.hold_navigation(hold).await;
            }
            Verdict::Drop => return http::dropped(),
            Verdict::Allow => {}
        }
        // The gate lets a GET on /presence through before its navigation step, cookie or not.
        if path == PRESENCE_PATH && method == Method::Get && !websocket {
            let ids = url
                .query_pairs()
                .find(|(key, _)| key == "ids")
                .map(|(_, value)| value.into_owned());
            let response = {
                let mut guard = self.live.borrow_mut();
                let Some(live) = guard.as_mut() else {
                    return server_error();
                };
                let Live {
                    core,
                    presence: limiter,
                    ..
                } = live;
                presence::answer(limiter, core, client, ids.as_deref(), now_ms())
            };
            return http::gate_response(response);
        }
        if path.starts_with("/__gate/manage/") {
            return self.admin(&method, &path, authorization.as_deref()).await;
        }
        if websocket {
            return self.accept_socket(&path, &url, client).await;
        }
        match assets::plan(&method_text, &path) {
            assets::Plan::Status(code) => http::status(code),
            assets::Plan::Fetch {
                path: asset_path,
                index,
            } => {
                let head = method == Method::Head;
                assets::serve(&self.env, &url, head, &asset_path, index).await
            }
        }
    }

    /// Holds an unapproved navigation open until the gate admits it (a 302 with the cookie) or
    /// drops it. Each check reads in-memory state only, and the timer between checks leaves the
    /// object free to handle other events, the decision among them.
    async fn hold_navigation(&self, hold: Hold) -> Result<Response> {
        loop {
            let now = now_ms();
            let check = {
                let mut guard = self.live.borrow_mut();
                match guard.as_mut() {
                    Some(live) => live.gate.check_hold(&mut WebCrypto, &hold, now),
                    None => HoldCheck::Drop,
                }
            };
            match check {
                HoldCheck::Waiting => {
                    let left = hold.until_ms().saturating_sub(now).clamp(1, HOLD_CHECK_MS);
                    Delay::from(Duration::from_millis(left)).await;
                }
                HoldCheck::Admit(response, effects) => {
                    self.run_gate_effects(effects).await;
                    return http::gate_response(response);
                }
                HoldCheck::Drop => return http::dropped(),
            }
        }
    }

    /// Accepts a WebSocket upgrade on one of the seven routes. The core decides whether the
    /// route takes it, and the socket hibernates with its snapshot as the attachment.
    async fn accept_socket(&self, path: &str, url: &Url, client: IpAddr) -> Result<Response> {
        let Some(route) = route_for_path(path) else {
            return not_found();
        };
        let (pc, epoch) = query_values(url);
        let upgrade = UpgradeRequest {
            route,
            pc,
            epoch,
            // Native sets this to `origin.is_none() || cookie_valid`. The gate returns Allow for a
            // WebSocket only when one of those holds, so the value is true here.
            viewer_authorized: true,
        };
        let accepted = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return server_error();
            };
            live.core.upgrade(upgrade, client, now_ms())
        };
        let Ok(id) = accepted else {
            return not_found();
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

    /// The admin API, behind the admin bearer. A wrong bearer, route or id is 404, as native.
    async fn admin(
        &self,
        method: &Method,
        path: &str,
        authorization: Option<&str>,
    ) -> Result<Response> {
        let allowed = {
            let guard = self.live.borrow();
            let Some(live) = guard.as_ref() else {
                return server_error();
            };
            http::bearer_matches(authorization, &live.admin_token)
        };
        if !allowed {
            return not_found();
        }
        if *method == Method::Get && path == "/__gate/manage/pcs" {
            let listing = {
                let guard = self.live.borrow();
                let Some(live) = guard.as_ref() else {
                    return server_error();
                };
                live.core.list_pcs()
            };
            return match listing {
                Ok(rows) => {
                    let rows: Vec<Value> = rows
                        .into_iter()
                        .map(|row| {
                            json!({
                                "id": row.id,
                                "public": row.public,
                                "first_seen": row.first_seen,
                                "last_seen": row.last_seen,
                                "online": row.online,
                                "admitted": row.admitted,
                            })
                        })
                        .collect();
                    http::json(&Value::Array(rows))
                }
                Err(AdminError::Unavailable) => http::status(503),
                Err(AdminError::NotFound) => not_found(),
            };
        }
        if let Some(id) = path.strip_prefix("/__gate/manage/pcs/")
            && *method == Method::Delete
            && !id.is_empty()
            && !id.contains('/')
        {
            return self.delete_pc(id).await;
        }
        not_found()
    }

    async fn delete_pc(&self, id: &str) -> Result<Response> {
        let result = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return server_error();
            };
            live.core.delete_pc(id)
        };
        match result {
            Ok(actions) => match self.apply(None, actions).await {
                Ok(()) => {
                    self.rearm().await?;
                    http::status(204)
                }
                Err(_) => http::status(503),
            },
            Err(AdminError::NotFound) => not_found(),
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
        self.rearm().await
    }

    /// The alarm: idle and deadline work, the keepalive activity, the gate's expiry, and the PC
    /// store retry. A failed wake makes the platform retry the alarm.
    pub(crate) async fn on_alarm(&self) -> Result<Response> {
        self.ready().await.map_err(Error::RustError)?;
        let now = now_ms();
        let (actions, effects) = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return server_error();
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
            let effects = live.gate.tick(now);
            (actions, effects)
        };
        if self.apply(None, actions).await.is_err() {
            worker::console_error!("the PC store could not be written on the alarm");
        }
        self.run_gate_effects(effects).await;
        self.retry_store().await;
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
            let _ = ws.close(Some(1008), Some(""));
        }
        *self.live.borrow_mut() = Some(woken.live);
        // Rehydrate's actions close the sockets that cannot come back. They write nothing.
        let _ = self.apply(None, woken.actions).await;
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
                    if !self.persist_current_pcs().await {
                        self.set_store_health(false);
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

    fn encode_current_pcs(&self) -> std::result::Result<String, String> {
        let guard = self.live.borrow();
        match guard.as_ref() {
            Some(live) => store::encode_pcs(live.core.pcs()),
            None => Err("the relay is not running".to_owned()),
        }
    }

    async fn persist_current_pcs(&self) -> bool {
        let Ok(text) = self.encode_current_pcs() else {
            return false;
        };
        self.write_pcs(text).await.is_ok()
    }

    /// Retries the PC write while the store is unhealthy. The alarm asks for this every second.
    async fn retry_store(&self) {
        let text = {
            let guard = self.live.borrow();
            match guard.as_ref() {
                Some(live) if !live.pcs_healthy => store::encode_pcs(live.core.pcs()),
                _ => return,
            }
        };
        let Ok(text) = text else {
            return;
        };
        if self.write_pcs(text).await.is_ok() {
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

    async fn write_gate(&self, snapshot: &GateSnapshot) -> Result<()> {
        let text = store::encode_gate(snapshot).map_err(Error::RustError)?;
        self.state.storage().put(GATE_KEY, text).await
    }

    /// Effects from the gate. A failed gate write is logged and the cookie records stay in
    /// memory until the next write.
    async fn run_gate_effects(&self, effects: Vec<GateEffect>) {
        for effect in effects {
            match effect {
                // The approval request goes to ntfy without holding up the response.
                GateEffect::NtfyPublish { url, headers, body } => {
                    self.state.wait_until(ntfy::publish(url, headers, body));
                }
                GateEffect::PersistGate(snapshot) => {
                    if self.write_gate(&snapshot).await.is_err() {
                        worker::console_error!("the gate records could not be written");
                    }
                }
            }
        }
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

/// The in-memory state, built from the config, the stored PCs and gate, and the sockets'
/// snapshots. It owns everything it touches, so it can run inside the concurrency block.
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
    let gate_text = storage
        .get::<String>(GATE_KEY)
        .await
        .map_err(|_| "the gate store could not be read".to_owned())?;
    let pcs = store::decode_pcs(pcs_text.as_deref())?;
    let gate_snapshot = store::decode_gate(gate_text.as_deref())?;
    let now = now_ms();
    let origin = settings.core.public_origin.clone();
    let public_host = Url::parse(&origin)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .ok_or_else(|| "PUBLIC_ORIGIN has no host".to_owned())?;
    let admin_token = settings.admin_token;
    let mut gate = Gate::new(settings.gate, gate_snapshot, now)
        .map_err(|_| "the gate configuration was rejected".to_owned())?;
    if let Some(hold_ms) = settings.gate_hold_ms {
        gate.set_hold_ms(hold_ms);
    }
    let (core, actions) = CoreRelay::rehydrate(settings.core, pcs, snapshots, now);
    Ok(Woken {
        live: Live {
            core,
            gate,
            presence: PresenceLimiter::new(),
            sockets,
            origin,
            pcs_healthy: true,
            admin_token,
            public_host,
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

fn not_found() -> Result<Response> {
    Ok(Response::empty()?.with_status(404))
}

fn server_error() -> Result<Response> {
    Ok(Response::empty()?.with_status(500))
}

/// Accepts a browser socket that the gate refused, and closes it 1008 with an empty reason.
fn refused_socket(state: &State) -> Result<Response> {
    let pair = WebSocketPair::new()?;
    state.accept_web_socket(&pair.server);
    pair.server.close(Some(1008), Some(""))?;
    Response::from_websocket(pair.client)
}
