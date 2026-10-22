//! The axum router. `/healthz` is answered first, as the Docker probe needs it. Every other
//! request goes through the gate; what the gate allows goes to the admin API, a WebSocket route or
//! the static-asset handler. Every response gets the common security headers.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use bp_relay_core::gate::{Request as GateRequest, Response as GateResponse, Verdict};
use bp_relay_core::relay::{AdminError, UpgradeRequest};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use url::form_urlencoded;

use crate::assets::serve_asset;
use crate::net::client_ip;
use crate::runtime::{OsRandom, Shared, now_ms};
use crate::socket::{accept, route_for_path};

/// The state every handler gets.
pub type AppState = Arc<Shared>;

/// The largest body a request may carry into the gate. A larger body is 413 and never reaches it.
const GATE_BODY_LIMIT: usize = 16 * 1024;

pub fn router(state: AppState) -> Router {
    Router::new()
        .fallback(dispatch)
        .layer(middleware::from_fn(common_headers))
        .with_state(state)
}

async fn common_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

async fn dispatch(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    if parts.uri.path() == "/healthz"
        && (parts.method == Method::GET || parts.method == Method::HEAD)
    {
        return healthz(&state, peer, &parts.headers, parts.method == Method::HEAD);
    }
    let body: Bytes = match to_bytes(body, GATE_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let path = parts.uri.path();
    let websocket = header_str(&parts.headers, "upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let client = client_ip(peer, &parts.headers, &state.trusted_proxies);
    let verdict = {
        let req = GateRequest {
            method: parts.method.as_str(),
            path,
            websocket,
            origin: header_str(&parts.headers, "origin"),
            cookie: header_str(&parts.headers, "cookie"),
            authorization: header_str(&parts.headers, "authorization"),
            sec_fetch_dest: header_str(&parts.headers, "sec-fetch-dest"),
            body: &body[..],
            client_ip: client,
            now_ms: now_ms(),
        };
        state.gate().handle(&mut OsRandom, &req)
    };
    match verdict {
        Verdict::Refuse => StatusCode::NOT_FOUND.into_response(),
        Verdict::Wait(response, effects) | Verdict::Handled(response, effects) => {
            state.run_gate_effects(effects).await;
            gate_response(response)
        }
        Verdict::Allow => {
            if path.starts_with("/__gate/manage/") {
                return admin(&state, &parts.method, path, &parts.headers).await;
            }
            if websocket {
                let Some(route) = route_for_path(path) else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                let (pc, epoch) = query_values(parts.uri.query());
                let viewer_authorized = cfg!(feature = "lab")
                    || state.cookie_valid(header_str(&parts.headers, "cookie"), now_ms());
                let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
                    Ok(upgrade) => upgrade,
                    Err(rejection) => return rejection.into_response(),
                };
                let req = UpgradeRequest {
                    route,
                    pc,
                    epoch,
                    viewer_authorized,
                };
                return accept(state, upgrade, req);
            }
            serve_asset(&state.assets_dir, &parts.method, path, parts.uri.query()).await
        }
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// `pc` and `epoch` from a query string: the first of each.
fn query_values(query: Option<&str>) -> (Option<String>, Option<String>) {
    let mut pc = None;
    let mut epoch = None;
    for (key, value) in form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
        match &*key {
            "pc" if pc.is_none() => pc = Some(value.into_owned()),
            "epoch" if epoch.is_none() => epoch = Some(value.into_owned()),
            _ => {}
        }
    }
    (pc, epoch)
}

fn gate_response(response: GateResponse) -> Response {
    let mut out = Response::new(Body::from(response.body));
    *out.status_mut() =
        StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    for (name, value) in response.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            out.headers_mut().append(name, value);
        }
    }
    out
}

/// `200 ok` for a loopback peer or a request bearing the admin token, when the Host header names
/// a loopback address, the public origin host or the listen address (or is absent). Anything else
/// is a 404. HEAD gets the status and headers only.
fn healthz(state: &Shared, peer: SocketAddr, headers: &HeaderMap, head: bool) -> Response {
    let allowed = host_allowed(state, headers)
        && (peer.ip().is_loopback() || bearer_matches(headers, &state.admin_token));
    if !allowed {
        return StatusCode::NOT_FOUND.into_response();
    }
    let body = if head { "" } else { "ok" };
    ([(header::CACHE_CONTROL, "no-store")], body).into_response()
}

fn host_allowed(state: &Shared, headers: &HeaderMap) -> bool {
    let Some(value) = header_str(headers, "host") else {
        return true;
    };
    let host = host_without_port(value).to_ascii_lowercase();
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
        || host == state.public_host
        || host == state.listen_host.to_ascii_lowercase()
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

/// Constant-time check of `Authorization: Bearer <token>`.
fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    let Some(presented) =
        header_str(headers, "authorization").and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    bool::from(presented.as_bytes().ct_eq(expected.as_bytes()))
}

/// The admin API, behind the admin bearer. Anything wrong with the bearer, the route or the id is
/// 404.
async fn admin(state: &Shared, method: &Method, path: &str, headers: &HeaderMap) -> Response {
    if !bearer_matches(headers, &state.admin_token) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if *method == Method::GET && path == "/__gate/manage/pcs" {
        let listing = state.relay().list_pcs();
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
                        })
                    })
                    .collect();
                json_response(StatusCode::OK, &Value::Array(rows))
            }
            Err(AdminError::Unavailable) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            Err(AdminError::NotFound) => StatusCode::NOT_FOUND.into_response(),
        };
    }
    if let Some(id) = path.strip_prefix("/__gate/manage/pcs/") {
        if *method == Method::DELETE && !id.is_empty() && !id.contains('/') {
            return delete_pc(state, id).await;
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn delete_pc(state: &Shared, id: &str) -> Response {
    let result = state.relay().delete_pc(id);
    match result {
        Ok(actions) => match state.apply(None, actions).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        },
        Err(AdminError::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(AdminError::Unavailable) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn json_response(status: StatusCode, value: &Value) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_default();
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
