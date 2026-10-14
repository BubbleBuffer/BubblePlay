//! Relay password/TOTP login: `POST {origin}/__auth/login` with JSON, which
//! returns the session cookies. Redirects are not followed.

use std::time::Duration;

use reqwest::header::{RETRY_AFTER, SET_COOKIE};
use serde_json::{json, Map, Value};

use super::config::RelayEndpoint;
use super::cookies::{CookieJar, GATE_COOKIE, SESSION_COOKIE};
use super::error::{LoginError, NetError};

/// Credentials for one login. `totp` is sent only when present.
#[derive(Clone, Copy)]
pub struct LoginRequest<'a> {
    pub password: &'a str,
    pub totp: Option<&'a str>,
    pub remember: bool,
}

impl std::fmt::Debug for LoginRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginRequest")
            .field("password", &"[redacted]")
            .field("has_totp", &self.totp.is_some())
            .field("remember", &self.remember)
            .finish()
    }
}

/// Log in and return the cookie jar for [`crate::net::NetConfig`].
pub async fn login(relay: &str, request: &LoginRequest<'_>) -> Result<CookieJar, NetError> {
    login_with_cookies(relay, request, &CookieJar::new()).await
}

/// Password login can reuse a valid approval gate instead of supplying TOTP.
/// Successful login rotates both cookies; only the newly returned jar is used.
pub async fn login_with_cookies(
    relay: &str,
    request: &LoginRequest<'_>,
    existing: &CookieJar,
) -> Result<CookieJar, NetError> {
    let endpoint = RelayEndpoint::parse(relay)?;
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20));
    // Even protocol-level retries must be off for the single-use dev TOTP.
    #[cfg(feature = "dev-login")]
    {
        builder = builder.retry(reqwest::retry::never());
    }
    if endpoint.is_loopback() {
        // A loopback relay never goes through a system proxy.
        builder = builder.no_proxy();
    }
    let client = builder
        .build()
        .map_err(|error| transport(error.to_string()))?;

    let mut body = Map::new();
    body.insert("password".into(), json!(request.password));
    if let Some(totp) = request.totp {
        body.insert("totp".into(), json!(totp));
    }
    body.insert("remember".into(), json!(request.remember));

    let mut pending = client
        .post(endpoint.http_url("/__auth/login"))
        .header(reqwest::header::ORIGIN, endpoint.origin())
        .json(&Value::Object(body));
    if let Some(cookie) = existing.cookie_header() {
        pending = pending.header(reqwest::header::COOKIE, cookie);
    }
    let response = pending
        .send()
        .await
        .map_err(|error| transport(error.to_string()))?;

    let status = response.status().as_u16();
    match status {
        204 => {
            let cookies: Vec<String> = response
                .headers()
                .get_all(SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok().map(str::to_owned))
                .collect();
            let jar = CookieJar::from_set_cookie(cookies.iter().map(String::as_str));
            if jar.get(GATE_COOKIE).is_none() || jar.get(SESSION_COOKIE).is_none() {
                return Err(NetError::Login(LoginError::MissingCookies));
            }
            Ok(jar)
        }
        401 => Err(NetError::Login(LoginError::Rejected)),
        429 => {
            let retry_after_secs = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok());
            Err(NetError::Login(LoginError::RateLimited {
                retry_after_secs,
            }))
        }
        503 => Err(NetError::Login(LoginError::Unavailable)),
        other => Err(NetError::Login(LoginError::Status(other))),
    }
}

/// Blocking wrapper around [`login`] for callers on a plain thread.
pub fn login_blocking(relay: &str, request: &LoginRequest<'_>) -> Result<CookieJar, NetError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| transport(error.to_string()))?;
    runtime.block_on(login(relay, request))
}

