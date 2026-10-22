// M3-A scaffold: Cloudflare Workers entry point and a stub Durable Object.
// Every request is routed to one Relay object (idFromName "relay-v1"). The
// object accepts a WebSocket on /ws with the hibernation API; every other
// request gets an empty 404. The relay logic comes in later assignments.
use worker::*;

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

#[durable_object]
pub struct Relay {
    state: State,
    _env: Env,
}

impl DurableObject for Relay {
    fn new(state: State, env: Env) -> Self {
        console_error_panic_hook::set_once();
        Self { state, _env: env }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let is_upgrade = req.headers().get("Upgrade")?.as_deref() == Some("websocket");
        if req.url()?.path() != "/ws" || !is_upgrade {
            return Ok(Response::empty()?.with_status(404));
        }
        let pair = WebSocketPair::new()?;
        // Hibernation API: the runtime keeps the socket while the object is evicted.
        self.state.accept_web_socket(&pair.server);
        Response::from_websocket(pair.client)
    }

    async fn websocket_message(
        &self,
        ws: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        // Stub: every text frame is refused. Binary frames are ignored for now.
        if let WebSocketIncomingMessage::String(_) = message {
            ws.close(Some(1008), Some(""))?;
        }
        Ok(())
    }

    // The workers-rs defaults are unimplemented!() and would panic.
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
        // Fixed text only: the error value can carry peer data.
        worker::console_error!("relay websocket error");
        Ok(())
    }
}
