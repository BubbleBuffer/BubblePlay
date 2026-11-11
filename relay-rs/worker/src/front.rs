//! The front Durable Object (`relay-v1`): the site gate, the HTTP side (static assets, healthz, the
//! admin API, `GET /presence`) and the directory of registered PCs. It holds no session sockets;
//! those live in one object per PC (pc.rs).
//!
//! It also checks the gate for a browser's WebSocket upgrade (the Worker sends only upgrades that
//! carry an Origin here, see lib.rs): it answers 204 when the gate lets the socket through and the
//! Worker then forwards the upgrade to the PC's object. A refused browser socket is accepted and
//! closed 1008 here, as before.
//!
//! The PC objects report to it (`POST /__internal/dir/report`): a PC's record, its admitted
//! count and whether its host is online. The report is the only way a PC enters the directory, and
//! the directory refuses a new PC once it holds `MAX_PCS`.

use std::collections::BTreeMap;
use std::time::Duration;

use bp_relay_core::directory::{CapReached, DirEntry, Directory, PcReport};
use bp_relay_core::gate::{
    Gate, GateEffect, GateSnapshot, Hold, HoldCheck, Request as GateRequest, Verdict,
};
use bp_relay_core::presence::{self, PRESENCE_PATH, PresenceLimiter};
use bp_relay_core::relay::reasons;
use serde_json::{Value, json};
use worker::js_sys;
use worker::wasm_bindgen::JsValue;
use worker::worker_sys::WebSocketRequestResponsePair;
use worker::{
    Delay, Error, ListOptions, Method, Request, RequestInit, Response, Result, ScheduledTime,
    Storage, Url, WebSocket, WebSocketIncomingMessage, WebSocketPair,
};

use crate::assets;
use crate::config::{self, Settings};
use crate::http;
use crate::live::now_ms;
use crate::ntfy;
use crate::rng::WebCrypto;
use crate::routes::{client_ip, route_for_path};
use crate::store;
use crate::{INTERNAL_PREFIX, PC_RELAY_BINDING, WS_CHECK_HEADER};

const GATE_KEY: &str = "gate";
/// One storage key per registered PC: `pc:<canonical id>` holds its `DirEntry` as JSON.
const DIR_PREFIX: &str = "pc:";
/// How often a held navigation looks at the gate's in-memory state.
const HOLD_CHECK_MS: u64 = 250;
const REPORT_PATH: &str = "/__internal/dir/report";

/// The in-memory state: the gate, the presence limiter and the directory.
pub struct FrontLive {
    gate: Gate,
    /// Per-prefix counter for `GET /presence`. Memory only: eviction resets it.
    presence: PresenceLimiter,
    directory: Directory,
    /// The canonical public origin, for the browser-socket check.
    origin: String,
    /// The admin bearer, for the admin API and healthz.
    admin_token: String,
    /// The public origin's host, lowercase, without a port. The healthz Host check uses it.
    public_host: String,
    /// The alarm time last set, if known. The outer None means unknown. Some(None) means none.
    armed: Option<Option<u64>>,
}

impl FrontLive {
    /// The alarm change that matches the gate's earliest deadline, or None when the armed alarm
    /// already matches. Some(None) means delete the alarm.
    fn alarm_change(&mut self) -> Option<Option<u64>> {
        let next = self.gate.next_deadline();
        if self.armed == Some(next) {
            return None;
        }
        self.armed = Some(next);
        Some(next)
    }
}

