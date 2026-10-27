//! Inert Durable Object classes that keep the four classes of the JS relay's live script
//! (`playfull-cat`) in place during an in-place deploy of this Worker. Cloudflare needs every
//! class the script has to stay declared in the migration history, and the stored data of those
//! classes is only kept if the names and tags match. Nothing in the relay uses these classes.
//!
//! Each class is inert:
//! - `fetch` returns 404 with an empty body. A WebSocket upgrade is a fetch too, so it gets 404
//!   and no socket is accepted. This is a doc claim: no test binds a stub, so none calls its fetch.
//! - `alarm` does nothing and returns an empty response.
//! - The WebSocket handlers do nothing. No socket is ever accepted, so they are never called. They
//!   are overridden because the workers-rs 0.8.7 defaults are `unimplemented!()` and would panic.
//! - Nothing reads or writes storage. `State` and `Env` are kept only because `new` takes them.
//!
//! No route reaches them. The local `wrangler.toml` binds only RELAY, and
//! `wrangler.deploy.toml.example` binds these classes only in its commented in-place block. The
//! `m3` node tests in `tests/relay.test.mjs` check that local config and that stub-named paths
//! keep the relay's own 404.

use worker::*;

fn refuse() -> Result<Response> {
    Ok(Response::empty()?.with_status(404))
}

macro_rules! inert_durable_object {
    ($($name:ident),+ $(,)?) => {
        $(
            #[durable_object]
            pub struct $name {
                _state: State,
                _env: Env,
            }

            impl DurableObject for $name {
                fn new(state: State, env: Env) -> Self {
                    Self {
                        _state: state,
                        _env: env,
                    }
                }

                async fn fetch(&self, _req: Request) -> Result<Response> {
                    refuse()
                }

                async fn alarm(&self) -> Result<Response> {
                    Response::empty()
                }

                async fn websocket_message(
                    &self,
                    _ws: WebSocket,
                    _message: WebSocketIncomingMessage,
                ) -> Result<()> {
                    Ok(())
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
        )+
    };
}

// These names must match the classes the live script already has. Do not rename them.
inert_durable_object!(Room, MediaRoom, ApprovalGate, AgentRoom);
