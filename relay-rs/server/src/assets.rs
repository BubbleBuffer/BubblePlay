//! Static assets from `assets_dir`. This follows the Go relay's `serveAsset`:
//!
//! - only GET and HEAD; anything else is a 404;
//! - the path is cleaned (`path.Clean` rules) and `/` maps to `/index.html`;
//! - a backslash in the cleaned path is a 404;
//! - the file must resolve (symlinks followed) to a regular file inside the assets directory;
//! - a request ending in `/index.html` redirects to `./`, as `http.ServeFile` does;
//! - `Cache-Control: no-store`; the nosniff and no-referrer headers come from the router layer.
//!
//! Dotfiles (any path segment starting with `.`) are refused too. The Go relay does not do that
//! check; it is part of this server's spec.

use std::path::Path;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// `path.Clean("/" + input)` for a rooted path: drops empty and `.` segments, applies `..`
/// lexically, and never climbs above the root.
pub fn clean_path(input: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in input.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    format!("/{}", parts.join("/"))
}

/// Percent-decodes the request path. `None` for a malformed escape or invalid UTF-8.
fn decode_path(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let text = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(text, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

fn content_type(path: &Path) -> String {
    let guess = mime_guess::from_path(path).first_or_octet_stream();
    let essence = guess.essence_str();
    let charset = essence.starts_with("text/") || essence == "application/javascript";
    if charset {
        format!("{essence}; charset=utf-8")
    } else {
        essence.to_string()
    }
}

/// Serves `raw_path` (undecoded) from `root`. Called after the gate has allowed the request.
pub async fn serve_asset(
    root: &Path,
    method: &Method,
    raw_path: &str,
    query: Option<&str>,
) -> Response {
    if *method != Method::GET && *method != Method::HEAD {
        return not_found();
    }
    let Some(decoded) = decode_path(raw_path) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let cleaned = clean_path(&decoded);
    let cleaned = if cleaned == "/" {
        "/index.html".to_string()
    } else {
        cleaned
    };
    if cleaned.contains('\\') || cleaned.split('/').any(|segment| segment.starts_with('.')) {
        return not_found();
    }

    let Ok(root) = tokio::fs::canonicalize(root).await else {
        return not_found();
    };
    let Ok(resolved) = tokio::fs::canonicalize(root.join(&cleaned[1..])).await else {
        return not_found();
    };
    if !resolved.starts_with(&root) {
        return not_found();
    }
    let Ok(meta) = tokio::fs::metadata(&resolved).await else {
        return not_found();
    };
    if !meta.is_file() {
        return not_found();
    }

    if decoded.ends_with("/index.html") {
        return redirect_to_dot_slash(query);
    }

    let body = if *method == Method::HEAD {
        Vec::new()
    } else {
        match tokio::fs::read(&resolved).await {
            Ok(bytes) => bytes,
            Err(_) => return not_found(),
        }
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type(&resolved))
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(length) = HeaderValue::from_str(&meta.len().to_string()) {
        headers.insert(header::CONTENT_LENGTH, length);
    }
    let mut response = Response::new(Body::from(body));
    *response.headers_mut() = headers;
    response
}

fn redirect_to_dot_slash(query: Option<&str>) -> Response {
    let location = match query {
        Some(q) if !q.is_empty() => format!("./?{q}"),
        _ => "./".to_string(),
    };
    let mut response = StatusCode::MOVED_PERMANENTLY.into_response();
    if let Ok(value) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_path_matches_go_path_clean() {
        assert_eq!(clean_path("/"), "/");
        assert_eq!(clean_path("/a/./b//c/"), "/a/b/c");
        assert_eq!(clean_path("/a/../b"), "/b");
        assert_eq!(clean_path("/../../etc/passwd"), "/etc/passwd");
        assert_eq!(clean_path("/a/b/.."), "/a");
        assert_eq!(clean_path(""), "/");
    }

    #[test]
    fn decode_path_handles_escapes_and_rejects_bad_ones() {
        assert_eq!(decode_path("/a%20b").as_deref(), Some("/a b"));
        assert_eq!(decode_path("/%2e%2e/x").as_deref(), Some("/../x"));
        assert_eq!(decode_path("/%zz"), None);
        assert_eq!(decode_path("/%2"), None);
        assert_eq!(decode_path("/%ff"), None);
    }

    #[test]
    fn content_type_adds_charset_for_text_and_js() {
        assert_eq!(
            content_type(Path::new("a.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("a.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("a.png")), "image/png");
        assert_eq!(
            content_type(Path::new("a.unknownext")),
            "application/octet-stream"
        );
    }
}
