// Cloudflare Workers adapter for the relay core (M3-C1 WebSockets, M3-C2 HTTP).
// Every request is routed to one Relay object (idFromName "relay-v1"). The object
// runs the core relay and the site gate, serves the gate's pages, the admin API,
// healthz and the static assets (Workers Static Assets, through the ASSETS binding),
// and accepts the seven WebSocket routes with the hibernation API.
mod assets;
mod config;
mod http;
mod live;
mod ntfy;
mod object;
mod rng;
mod routes;
mod store;
mod stubs;

use std::cell::RefCell;

use worker::*;

use crate::live::Live;

// Inert classes for the in-place deploy on the JS relay's script. Nothing routes to them.
pub use stubs::{AgentRoom, ApprovalGate, MediaRoom, Room};

const RELAY_NAME: &str = "relay-v1";

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();
    let stub = env
        .durable_object("RELAY")?
        .id_from_name(RELAY_NAME)?
        .get_stub()?;
    stub.fetch_with_request(req).await
}

/// The relay's one Durable Object. `live` holds the core relay and the gate. It
/// is built by the first event after the object starts (see `object::ready`) and
/// kept in memory until eviction. Each socket's state lives in its attachment.
#[durable_object]
pub struct Relay {
    state: State,
    env: Env,
    live: RefCell<Option<Live>>,
}

impl DurableObject for Relay {
    fn new(state: State, env: Env) -> Self {
        console_error_panic_hook::set_once();
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