impl crate::Relay {
    /// Every HTTP request and the gate check of a browser's WebSocket upgrade, with the security
    /// headers added last. The order is native's (app.rs): healthz, the body cap, the gate, then
    /// `GET /presence`, the admin API, a WebSocket route or the static assets.
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
        if path == REPORT_PATH {
            return self.directory_report(req, &method).await;
        }
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
        let ws_check = headers.get(WS_CHECK_HEADER)?.is_some();
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
                let FrontLive {
                    directory,
                    presence: limiter,
                    ..
                } = live;
                let now = now_ms();
                presence::answer_with(
                    limiter,
                    |id| directory.is_online(id, now),
                    client,
                    ids.as_deref(),
                    now,
                )
            };
            return http::gate_response(response);
        }
        if path.starts_with("/__gate/manage/") {
            return self.admin(&method, &path, authorization.as_deref()).await;
        }
        if websocket {
            // The gate let a browser socket through. The Worker forwards it to the PC's object.
            return if ws_check && route_for_path(&path).is_some() {
                http::status(204)
            } else {
                not_found()
            };
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

    /// A PC object's report. Only the objects reach this path: the Worker answers 404 to a request
    /// from outside that names it.
    async fn directory_report(&self, req: Request, method: &Method) -> Result<Response> {
        if *method != Method::Post {
            return not_found();
        }
        let Some(body) = http::read_body(&req).await? else {
            return http::status(413);
        };
        let Ok(report) = serde_json::from_slice::<PcReport>(&body) else {
            return http::status(400);
        };
        if !report.is_valid() {
            return http::status(400);
        }
        let stored = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return server_error();
            };
            live.directory.report(&report, now_ms())
        };
        let entry = match stored {
            Ok(entry) => entry,
            Err(CapReached) => return http::status(409),
        };
        let Ok(text) = serde_json::to_string(&entry) else {
            return http::status(503);
        };
        match self
            .state
            .storage()
            .put(&format!("{DIR_PREFIX}{}", report.id), text)
            .await
        {
            Ok(()) => http::status(204),
            Err(_) => http::status(503),
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
            let rows = {
                let guard = self.live.borrow();
                let Some(live) = guard.as_ref() else {
                    return server_error();
                };
                live.directory.listing(now_ms())
            };
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
            return http::json(&Value::Array(rows));
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

    /// Forgets a PC: its object drops the record and closes its host, then the directory forgets
    /// it. An id that is not a device id, or one the directory does not hold, is 404.
    async fn delete_pc(&self, id: &str) -> Result<Response> {
        let Ok(parsed) = bp_device_key::DeviceId::parse(id) else {
            return not_found();
        };
        let id = parsed.to_string();
        let known = {
            let guard = self.live.borrow();
            let Some(live) = guard.as_ref() else {
                return server_error();
            };
            live.directory.is_registered(&id)
        };
        if !known {
            return not_found();
        }
        // The PC's object answers 404 when it holds no record (a PC that never reported a record
        // there): the directory entry still goes.
        let object = bp_relay_core::directory::session_object_name(Some(&id));
        let stub = self
            .env
            .durable_object(PC_RELAY_BINDING)?
            .id_from_name(&object)?
            .get_stub()?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post);
        let request = Request::new_with_init(
            &format!("https://relay.internal{INTERNAL_PREFIX}pc/delete"),
            &init,
        )?;
        let status = stub.fetch_with_request(request).await?.status_code();
        if status != 204 && status != 404 {
            return http::status(503);
        }
        if let Some(live) = self.live.borrow_mut().as_mut() {
            live.directory.remove(&id);
        }
        match self
            .state
            .storage()
            .delete(&format!("{DIR_PREFIX}{id}"))
            .await
        {
            Ok(_) => http::status(204),
            Err(_) => http::status(503),
        }
    }

    /// Browser sockets the gate refused are accepted and closed at once, so nothing is left to do
    /// for one. The handlers exist because the workers-rs defaults would panic.
    pub(crate) async fn on_message(
        &self,
        _ws: WebSocket,
        _message: WebSocketIncomingMessage,
    ) -> Result<()> {
        Ok(())
    }

    /// The alarm: the gate's expiry. A failed wake makes the platform retry the alarm.
    pub(crate) async fn on_alarm(&self) -> Result<Response> {
        self.ready().await.map_err(Error::RustError)?;
        let now = now_ms();
        let effects = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return server_error();
            };
            // The alarm that fired is gone, so the next re-arm must set one.
            live.armed = None;
            live.gate.tick(now)
        };
        self.run_gate_effects(effects).await;
        self.rearm().await?;
        Response::empty()
    }

    /// Makes sure the in-memory state exists. The first event after the object starts builds it
    /// inside a concurrency block, so no other event runs until it is done.
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
        // Nothing is ever pinged here, but the keepalive text must still be answered by the
        // runtime if a client sends it to a refused socket.
        let keepalive = WebSocketRequestResponsePair::new(crate::PING, crate::PONG)
            .map_err(|_| "the keepalive could not be set".to_owned())?;
        self.state.set_websocket_auto_response(&keepalive);
        let storage = self.state.storage();
        let woken = self
            .state
            .block_concurrency_while(async move { Ok(wake_state(storage, settings).await) })
            .await;
        let live = match woken {
            Ok(Ok(live)) => live,
            Ok(Err(reason)) => {
                worker::console_error!("relay wake failed: {}", reason);
                return Err(reason);
            }
            Err(_) => {
                worker::console_error!("relay wake failed");
                return Err("relay wake failed".to_owned());
            }
        };
        *self.live.borrow_mut() = Some(live);
        Ok(())
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

    /// Makes the alarm match the gate's earliest deadline. Only changes the alarm when it must.
    async fn rearm(&self) -> Result<()> {
        let change = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return Ok(());
            };
            live.alarm_change()
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

