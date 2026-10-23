//! The approval request to ntfy. The core returns the URL, the headers and the JSON body; this
//! posts them with a 10 s deadline, as the native runtime does. A failure is logged with fixed
//! text only: the body carries the approve and deny bearers and the headers carry the ntfy token,
//! so neither is ever printed.

use worker::js_sys::{self, Function, Reflect, Uint8Array};
use worker::wasm_bindgen::closure::Closure;
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::{AbortController, Fetch, Headers, Method, Request, RequestInit, Result};

/// The same deadline the native server uses.
const TIMEOUT_MS: f64 = 10_000.0;

/// Posts one approval request. Meant for `State::wait_until`, so it never blocks the response.
pub async fn publish(url: String, headers: Vec<(String, String)>, body: Vec<u8>) {
    match post(&url, &headers, body).await {
        Ok(status) if (200..300).contains(&status) => {}
        Ok(status) => worker::console_error!("ntfy publish was refused: status {}", status),
        Err(_) => worker::console_error!("ntfy publish failed"),
    }
}

async fn post(url: &str, headers: &[(String, String)], body: Vec<u8>) -> Result<u16> {
    let controller = AbortController::default();
    let signal = controller.signal();
    let deadline = Deadline::start(controller)?;
    let mut header_map = Headers::new();
    for (name, value) in headers {
        header_map.set(name, value)?;
    }
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(header_map)
        .with_body(Some(Uint8Array::from(body.as_slice()).into()));
    let sent = match Request::new_with_init(url, &init) {
        Ok(request) => Fetch::Request(request).send_with_signal(&signal).await,
        Err(error) => Err(error),
    };
    deadline.clear();
    Ok(sent?.status_code())
}

/// A `setTimeout` that aborts the request when it fires. `clear` cancels it once the request has
/// returned, so no timer outlives the publish.
struct Deadline(JsValue);

impl Deadline {
    fn start(controller: AbortController) -> Result<Self> {
        let global = js_sys::global();
        let abort = Closure::once_into_js(move || controller.abort());
        let set_timeout: Function =
            Reflect::get(&global, &JsValue::from_str("setTimeout"))?.dyn_into()?;
        let id = set_timeout.call2(&global, &abort, &JsValue::from_f64(TIMEOUT_MS))?;
        Ok(Self(id))
    }

    fn clear(self) {
        let global = js_sys::global();
        let clear_timeout = Reflect::get(&global, &JsValue::from_str("clearTimeout"))
            .and_then(|value| value.dyn_into::<Function>());
        if let Ok(clear_timeout) = clear_timeout {
            let _ = clear_timeout.call1(&global, &self.0);
        }
    }
}
