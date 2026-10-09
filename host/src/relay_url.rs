//! Relay URL transport rule, shared by config load and every site that turns
//! a relay URL into a socket URL.
//!
//! The host sends bearer credentials in the WebSocket upgrade (and the host
//! token in the first signaling message), so a relay URL must be TLS. The one
//! exception is a relay on this machine, which the Stream Lab uses.

use std::net::IpAddr;

use anyhow::{bail, Context, Result};
use tungstenite::http::Uri;

/// The rule, worded for the operator. Errors carry this text and the URL's
/// scheme only: never the URL itself, a token or a header value.
pub const RELAY_URL_RULE: &str = "the relay URL must use wss:// (or https://); cleartext ws:// \
     and http:// are accepted only for a loopback relay (localhost, 127.0.0.0/8 or [::1])";

/// Refuse a relay URL that would carry host credentials in cleartext to
/// anything but this machine.
pub fn require_secure_relay_url(url: &str) -> Result<()> {
    // Uri normalises HTTP schemes but preserves WebSocket schemes. Reject
    // uppercase at the boundary so every host/tray transport uses one rule.
    if url
        .split_once(':')
        .is_some_and(|(scheme, _)| scheme.bytes().any(|b| b.is_ascii_uppercase()))
    {
        bail!("relay URL scheme must be lowercase; {RELAY_URL_RULE}");
    }
    let uri = Uri::try_from(url)
        .ok()
        .with_context(|| format!("relay URL is not a valid URL; {RELAY_URL_RULE}"))?;
    match uri.scheme_str() {
        Some("wss" | "https") => Ok(()),
        Some("ws" | "http") => {
            if uri.host().is_some_and(is_loopback_host) {
                Ok(())
            } else {
                bail!("refusing a cleartext relay URL for a non-loopback server; {RELAY_URL_RULE}")
            }
        }
        Some(scheme) => bail!("unsupported relay URL scheme {scheme:?}; {RELAY_URL_RULE}"),
        None => bail!("relay URL has no scheme; {RELAY_URL_RULE}"),
    }
}

/// `host` is the URI host: exactly `localhost`, or an IP literal in
/// 127.0.0.0/8 or `::1`. Names that merely contain or resolve to loopback
/// (`localhost.evil.com`, `localhost.`) do not count.
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host)
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_relay_urls_are_accepted_for_any_server() {
        for url in [
            "wss://play.example/host-ws",
            "https://play.example/host-ws",
            "wss://192.168.1.20:8443/host-ws",
        ] {
            require_secure_relay_url(url).unwrap_or_else(|error| panic!("{url}: {error:#}"));
        }
    }

    #[test]
    fn cleartext_relay_urls_are_accepted_only_for_loopback() {
        for url in [
            "ws://localhost:8787/lab-host-ws?run=abc",
            "ws://LOCALHOST/host-ws",
            "ws://127.0.0.1:8787/host-ws",
            "ws://127.8.9.10/host-ws",
            "ws://[::1]:8787/host-ws",
            "http://localhost:8787/host-ws",
            "http://127.0.0.1/host-ws",
        ] {
            require_secure_relay_url(url).unwrap_or_else(|error| panic!("{url}: {error:#}"));
        }
    }

    #[test]
    fn cleartext_relay_urls_to_other_servers_are_rejected() {
        for url in [
            "ws://192.168.1.20:8787/host-ws",
            "ws://10.0.0.5/host-ws",
            "ws://play.example/host-ws",
            "http://play.example/host-ws",
            "ws://localhost.evil.com/host-ws",
            "ws://localhost.:8787/host-ws",
            "ws://127.0.0.1.evil.com/host-ws",
            "ws://localhost@evil.com/host-ws",
            "ws://127.0.0.1@evil.com:8787/host-ws",
            "ws://0.0.0.0:8787/host-ws",
            "ws://[::ffff:192.168.1.20]/host-ws",
            "ws://[fe80::1]/host-ws",
        ] {
            let error = require_secure_relay_url(url).expect_err(url).to_string();
            assert!(error.contains("loopback"), "{url}: {error}");
        }
    }

    #[test]
    fn other_schemes_and_malformed_urls_are_rejected_without_echoing_the_url() {
        for url in [
            "ftp://play.example/host-ws",
            // tungstenite only connects lowercase ws/wss, so other spellings
            // are refused here rather than later.
            "WSS://play.example/host-ws",
            "WS://127.0.0.1/host-ws",
            "HTTPS://play.example/host-ws",
            "Https://play.example/host-ws",
            "Http://127.0.0.1/host-ws",
            "play.example/host-ws",
            "/host-ws",
            "",
            "ws://",
        ] {
            assert!(require_secure_relay_url(url).is_err(), "{url}");
        }
        let error = require_secure_relay_url("ws://secret-user:secret-pass@play.example/host-ws")
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret"), "{error}");
    }
}
