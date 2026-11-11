//! Static assets, served from the `ASSETS` binding (Workers Static Assets over the JS Worker's
//! public directory). The Durable Object calls this only after the gate has allowed the request.
//!
//! The rules are native's (server/src/assets.rs) where the binding does not give them:
//! - only GET and HEAD; anything else is 404;
//! - a malformed percent escape is 400;
//! - a backslash, a dot segment or a dotfile is 404 before anything is fetched. Native cleans
//!   `..` lexically and serves the result; this does not, which is a known difference;
//! - `/pair` is served as `/`, the browser page, as native does;
//! - `/index.html`, when the file exists, is a 301 to `./` (keeping the query), as native does;
//! - `Cache-Control: no-store` on every served file.

use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use worker::{Env, Method, RequestInit, Response, Result, Url};

/// The viewer needs inline styles, same-origin module workers and WebAssembly for Argon2.
/// Inline scripts are admitted only by their response-specific content hashes.
pub fn security_headers(response: &mut Response, html: Option<&str>) -> Result<()> {
    let mut script = "script-src 'self' 'wasm-unsafe-eval'".to_owned();
    if let Some(html) = html {
        // HTML parsers normalize line endings before checking script hashes. The public page
        // uses plain <script> for its inline bootstrap and src attributes for external scripts.
        let normalized = html.replace("\r\n", "\n").replace('\r', "\n");
        for tail in normalized.split("<script>").skip(1) {
            if let Some((source, _)) = tail.split_once("</script>") {
                let hash = STANDARD.encode(Sha256::digest(source.as_bytes()));
                script.push_str(&format!(" 'sha256-{hash}'"));
            }
        }
    }
    let policy = format!(
        "default-src 'self'; {script}; connect-src 'self' wss: https:; img-src 'self' data: blob:; media-src 'self' blob:; style-src 'self' 'unsafe-inline'; worker-src 'self' blob:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
    );
    let headers = response.headers_mut();
    headers.set("Content-Security-Policy", &policy)?;
    headers.set("X-Frame-Options", "DENY")?;
    Ok(())
}

/// What to ask the binding for.
pub enum Plan {
    /// The percent-encoded path to fetch. `index` is set when the request named `/index.html`.
    Fetch { path: String, index: bool },
    /// Answer with this status and no body.
    Status(u16),
}

/// Decides what to fetch from the raw (undecoded) request path.
pub fn plan(method: &str, raw_path: &str) -> Plan {
    if method != "GET" && method != "HEAD" {
        return Plan::Status(404);
    }
    let Some(decoded) = percent_decode(raw_path) else {
        return Plan::Status(400);
    };
    if decoded.contains('\\') {
        return Plan::Status(404);
    }
    let segments: Vec<&str> = decoded
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.iter().any(|segment| segment.starts_with('.')) {
        return Plan::Status(404);
    }
    // `/pair` is the same page as `/`: the browser reads the pairing code from the fragment. The
    // binding serves that page at `/`, so `/pair` fetches `/`, as native maps it to index.html.
    if segments == ["pair"] {
        return Plan::Fetch {
            path: "/".to_owned(),
            index: false,
        };
    }
    let index = decoded.ends_with("/index.html");
    let path = if segments.is_empty() {
        "/".to_owned()
    } else {
        segments
            .iter()
            .map(|segment| format!("/{}", percent_encode(segment)))
            .collect()
    };
    Plan::Fetch { path, index }
}

/// Fetches `path` from the binding and returns the file, or the binding's status for anything
/// else. `url` supplies the origin and the query.
pub async fn serve(env: &Env, url: &Url, head: bool, path: &str, index: bool) -> Result<Response> {
    let assets = env.assets("ASSETS")?;
    let mut target = url.clone();
    target.set_path(path);
    let mut init = RequestInit::new();
    // Read the same representation for HEAD so its CSP hashes equal GET's hashes.
    init.with_method(Method::Get);
    let mut asset = assets.fetch(target.as_str(), Some(init)).await?;
    let status = asset.status_code();
    if status != 200 {
        let mut out = Response::empty()?.with_status(status);
        if let Some(location) = asset.headers().get("Location")? {
            out.headers_mut().set("Location", &location)?;
        }
        security_headers(&mut out, None)?;
        return Ok(out);
    }
    if index {
        return redirect_to_dot_slash(url.query());
    }
    let body = asset.bytes().await?;
    let content_type = asset.headers().get("Content-Type")?;
    let html = content_type
        .as_deref()
        .filter(|value| value.starts_with("text/html"))
        .and_then(|_| std::str::from_utf8(&body).ok())
        .map(str::to_owned);
    let mut out = if head || body.is_empty() {
        Response::empty()?
    } else {
        Response::from_bytes(body)?
    };
    let headers = out.headers_mut();
    if let Some(content_type) = content_type {
        headers.set("Content-Type", &content_type)?;
    }
    headers.set("Cache-Control", "no-store")?;
    security_headers(&mut out, html.as_deref())?;
    Ok(out)
}

fn redirect_to_dot_slash(query: Option<&str>) -> Result<Response> {
    let location = match query {
        Some(query) if !query.is_empty() => format!("./?{query}"),
        _ => "./".to_owned(),
    };
    let mut out = Response::empty()?.with_status(301);
    out.headers_mut().set("Location", &location)?;
    security_headers(&mut out, None)?;
    Ok(out)
}

/// Percent-decodes a path. None for a malformed escape or for bytes that are not UTF-8.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = hex_value(*bytes.get(i + 1)?)?;
            let low = hex_value(*bytes.get(i + 2)?)?;
            out.push(high * 16 + low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|digit| digit as u8)
}

/// Escapes every byte outside the unreserved set, so a decoded `%`, `?` or `#` stays in its
/// segment.
fn percent_encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
