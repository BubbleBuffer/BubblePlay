//! Security headers both relay adapters put on the viewer's responses, so the native server and
//! the Cloudflare Worker serve the same policy.
//!
//! The policy is derived from the viewer in `worker/public`: scripts, workers and the Argon2 WASM
//! module come from the relay's own origin; the page has one inline `<style>` block; monitor
//! previews are `blob:` images; the signaling and media sockets and the one `fetch` go back to
//! the relay's own origin.

/// The Content-Security-Policy of every HTML response.
///
/// - `script-src 'self' 'wasm-unsafe-eval'`: no inline script (the page only loads files).
///   `'wasm-unsafe-eval'` lets the vendored hash-wasm module compile its WebAssembly for the
///   device-key passphrase KDF; it does not allow `eval` or inline script.
/// - `style-src 'self' 'unsafe-inline'`: the page has an inline `<style>` block.
/// - `img-src 'self' blob:`: icons, and the monitor previews that are object URLs.
/// - `connect-src 'self'`: the WebSockets and the gate's `fetch` all go to the relay's origin.
/// - `worker-src 'self'`: the latency, WSS-decode and KDF Web Workers are files on the origin.
pub const VIEWER_CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; \
style-src 'self' 'unsafe-inline'; img-src 'self' blob:; connect-src 'self'; worker-src 'self'; \
font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// One year, no `includeSubDomains`: the relay's origin only.
pub const HSTS: &str = "max-age=31536000";

/// The headers to add to a response with this `Content-Type`.
///
/// `https` is true only when the adapter knows the origin is served over HTTPS (a configured
/// `https` public origin, or an `https` request URL); it gates HSTS, which a plain-HTTP or
/// loopback development server must not send.
pub fn security_headers(
    content_type: Option<&str>,
    https: bool,
) -> Vec<(&'static str, &'static str)> {
    let mut headers = vec![
        ("X-Content-Type-Options", "nosniff"),
        ("Referrer-Policy", "no-referrer"),
        ("X-Frame-Options", "DENY"),
    ];
    let html = content_type.is_some_and(|value| {
        value
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("text/html")
    });
    if html {
        headers.push(("Content-Security-Policy", VIEWER_CSP));
    }
    if https {
        headers.push(("Strict-Transport-Security", HSTS));
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value<'a>(headers: &'a [(&'static str, &'static str)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, v)| *v)
    }

    #[test]
    fn html_gets_the_csp_and_the_framing_headers() {
        for content_type in ["text/html; charset=utf-8", "TEXT/HTML"] {
            let headers = security_headers(Some(content_type), false);
            let csp = value(&headers, "Content-Security-Policy").expect(content_type);
            for directive in [
                "default-src 'self'",
                "script-src 'self' 'wasm-unsafe-eval'",
                "object-src 'none'",
                "base-uri 'none'",
                "frame-ancestors 'none'",
            ] {
                assert!(csp.contains(directive), "{directive} in {csp}");
            }
            assert!(!csp.contains("script-src 'self' 'wasm-unsafe-eval' 'unsafe-inline'"));
            assert_eq!(value(&headers, "X-Frame-Options"), Some("DENY"));
            assert_eq!(value(&headers, "X-Content-Type-Options"), Some("nosniff"));
            assert_eq!(value(&headers, "Referrer-Policy"), Some("no-referrer"));
        }
    }

    #[test]
    fn only_html_gets_the_csp_and_only_https_gets_hsts() {
        let js = security_headers(Some("text/javascript; charset=utf-8"), false);
        assert_eq!(value(&js, "Content-Security-Policy"), None);
        assert_eq!(value(&js, "X-Frame-Options"), Some("DENY"));
        assert_eq!(
            value(&security_headers(None, false), "Strict-Transport-Security"),
            None
        );
        let tls = security_headers(Some("text/html"), true);
        assert_eq!(value(&tls, "Strict-Transport-Security"), Some(HSTS));
    }
}
