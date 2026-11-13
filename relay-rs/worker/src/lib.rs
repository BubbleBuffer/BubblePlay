// Cloudflare Workers adapter for the relay core (M3-C1 WebSockets, M3-C2 HTTP).
// Two Durable Object classes. The front object (`Relay`, idFromName "relay-v1") runs the site gate,
// serves the gate's pages, the admin API, healthz and the static assets (Workers Static Assets,
// through the ASSETS binding), answers the presence query and keeps the directory of registered
// PCs. Each PC has its own session object (`PcRelay`, idFromName "pc-<device id>") that runs the
// core relay for that PC and accepts its WebSocket routes with the hibernation API, so streams of
// different PCs never share an object, a connection cap or a CPU budget.
mod assets;
mod config;
mod directory;
mod front;
mod front_admin;
mod front_directory;
mod http;
mod live;
mod ntfy;
mod pc;
mod pc_sockets;
mod pc_store;
mod rng;
mod routes;
mod store;
mod stubs;

use std::cell::RefCell;

use worker::*;

use crate::front::FrontLive;
use crate::live::PcLive;
use crate::routes::{query_values, route_for_path};

// Inert classes for the in-place deploy on the JS relay's script. Nothing routes to them.
pub use stubs::{AgentRoom, ApprovalGate, MediaRoom, Room};

const RELAY_NAME: &str = "relay-v1";
const RELAY_BINDING: &str = "RELAY";
const PC_RELAY_BINDING: &str = "PC_RELAY";
/// Paths under this prefix are the objects' own: a request from outside gets a 404.
const INTERNAL_PREFIX: &str = "/__internal/";
/// Set by the Worker on the gate check it sends the front object for a browser's WebSocket upgrade.
const WS_CHECK_HEADER: &str = "X-BP-Ws-Check";
/// Answered by the runtime itself, so a keepalive does not wake the object.
const PING: &str = r#"{"type":"ping"}"#;
const PONG: &str = r#"{"type":"pong"}"#;

/// Relay log lines (structured JSON, one per call) go to the Workers console, where
/// observability collects them. The level picks the console method.
fn console_sink(level: bp_relay_core::log::Level, line: &str) {
    use bp_relay_core::log::Level;
    match level {
        Level::Debug => worker::console_debug!("{}", line),
        Level::Info => worker::console_log!("{}", line),
        Level::Warn => worker::console_warn!("{}", line),
        Level::Error => worker::console_error!("{}", line),
    }
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();
    let url = req.url()?;
    let path = url.path().to_owned();
    if path.starts_with(INTERNAL_PREFIX) {
        return http::finish(http::status(404)?);
    }
    let front = env
        .durable_object(RELAY_BINDING)?
        .id_from_name(RELAY_NAME)?
        .get_stub()?;
    let websocket = req
        .headers()
        .get("Upgrade")?
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    if !(websocket && route_for_path(&path).is_some()) {
        return front.fetch_with_request(req).await;
    }
    // A session socket: the PC's own object takes it. A browser's upgrade (it carries an Origin)
    // passes the site gate in the front object first; a native client has no Origin and needs
    // no gate.
    if req.headers().get("Origin")?.is_some() {
        let mut check = req.clone_mut()?;
        check.headers_mut()?.set(WS_CHECK_HEADER, "1")?;
        let answer = front.fetch_with_request(check).await?;
        if answer.status_code() != 204 {
            return Ok(answer);
        }
    }
    let (pc, _) = query_values(&url);
    let name = bp_relay_core::directory::session_object_name(pc.as_deref());
    env.durable_object(PC_RELAY_BINDING)?
        .id_from_name(&name)?
        .get_stub()?
        .fetch_with_request(req)
        .await
}

/// The front object: the gate, the HTTP side and the directory. `live` is built by the first
/// event after the object starts (see `front::ready`) and kept in memory until eviction.
#[durable_object]
pub struct Relay {
    state: State,
    env: Env,
    live: RefCell<Option<FrontLive>>,
}

impl DurableObject for Relay {
    fn new(state: State, env: Env) -> Self {
        console_error_panic_hook::set_once();
        bp_relay_core::log::set_sink(console_sink);
        Self {
            state,
            env,
            live: RefCell::new(None),
        }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        self.on_fetch(req).await
    }

    async fn alarm(&self) -> Result<Response> {
        self.on_alarm().await
    }

    // Browser sockets the gate refused are closed at once. The workers-rs defaults would panic.
    async fn websocket_message(
        &self,
        ws: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        self.on_message(ws, message).await
    }

    async fn websocket_close(
        &self,
        _ws: WebSocket,
        _code: usize,
        _reason: String,
        _was_clean: bool,
    ) -> Result<()> {
        Ok(())
    }

    async fn websocket_error(&self, _ws: WebSocket, _error: Error) -> Result<()> {
        Ok(())
    }
}

/// One PC's session object. `live` holds the core relay for that PC and its sockets. Each
/// socket's state lives in its attachment.
#[durable_object]
pub struct PcRelay {
    state: State,
    env: Env,
    live: RefCell<Option<PcLive>>,
}

impl DurableObject for PcRelay {
    fn new(state: State, env: Env) -> Self {
        console_error_panic_hook::set_once();
        bp_relay_core::log::set_sink(console_sink);
        Self {
            state,
            env,
            live: RefCell::new(None),
        }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        self.on_fetch(req).await
    }

    async fn alarm(&self) -> Result<Response> {
        self.on_alarm().await
    }

    async fn websocket_message(
        &self,
        ws: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        self.on_message(ws, message).await
    }

    // The workers-rs defaults are unimplemented!() and would panic.
    async fn websocket_close(
        &self,
        ws: WebSocket,
        _code: usize,
        _reason: String,
        _was_clean: bool,
    ) -> Result<()> {
        self.on_socket_gone(ws).await
    }

    async fn websocket_error(&self, ws: WebSocket, _error: Error) -> Result<()> {
        // Fixed text only: the error value can carry peer data.
        worker::console_error!("relay websocket error");
        self.on_socket_gone(ws).await
    }
}
