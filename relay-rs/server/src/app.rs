//! The axum router: `/healthz` and the static-asset fallback, with the headers every response gets.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use subtle::ConstantTimeEq;

use crate::assets::serve_asset;

#[derive(Clone)]
pub struct AppState {
    pub admin_token: Arc<str>,
    pub assets_dir: Arc<Path>,
}

impl AppState {
    pub fn new(admin_token: &str, assets_dir: &Path) -> Self {
        Self {
            admin_token: Arc::from(admin_token),
            assets_dir: Arc::from(assets_dir),
        }
    }
}

/// The router for `serve`. Every path that is not `/healthz` goes to the asset handler, which
/// answers 404 for anything it does not serve.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .fallback(serve_asset)
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

/// `200 ok` for a loopback peer or a request bearing the admin token. Anything else is a 404.
async fn healthz(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if peer.ip().is_loopback() || bearer_matches(&headers, &state.admin_token) {
        ([(header::CACHE_CONTROL, "no-store")], "ok").into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// Constant-time check of `Authorization: Bearer <token>`.
fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(presented) = value.strip_prefix("Bearer ") else {
        return false;
    };
    bool::from(presented.as_bytes().ct_eq(expected.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_check_needs_the_exact_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer admin-x"),
        );
        assert!(bearer_matches(&headers, "admin-x"));
        assert!(!bearer_matches(&headers, "admin-y"));
        assert!(!bearer_matches(&headers, "admin"));
    }

    #[test]
    fn bearer_check_needs_the_bearer_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic admin-x"),
        );
        assert!(!bearer_matches(&headers, "admin-x"));
        assert!(!bearer_matches(&HeaderMap::new(), "admin-x"));
    }
}