fn transport(message: String) -> NetError {
    NetError::Login(LoginError::Transport(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    /// One-shot HTTP server on 127.0.0.1. Returns the origin and a channel carrying
    /// the raw request text (headers and body) it received.
    fn serve_once(
        status_line: &'static str,
        extra_headers: &'static str,
        body: &'static str,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request = read_request(&mut stream);
            let _ = sender.send(request);
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (origin, receiver)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut data = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            let read = stream.read(&mut buffer).unwrap_or(0);
            if read == 0 {
                break;
            }
            data.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&data).to_string();
            if let Some(header_end) = text.find("\r\n\r\n") {
                let length = text[..header_end]
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if data.len() >= header_end + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&data).to_string()
    }

    const COOKIES_204: &str = "Set-Cookie: __Host-gate=gate-val; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=3600\r\nSet-Cookie: __Host-bp-session=sess-val; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=3600\r\n";

    #[test]
    fn successful_login_posts_json_with_origin_and_returns_both_cookies() {
        let (origin, requests) = serve_once("204 No Content", COOKIES_204, "");
        let jar = login_blocking(
            &origin,
            &LoginRequest {
                password: "pw-1",
                totp: None,
                remember: true,
            },
        )
        .unwrap();
        assert_eq!(jar.get(GATE_COOKIE), Some("gate-val"));
        assert_eq!(jar.get(SESSION_COOKIE), Some("sess-val"));

        let request = requests.recv().unwrap();
        assert!(
            request.starts_with("POST /__auth/login HTTP/1.1"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains(&format!("origin: {origin}").to_ascii_lowercase()),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: application/json"),
            "{request}"
        );
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            body,
            json!({ "password": "pw-1", "remember": true }),
            "totp is omitted when absent"
        );
    }

    #[test]
    fn totp_is_included_only_when_given() {
        let (origin, requests) = serve_once("204 No Content", COOKIES_204, "");
        login_blocking(
            &origin,
            &LoginRequest {
                password: "pw",
                totp: Some("123456"),
                remember: false,
            },
        )
        .unwrap();
        let request = requests.recv().unwrap();
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body,
            json!({ "password": "pw", "totp": "123456", "remember": false })
        );
    }

    #[test]
    fn login_rejection_maps_to_rejected() {
        let (origin, _requests) = serve_once("401 Unauthorized", "", "");
        let error = login_blocking(
            &origin,
            &LoginRequest {
                password: "bad",
                totp: None,
                remember: false,
            },
        )
        .unwrap_err();
        assert_eq!(error, NetError::Login(LoginError::Rejected));
    }

    #[test]
    fn rate_limit_reads_retry_after_seconds() {
        let (origin, _requests) = serve_once("429 Too Many Requests", "Retry-After: 42\r\n", "");
        let error = login_blocking(
            &origin,
            &LoginRequest {
                password: "x",
                totp: None,
                remember: false,
            },
        )
        .unwrap_err();
        assert_eq!(
            error,
            NetError::Login(LoginError::RateLimited {
                retry_after_secs: Some(42)
            })
        );
    }

    #[test]
    fn unavailable_and_other_statuses_map_to_their_variants() {
        let (origin, _requests) = serve_once("503 Service Unavailable", "", "");
        let error = login_blocking(
            &origin,
            &LoginRequest {
                password: "x",
                totp: None,
                remember: false,
            },
        )
        .unwrap_err();
        assert_eq!(error, NetError::Login(LoginError::Unavailable));

        let (origin, _requests) = serve_once("418 I'm a teapot", "", "");
        let error = login_blocking(
            &origin,
            &LoginRequest {
                password: "x",
                totp: None,
                remember: false,
            },
        )
        .unwrap_err();
        assert_eq!(error, NetError::Login(LoginError::Status(418)));
    }

    #[test]
    fn a_204_without_both_cookies_is_an_error() {
        let (origin, _requests) = serve_once(
            "204 No Content",
            "Set-Cookie: __Host-gate=only; Path=/\r\n",
            "",
        );
        let error = login_blocking(
            &origin,
            &LoginRequest {
                password: "x",
                totp: None,
                remember: false,
            },
        )
        .unwrap_err();
        assert_eq!(error, NetError::Login(LoginError::MissingCookies));
    }

    #[test]
    fn plaintext_remote_relay_is_refused_before_any_request() {
        let error = login_blocking(
            "http://relay.example",
            &LoginRequest {
                password: "x",
                totp: None,
                remember: false,
            },
        )
        .unwrap_err();
        assert!(matches!(error, NetError::PlaintextRefused(_)), "{error:?}");
    }

    #[test]
    fn connection_refused_is_a_transport_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let origin = format!("http://127.0.0.1:{port}");
        let error = login_blocking(
            &origin,
            &LoginRequest {
                password: "x",
                totp: None,
                remember: false,
            },
        )
        .unwrap_err();
        assert!(
            matches!(error, NetError::Login(LoginError::Transport(_))),
            "{error:?}"
        );
    }
}