/// The in-memory state, built from the config and the stored gate records and PC directory. It
/// owns everything it touches, so it can run inside the concurrency block.
async fn wake_state(
    storage: Storage,
    settings: Settings,
) -> std::result::Result<FrontLive, String> {
    let gate_text = storage
        .get::<String>(GATE_KEY)
        .await
        .map_err(|_| "the gate store could not be read".to_owned())?;
    let gate_snapshot = store::decode_gate(gate_text.as_deref())?;
    let rows = storage
        .list_with_options(ListOptions::new().prefix(DIR_PREFIX))
        .await
        .map_err(|_| "the PC directory could not be read".to_owned())?;
    let mut entries = BTreeMap::new();
    let mut corrupt = false;
    rows.for_each(&mut |value: JsValue, key: JsValue| {
        let id = key
            .as_string()
            .and_then(|key| key.strip_prefix(DIR_PREFIX).map(str::to_owned));
        let entry = value
            .as_string()
            .and_then(|text| serde_json::from_str::<DirEntry>(&text).ok());
        match (id, entry) {
            (Some(id), Some(entry)) => {
                entries.insert(id, entry);
            }
            _ => corrupt = true,
        }
    });
    if corrupt {
        return Err("the stored PC directory is corrupt".to_owned());
    }
    let now = now_ms();
    let origin = settings.core.public_origin.clone();
    let public_host = Url::parse(&origin)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .ok_or_else(|| "PUBLIC_ORIGIN has no host".to_owned())?;
    let mut gate = Gate::new(settings.gate, gate_snapshot, now)
        .map_err(|_| "the gate configuration was rejected".to_owned())?;
    if let Some(hold_ms) = settings.gate_hold_ms {
        gate.set_hold_ms(hold_ms);
    }
    Ok(FrontLive {
        gate,
        presence: PresenceLimiter::new(),
        directory: Directory::new(entries),
        origin,
        admin_token: settings.admin_token,
        public_host,
        armed: None,
    })
}

fn not_found() -> Result<Response> {
    http::status(404)
}

fn server_error() -> Result<Response> {
    http::status(500)
}

/// Accepts a browser socket that the gate refused, and closes it 1008 "not approved".
fn refused_socket(state: &worker::State) -> Result<Response> {
    let pair = WebSocketPair::new()?;
    state.accept_web_socket(&pair.server);
    bp_relay_core::log::emit(
        bp_relay_core::log::Level::Warn,
        now_ms(),
        "relay",
        "socket refused",
        None,
        json!({"class": "gate.refused", "status": 1008, "reason": reasons::NOT_APPROVED}),
    );
    pair.server.close(Some(1008), Some(reasons::NOT_APPROVED))?;
    Response::from_websocket(pair.client)
}
