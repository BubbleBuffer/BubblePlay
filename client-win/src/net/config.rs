//! Session configuration and relay endpoint rules.

use super::cookies::CookieJar;
use super::error::NetError;

/// Video codec for `media-control start` and the BPV4 codec byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    /// The name the relay and host use in `media-control start`.
    pub fn wire_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
        }
    }

    /// The BPV4 header codec byte: 1 HEVC, 2 H.264.
    pub fn packet_code(self) -> u8 {
        match self {
            Codec::Hevc => 1,
            Codec::H264 => 2,
        }
    }

    /// Parse the BPV4 header codec byte; anything but 1 or 2 is `None`.
    pub fn from_packet_code(code: u8) -> Option<Codec> {
        match code {
            1 => Some(Codec::Hevc),
            2 => Some(Codec::H264),
            _ => None,
        }
    }
}

/// Everything `run` needs to start a session.
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// Relay origin, for example `https://relay.example` or `http://127.0.0.1:8787`.
    pub relay: String,
    pub codec: Codec,
    /// Session cookies from [`crate::net::login`] (or a retained jar).
    pub cookies: CookieJar,
    /// Optional native input mailbox, scoped to each /ws epoch.
    pub input: Option<super::InputLink>,
    pub stream_mode: Option<String>,
}

impl NetConfig {
    pub fn new(relay: impl Into<String>, codec: Codec, cookies: CookieJar) -> Self {
        Self {
            relay: relay.into(),
            codec,
            cookies,
            input: None,
            stream_mode: None,
        }
    }
}

/// A parsed relay origin. Only `https`, or `http` to the IP literal `127.0.0.1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayEndpoint {
    origin: String,
    authority: String,
    secure: bool,
}

impl RelayEndpoint {
    pub fn parse(input: &str) -> Result<Self, NetError> {
        let trimmed = input.trim();
        let mut url =
            url::Url::parse(trimmed).map_err(|_| NetError::InvalidRelayUrl(input.to_owned()))?;
        match url.scheme() {
            "ws" => {
                url.set_scheme("http")
                    .map_err(|_| NetError::InvalidRelayUrl(input.to_owned()))?;
            }
            "wss" => {
                url.set_scheme("https")
                    .map_err(|_| NetError::InvalidRelayUrl(input.to_owned()))?;
            }
            _ => {}
        }
        let secure = match url.scheme() {
            "https" => true,
            "http" => false,
            _ => return Err(NetError::InvalidRelayUrl(input.to_owned())),
        };
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || (url.path() != "/" && !url.path().is_empty())
        {
            return Err(NetError::InvalidRelayUrl(input.to_owned()));
        }
        let plaintext_allowed = url.host_str() == Some("127.0.0.1");
        #[cfg(feature = "dev-login")]
        let plaintext_allowed = plaintext_allowed
            || matches!(url.host(), Some(url::Host::Domain("localhost")))
            || matches!(url.host(),Some(url::Host::Ipv6(ip)) if ip==std::net::Ipv6Addr::LOCALHOST);
        if !secure && !plaintext_allowed {
            return Err(NetError::PlaintextRefused(input.to_owned()));
        }
        Ok(Self {
            origin: url.origin().ascii_serialization(),
            authority: url.authority().to_owned(),
            secure,
        })
    }

    /// The `Origin` header and login origin: scheme, host and non-default port.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn is_secure(&self) -> bool {
        self.secure
    }

    /// Loopback endpoints, including TLS, never use a system proxy.
    pub fn is_loopback(&self) -> bool {
        url::Url::parse(&self.origin).is_ok_and(|url| match url.host() {
            Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
            Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
            Some(url::Host::Domain(host)) => host == "localhost",
            _ => false,
        })
    }

    /// A WebSocket URL on this relay, for example `wss://relay.example/ws`.
    pub fn ws_url(&self, path: &str, query: Option<&str>) -> String {
        let scheme = if self.secure { "wss" } else { "ws" };
        match query {
            Some(query) => format!("{scheme}://{}{path}?{query}", self.authority),
            None => format!("{scheme}://{}{path}", self.authority),
        }
    }

    /// The HTTP URL for a path on this relay.
    pub fn http_url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_origin_drops_default_port_and_lowercases_host() {
        let endpoint = RelayEndpoint::parse("https://Relay.Example:443/").unwrap();
        assert_eq!(endpoint.origin(), "https://relay.example");
        assert_eq!(endpoint.ws_url("/ws", None), "wss://relay.example/ws");
        assert!(endpoint.is_secure());
    }

    #[test]
    fn loopback_plaintext_is_allowed_and_keeps_port() {
        let endpoint = RelayEndpoint::parse("http://127.0.0.1:8787").unwrap();
        assert_eq!(endpoint.origin(), "http://127.0.0.1:8787");
        assert_eq!(
            endpoint.ws_url("/media-ws", Some("epoch=0123456789abcdef0123456789abcdef")),
            "ws://127.0.0.1:8787/media-ws?epoch=0123456789abcdef0123456789abcdef"
        );
        assert!(endpoint.is_loopback());
    }

    #[test]
    fn plaintext_to_any_other_host_is_refused() {
        let refused = [
            "http://127.0.0.2",
            "http://192.168.1.10",
            "http://127.0.0.1.example.test",
            "http://relay.example",
        ];
        for url in refused {
            assert!(
                matches!(
                    RelayEndpoint::parse(url),
                    Err(NetError::PlaintextRefused(_))
                ),
                "{url} must be refused as plaintext"
            );
        }
        #[cfg(not(feature = "dev-login"))]
        for url in ["http://localhost:8787", "http://[::1]:8787"] {
            assert!(matches!(
                RelayEndpoint::parse(url),
                Err(NetError::PlaintextRefused(_))
            ));
        }
        #[cfg(feature = "dev-login")]
        for url in ["http://localhost:8787", "http://[::1]:8787"] {
            assert!(RelayEndpoint::parse(url).is_ok());
        }
    }

    #[test]
    fn non_origin_urls_and_other_schemes_are_refused() {
        for url in [
            "ftp://relay.example",
            "https://relay.example/path",
            "https://relay.example/?q=1",
            "https://relay.example/#frag",
            "https://user:pw@relay.example",
            "not a url",
            "",
        ] {
            assert!(
                matches!(RelayEndpoint::parse(url), Err(NetError::InvalidRelayUrl(_))),
                "{url:?} must be an invalid relay URL"
            );
        }
    }

    #[test]
    fn trailing_slash_and_whitespace_are_accepted() {
        assert_eq!(
            RelayEndpoint::parse("  https://relay.example/  ")
                .unwrap()
                .origin(),
            "https://relay.example"
        );
    }

    #[test]
    fn codec_names_and_packet_codes_match_the_contract() {
        assert_eq!(Codec::Hevc.wire_name(), "hevc");
        assert_eq!(Codec::H264.wire_name(), "h264");
        assert_eq!(Codec::from_packet_code(1), Some(Codec::Hevc));
        assert_eq!(Codec::from_packet_code(2), Some(Codec::H264));
        for refused in [0u8, 3, 255] {
            assert_eq!(Codec::from_packet_code(refused), None);
        }
        assert_eq!(Codec::Hevc.packet_code(), 1);
        assert_eq!(Codec::H264.packet_code(), 2);
    }
}
