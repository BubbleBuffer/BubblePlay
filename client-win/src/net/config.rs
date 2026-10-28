//! Session configuration and relay endpoint rules.

use std::sync::Arc;

use bp_device_key::{DeviceId, DeviceIdentity};

use super::controls::SessionControls;
use super::error::NetError;

/// Who connects and to which PC: the unlocked viewer key that signs the relay proof, the
/// PC ID that goes into `/ws?pc=` and `/media-ws?pc=`, and the PC keys pinned in `pcs.json`.
#[derive(Clone)]
pub struct ViewerAuth {
    identity: Arc<DeviceIdentity>,
    pc: Option<DeviceId>,
    paired: Arc<Vec<(DeviceId, [u8; 32])>>,
}

impl ViewerAuth {
    /// `pc` is `None` only for the loopback fake host, which serves a single PC.
    pub fn new(identity: DeviceIdentity, pc: Option<DeviceId>) -> Self {
        Self {
            identity: Arc::new(identity),
            pc,
            paired: Arc::new(Vec::new()),
        }
    }

    /// The paired PCs and their pinned public keys (`pcs.json`). The handshake refuses any
    /// other PC, and any PC whose key differs from its pinned key.
    pub fn with_paired(mut self, paired: Vec<(DeviceId, [u8; 32])>) -> Self {
        self.paired = Arc::new(paired);
        self
    }

    pub fn paired(&self) -> &[(DeviceId, [u8; 32])] {
        &self.paired
    }

    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    pub fn pc(&self) -> Option<DeviceId> {
        self.pc
    }

    /// The fixture viewer key (fixtures/device-keys-contract.json `ids[0]`) for PC
    /// `77VH-7M56-8GJK-Y12J`, paired with the fixture PC key (`handshake.pc`). Test only.
    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        Self::new(
            DeviceIdentity::from_seed(&seed),
            Some(DeviceId::parse("77VH-7M56-8GJK-Y12J").expect("fixture PC ID")),
        )
        .with_paired(vec![super::phase2::test_pc::fixture_pc()])
    }
}

impl std::fmt::Debug for ViewerAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewerAuth")
            .field("viewer", &self.identity.id())
            .field("pc", &self.pc)
            .field(
                "paired",
                &self.paired.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Development and session transport choice. Auto remembers an RTC failure
/// within this session, matching the browser's tab-scoped fallback memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TransportChoice {
    Auto,
    WebRtc,
    #[default]
    Wss,
}

/// Deliberate native deviation: receiver.js keeps waiting while ICE checks.
/// Auto rescues a stuck checking peer ten seconds after transmitting its offer.
/// The dev CLI can override this through `--ice-timeout-ms`.
pub const AUTO_ICE_CHECKING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
    /// The viewer key and PC for the signed `/ws` hello.
    pub viewer: ViewerAuth,
    /// Optional native input mailbox, scoped to each /ws epoch.
    pub input: Option<super::InputLink>,
    /// Start mode sent in `signaling-start`. Kept in step with live mode changes from `controls`.
    pub stream_mode: Option<String>,
    /// Live monitor and mode controls. Clone it into the UI; the net thread reads and writes the same state.
    pub controls: SessionControls,
    /// Independent bounded audio lane; absent means this receiver cannot play Opus.
    pub audio: Option<crate::audio::AudioHandle>,
    /// Disabled only by the decode-free audio development harness.
    pub video_watchdog: bool,
    pub transport: TransportChoice,
    pub ice_checking_timeout: std::time::Duration,
    /// Headless qualification may authorize synthetic input after MF decode;
    /// it never reports a presentation or sends a false presentation proof.
    pub headless_decoded_ready: bool,
}

impl NetConfig {
    pub fn new(relay: impl Into<String>, codec: Codec, viewer: ViewerAuth) -> Self {
        Self {
            relay: relay.into(),
            codec,
            viewer,
            input: None,
            stream_mode: None,
            controls: SessionControls::new(),
            audio: None,
            video_watchdog: true,
            transport: TransportChoice::Wss,
            ice_checking_timeout: AUTO_ICE_CHECKING_TIMEOUT,
            headless_decoded_ready: false,
        }
    }
}

/// A parsed relay origin. Only `https`, or `http` to a loopback host (`127.0.0.1`, `::1`,
/// `localhost`).
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
        let plaintext_allowed = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
            Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
            Some(url::Host::Domain(host)) => host == "localhost",
            None => false,
        };
        if !secure && !plaintext_allowed {
            return Err(NetError::PlaintextRefused(input.to_owned()));
        }
        Ok(Self {
            origin: url.origin().ascii_serialization(),
            authority: url.authority().to_owned(),
            secure,
        })
    }

    /// Scheme, host and non-default port. The relay proof signs this form (no `Origin`
    /// header is ever sent: a native socket is judged by its signed hello alone).
    pub fn origin(&self) -> &str {
        &self.origin
    }

    #[cfg(test)]
    pub fn is_secure(&self) -> bool {
        self.secure
    }

    /// A WebSocket URL on this relay, for example `wss://relay.example/ws`.
    pub fn ws_url(&self, path: &str, query: Option<&str>) -> String {
        let scheme = if self.secure { "wss" } else { "ws" };
        match query {
            Some(query) => format!("{scheme}://{}{path}?{query}", self.authority),
            None => format!("{scheme}://{}{path}", self.authority),
        }
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
        for url in ["http://localhost:8787", "http://[::1]:8787"] {
            assert!(RelayEndpoint::parse(url).is_ok(), "{url} is loopback");
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
