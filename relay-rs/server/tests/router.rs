//! `/healthz` and the static-asset handler, driven through the axum router with tower's `oneshot`.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::{Body, to_bytes};
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::http::{Request, Response, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bp_relay_core::gate::GATE_HOLD_MS;
use bp_relay_server::app::router;
use bp_relay_server::config::{Config, Secret};
use bp_relay_server::runtime::{Shared, cookie_hash};
use tower::ServiceExt;

const ADMIN: &str = "adm-test-token";
/// The gate cookie every test request carries. Its hash is pre-seeded in gate.json.
const COOKIE: &str = "__Host-gate=testcookie";

struct Site {
    _root: tempfile::TempDir,
    assets: PathBuf,
}

/// assets/ with a page, a script, a stylesheet, a nested page, dotfiles and a binary file, plus
/// a secret one level above assets/ that must never be served.
fn site() -> Site {
    let root = tempfile::tempdir().unwrap();
    let assets = root.path().join("assets");
    fs::create_dir_all(assets.join("sub")).unwrap();
    fs::create_dir_all(assets.join(".hidden")).unwrap();
    fs::write(assets.join("index.html"), "home").unwrap();
    fs::write(assets.join("app.js"), "console.log(1)").unwrap();
    fs::write(assets.join("style.css"), "body{}").unwrap();
    fs::write(assets.join("sub/page.html"), "sub page").unwrap();
    fs::write(assets.join(".env"), "SECRET=1").unwrap();
    fs::write(assets.join(".hidden/x.txt"), "hidden").unwrap();
    fs::write(assets.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
    fs::write(root.path().join("secret.txt"), "outside").unwrap();
    Site {
        assets,
        _root: root,
    }
}

/// The router over `assets`, with state files in a sibling `state/` directory and a gate cookie
/// that every request carries.
fn app(assets: &Path) -> Router {
    app_with_proxies(assets, Vec::new())
}

fn app_with_proxies(assets: &Path, trusted_proxy_cidrs: Vec<ipnet::IpNet>) -> Router {
    let state_dir = assets.parent().unwrap().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let cookie = URL_SAFE_NO_PAD.encode(cookie_hash("testcookie"));
    fs::write(
        state_dir.join("gate.json"),
        format!(r#"{{"version":1,"cookies":[{{"hash":"{cookie}","expires_ms":4102444800000}}]}}"#),
    )
    .unwrap();
    let cfg = Config {
        public_origin: "https://relay.test".to_string(),
        listen: "127.0.0.1:8080".parse().unwrap(),
        assets_dir: assets.to_path_buf(),
        state_dir,
        token: Secret::new("tok-test"),
        admin_token: Secret::new(ADMIN),
        max_connections: 32,
        max_unauthenticated_connections: 16,
        max_unauthenticated_per_prefix: 4,
        upgrade_rate_per_prefix: 30,
        ntfy_publish_limit: 10,
        ntfy_publish_window_s: 600,
        auth_failure_limit: 5,
        auth_failure_window_s: 600,

        turn: None,
        ntfy: None,
        approval_ttl_s: 90,
        trusted_proxy_cidrs,
        gate_hold_ms: GATE_HOLD_MS,
    };
    router(Arc::new(Shared::new(&cfg).unwrap()))
}

fn request(method: &str, uri: &str, peer: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", COOKIE)
        .body(Body::empty())
        .unwrap();
    let peer: SocketAddr = peer.parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    request
}

fn with_header(mut request: Request<Body>, name: &str, value: &str) -> Request<Body> {
    request.headers_mut().insert(
        axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
        value.parse().unwrap(),
    );
    request
}

async fn send(router: Router, request: Request<Body>) -> Response<Body> {
    router.oneshot(request).await.unwrap()
}

async fn body_text(response: Response<Body>) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn get(site: &Site, uri: &str) -> Response<Body> {
    send(app(&site.assets), request("GET", uri, "127.0.0.1:5000")).await
}

#[tokio::test]
async fn healthz_loopback_v4_is_ok() {
    let site = site();
    let response = send(
        app(&site.assets),
        request("GET", "/healthz", "127.0.0.1:5000"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "ok");
}

#[tokio::test]
async fn healthz_loopback_v6_is_ok() {
    let site = site();
    let response = send(app(&site.assets), request("GET", "/healthz", "[::1]:5000")).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn healthz_non_loopback_peer_is_404() {
    let site = site();
    let response = send(
        app(&site.assets),
        request("GET", "/healthz", "203.0.113.9:5000"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn healthz_admin_bearer_from_outside_is_ok() {
    let site = site();
    let req = with_header(
        request("GET", "/healthz", "203.0.113.9:5000"),
        "authorization",
        &format!("Bearer {ADMIN}"),
    );
    let response = send(app(&site.assets), req).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "ok");
}

#[tokio::test]
async fn healthz_wrong_bearer_from_outside_is_404() {
    let site = site();
    let req = with_header(
        request("GET", "/healthz", "203.0.113.9:5000"),
        "authorization",
        "Bearer not-the-admin-token",
    );
    let response = send(app(&site.assets), req).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn healthz_head_from_loopback_is_ok_without_body() {
    let site = site();
    let response = send(
        app(&site.assets),
        request("HEAD", "/healthz", "127.0.0.1:5000"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_text(response).await.is_empty());
}

#[tokio::test]
async fn healthz_carries_the_common_security_headers() {
    let site = site();
    let response = send(
        app(&site.assets),
        request("GET", "/healthz", "127.0.0.1:5000"),
    )
    .await;
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    assert_eq!(response.headers()["cache-control"], "no-store");
}

#[tokio::test]
async fn assets_root_serves_index_html() {
    let site = site();
    let response = get(&site, "/").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/html; charset=utf-8"
    );
    assert_eq!(body_text(response).await, "home");
}

#[tokio::test]
async fn assets_direct_index_html_redirects_to_dot_slash() {
    let site = site();
    let response = get(&site, "/index.html").await;
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(response.headers()["location"], "./");
}

#[tokio::test]
async fn assets_javascript_gets_a_javascript_type_with_charset() {
    let site = site();
    let response = get(&site, "/app.js").await;
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response.headers()["content-type"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_type.starts_with("text/javascript"),
        "{content_type}"
    );
    assert!(content_type.ends_with("charset=utf-8"), "{content_type}");
}

#[tokio::test]
async fn assets_binary_gets_its_own_type_and_no_charset() {
    let site = site();
    let response = get(&site, "/blob.bin").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/octet-stream"
    );
}

#[tokio::test]
async fn assets_nested_file_is_served() {
    let site = site();
    let response = get(&site, "/sub/page.html").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "sub page");
}

#[tokio::test]
async fn assets_responses_are_no_store_with_security_headers() {
    let site = site();
    let response = get(&site, "/style.css").await;
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
}

#[tokio::test]
async fn assets_head_reports_length_without_a_body() {
    let site = site();
    let response = send(
        app(&site.assets),
        request("HEAD", "/app.js", "127.0.0.1:5000"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-length"], "14");
    assert!(body_text(response).await.is_empty());
}

#[tokio::test]
async fn assets_percent_encoded_slash_is_decoded() {
    let site = site();
    let response = get(&site, "/sub%2Fpage.html").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "sub page");
}

#[tokio::test]
async fn assets_traversal_above_the_root_is_404() {
    let site = site();
    assert_eq!(
        get(&site, "/../secret.txt").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&site, "/%2e%2e/secret.txt").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn assets_traversal_through_a_subdirectory_is_404() {
    let site = site();
    assert_eq!(
        get(&site, "/sub/../../secret.txt").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn assets_dotfiles_and_dot_directories_are_404() {
    let site = site();
    assert_eq!(get(&site, "/.env").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        get(&site, "/.hidden/x.txt").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&site, "/sub/.env").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn assets_directories_and_missing_files_are_404() {
    let site = site();
    assert_eq!(get(&site, "/sub").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(get(&site, "/sub/").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        get(&site, "/nope.html").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn assets_non_get_methods_are_404() {
    let site = site();
    let response = send(
        app(&site.assets),
        request("POST", "/app.js", "127.0.0.1:5000"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn assets_backslash_in_the_path_is_404() {
    let site = site();
    assert_eq!(
        get(&site, "/sub%5Cpage.html").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn assets_malformed_escape_is_400() {
    let site = site();
    assert_eq!(get(&site, "/%zz").await.status(), StatusCode::BAD_REQUEST);
}

#[cfg(unix)]
#[tokio::test]
async fn assets_symlink_escaping_the_root_is_404() {
    let site = site();
    std::os::unix::fs::symlink(
        site.assets.parent().unwrap().join("secret.txt"),
        site.assets.join("escape.txt"),
    )
    .unwrap();
    assert_eq!(
        get(&site, "/escape.txt").await.status(),
        StatusCode::NOT_FOUND
    );
}

// -- GET /presence ----------------------------------------------------------------------------

fn presence_id(seed: u8) -> String {
    bp_device_key::DeviceIdentity::from_seed(&[seed; 32])
        .id()
        .to_string()
}

/// A request without the gate cookie, as the native app sends it.
fn cookieless(method: &str, uri: &str, peer: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let peer: SocketAddr = peer.parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    request
}

#[tokio::test]
async fn presence_cookieless_get_answers_json_no_store() {
    let site = site();
    let (a, b) = (presence_id(1), presence_id(2));
    let uri = format!("/presence?ids={a}%2C{b}");
    let response = send(
        app(&site.assets),
        cookieless("GET", &uri, "203.0.113.9:5000"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(headers["cache-control"], "no-store");
    assert!(headers.get("set-cookie").is_none());
    let body: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(body, serde_json::json!({ a: false, b: false }));
}

#[tokio::test]
async fn presence_refuses_bad_queries_with_400() {
    let site = site();
    let router = app(&site.assets);
    let many = (0..21).map(presence_id).collect::<Vec<_>>().join(",");
    for uri in [
        format!("/presence?ids={many}"),
        "/presence?ids=not-an-id".to_string(),
        format!("/presence?ids={},", presence_id(1)),
        "/presence".to_string(),
    ] {
        let response = send(router.clone(), cookieless("GET", &uri, "203.0.113.9:5000")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(response.headers()["content-type"], "application/json");
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
}

#[tokio::test]
async fn presence_rate_limit_is_per_prefix() {
    let site = site();
    let router = app(&site.assets);
    let uri = format!("/presence?ids={}", presence_id(1));
    for n in 0..60 {
        let peer = format!("203.0.113.{}:5000", n % 200);
        let response = send(router.clone(), cookieless("GET", &uri, &peer)).await;
        assert_eq!(response.status(), StatusCode::OK, "request {n}");
    }
    let response = send(
        router.clone(),
        cookieless("GET", &uri, "203.0.113.250:5000"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = response.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry));
    assert_eq!(response.headers()["cache-control"], "no-store");
    let response = send(router.clone(), cookieless("GET", &uri, "198.51.100.9:5000")).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn presence_foreign_origin_and_other_methods_keep_gate_answers() {
    let site = site();
    let router = app(&site.assets);
    let uri = format!("/presence?ids={}", presence_id(1));
    let req = with_header(
        cookieless("GET", &uri, "203.0.113.9:5000"),
        "origin",
        "https://evil.example",
    );
    assert_eq!(
        send(router.clone(), req).await.status(),
        StatusCode::NOT_FOUND
    );
    let req = with_header(
        cookieless("GET", &uri, "203.0.113.9:5000"),
        "origin",
        "https://relay.test",
    );
    assert_eq!(send(router.clone(), req).await.status(), StatusCode::OK);
    let response = send(router.clone(), cookieless("POST", &uri, "203.0.113.9:5000")).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn healthz_trusted_loopback_proxy_requires_bearer() {
    let site = site();
    let app = app_with_proxies(&site.assets, vec!["127.0.0.0/8".parse().unwrap()]);
    let req = request("GET", "/healthz", "127.0.0.1:5000");
    assert_eq!(send(app.clone(), req).await.status(), StatusCode::NOT_FOUND);
    let req = with_header(
        request("GET", "/healthz", "127.0.0.1:5000"),
        "authorization",
        &format!("Bearer {ADMIN}"),
    );
    assert_eq!(send(app, req).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn admin_and_healthz_share_five_failure_prefix_lockout() {
    let site = site();
    let app = app(&site.assets);
    for i in 0..5 {
        let path = if i % 2 == 0 {
            "/__gate/manage/pcs"
        } else {
            "/healthz"
        };
        let req = with_header(
            request("GET", path, "203.0.113.9:5000"),
            "authorization",
            "Bearer wrong",
        );
        assert_eq!(send(app.clone(), req).await.status(), StatusCode::NOT_FOUND);
    }
    for path in ["/healthz", "/__gate/manage/pcs"] {
        let req = with_header(
            request("GET", path, "203.0.113.250:5000"),
            "authorization",
            &format!("Bearer {ADMIN}"),
        );
        assert_eq!(send(app.clone(), req).await.status(), StatusCode::NOT_FOUND);
        let req = with_header(
            request("GET", path, "198.51.100.9:5000"),
            "authorization",
            &format!("Bearer {ADMIN}"),
        );
        assert_eq!(send(app.clone(), req).await.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn assets_carry_csp_with_inline_hash_and_frame_denial() {
    let site = site();
    let inline = "console.log('bootstrap')";
    fs::write(
        site.assets.join("index.html"),
        format!("<script>{inline}</script>"),
    )
    .unwrap();
    let response = get(&site, "/").await;
    let policy = response.headers()["content-security-policy"]
        .to_str()
        .unwrap();
    use sha2::{Digest, Sha256};
    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(inline.as_bytes()));
    assert!(policy.contains(&format!("'sha256-{hash}'")));
    for directive in [
        "default-src 'self'",
        "frame-ancestors 'none'",
        "worker-src 'self' blob:",
        "'wasm-unsafe-eval'",
    ] {
        assert!(policy.contains(directive), "{policy}");
    }
    assert_eq!(response.headers()["x-frame-options"], "DENY");
    let head = send(app(&site.assets), request("HEAD", "/", "127.0.0.1:5000")).await;
    assert_eq!(
        head.headers()["content-security-policy"],
        response.headers()["content-security-policy"]
    );
}
