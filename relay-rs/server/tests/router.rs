//! `/healthz` and the static-asset handler, driven through the axum router with tower's `oneshot`.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, Response, StatusCode};
use bp_relay_server::app::{AppState, router};
use bp_relay_server::assets::asset_gate;
use tower::ServiceExt;

const ADMIN: &str = "adm-test-token";

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

fn app(assets: &Path) -> Router {
    router(AppState::new(ADMIN, assets))
}

fn request(method: &str, uri: &str, peer: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
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

#[test]
fn asset_gate_allows_everything_for_now() {
    assert!(asset_gate(&axum::http::HeaderMap::new()));
}
