//! HTTP helpers for the Durable Object's router (object.rs). Each one follows a rule of the native
//! server's app.rs: the 16 KiB body cap, the healthz host and bearer rules, the constant-time
//! bearer check, the gate's response mapping, the JSON admin responses and the security headers.

use bp_relay_core::gate::Response as GateResponse;
use worker::js_sys::{Reflect, Uint8Array};
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::wasm_bindgen_futures::JsFuture;
use worker::web_sys::ReadableStreamDefaultReader;
use worker::{Request, Response, Result};

/// The largest body the router reads. A larger one is 413 and never reaches the gate.
pub const BODY_LIMIT: usize = 16 * 1024;

/// The request body, read up to `BODY_LIMIT`. None when it is larger. A declared length over the
/// cap is refused without reading anything; a chunked body is read only up to the cap.
pub async fn read_body(req: &Request) -> Result<Option<Vec<u8>>> {
    if let Some(declared) = req.headers().get("Content-Length")?
        && declared
            .trim()
            .parse::<u64>()
            .is_ok_and(|length| length > BODY_LIMIT as u64)
    {
        return Ok(None);
    }
    let Some(stream) = req.inner().body() else {
        return Ok(Some(Vec::new()));
    };
    let reader: ReadableStreamDefaultReader = stream.get_reader().unchecked_into();
    let mut body = Vec::new();
    loop {
        let chunk = JsFuture::from(reader.read()).await?;
        if Reflect::get(&chunk, &JsValue::from_str("done"))?
            .as_bool()
            .unwrap_or(true)
        {
            return Ok(Some(body));
        }
        let value = Reflect::get(&chunk, &JsValue::from_str("value"))?;
        let bytes = Uint8Array::new(&value).to_vec();
        if body.len() + bytes.len() > BODY_LIMIT {
            return Ok(None);
        }
        body.extend_from_slice(&bytes);
    }
}

/// True when the Host header is absent or names the public origin's host. The port is ignored,
/// as native does.
pub fn host_allowed(host: Option<&str>, public_host: &str) -> bool {
    match host {
        None => true,
        Some(value) => host_without_port(value).to_ascii_lowercase() == public_host,
    }
}

fn host_without_port(value: &str) -> &str {
    if let Some(rest) = value.strip_prefix('[') {
        return match rest.find(']') {
            Some(end) => &value[..end + 2],
            None => value,
        };
    }
    match value.rfind(':') {
        Some(index) if !value[..index].contains(':') => &value[..index],
        _ => value,
    }
}

/// True when `Authorization` is `Bearer <expected>`. The comparison visits every byte, so the
/// time it takes does not depend on where the first difference is.
pub fn bearer_matches(authorization: Option<&str>, expected: &str) -> bool {
    let Some(presented) = authorization.and_then(|value| value.strip_prefix("Bearer ")) else {
        return false;
    };
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    if presented.len() != expected.len() {
        return false;
    }
    presented
        .iter()
        .zip(expected)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

/// `200 ok` for healthz. HEAD gets the same headers and no body.
pub fn healthz(head: bool) -> Result<Response> {
    let mut response = if head {
        Response::empty()?
    } else {
        Response::from_bytes(b"ok".to_vec())?
    };
    let headers = response.headers_mut();
    headers.set("Content-Type", "text/plain; charset=utf-8")?;
    headers.set("Cache-Control", "no-store")?;
    Ok(response)
}

/// Writes a verdict's response as it is: status, every header and the body.
pub fn gate_response(response: GateResponse) -> Result<Response> {
    let body = if response.body.is_empty() {
        Response::empty()?
    } else {
        Response::from_bytes(response.body)?
    };
    let mut out = body.with_status(response.status);
    let headers = out.headers_mut();
    for (name, value) in &response.headers {
        headers.append(name, value)?;
    }
    Ok(out)
}

/// A response whose body stream fails before its first byte, for a request the gate drops. A
/// Worker cannot close the client connection itself, and a thrown error would show Cloudflare's
/// error page. On the edge the failed stream is expected to abort the response instead: the
/// client gets a network error (on HTTP/2 a reset stream, on HTTP/1.1 a closed connection) and no
/// content. Local workerd ends it cleanly as an empty 403. Either way the status and the few
/// headers that go out name nothing: no body type, no product, no HTML.
pub fn dropped() -> Result<Response> {
    let failed = futures_util::stream::once(async {
        Err::<Vec<u8>, worker::Error>(worker::Error::RustError("dropped".to_owned()))
    });
    let mut response = Response::from_stream(failed)?.with_status(403);
    response.headers_mut().set("Cache-Control", "no-store")?;
    Ok(response)
}

/// A 200 JSON response that no cache may keep.
pub fn json(value: &serde_json::Value) -> Result<Response> {
    let mut response = Response::from_json(value)?;
    let headers = response.headers_mut();
    headers.set("Content-Type", "application/json")?;
    headers.set("Cache-Control", "no-store")?;
    Ok(response)
}

/// An empty response with this status.
pub fn status(code: u16) -> Result<Response> {
    Ok(Response::empty()?.with_status(code))
}

/// The security headers native's router adds to every response. A WebSocket upgrade (101) gets
/// none: native does add them there, which is a known difference.
pub fn finish(mut response: Response) -> Result<Response> {
    if response.status_code() != 101 {
        let headers = response.headers_mut();
        headers.set("X-Content-Type-Options", "nosniff")?;
        headers.set("Referrer-Policy", "no-referrer")?;
    }
    Ok(response)
}
