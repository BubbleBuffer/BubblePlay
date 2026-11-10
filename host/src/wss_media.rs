//! WSS media egress for the Worker media relay (protocol v4 only).
//!
//! One media socket per session carries frames host->client and ACK /
//! keyframe / heartbeat messages client->host. The [`V4Link`] owns the
//! in-flight budget and the delay-based bitrate controller; this module only
//! derives the relay route and adapts the producer's reserve/submit calls.
//! The producer reserves before capture/encode, so a full link is a skip
//! signal, never a reason to wait in the capture path.

use std::fmt;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use crate::audio::AudioFrame;
use crate::wss_v4::{
    host_websocket_config, V4AudioSubmit, V4Events, V4Link, V4Reserve, V4Stats, V4Submit,
    V4_HEADER_BYTES,
};
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::Deserialize;
use tungstenite::client::{connect_with_config, IntoClientRequest};
use tungstenite::http::Request;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};
use zeroize::{Zeroize, Zeroizing};

/// Data-socket ticket from a keyed host's signaling `hello-ok`: 32 random bytes in unpadded
/// base64url. It authorizes media and file sockets only while its signaling socket stays open.
/// Never printed: `Debug` redacts it, and the buffer is overwritten on drop.
#[derive(Clone, Deserialize)]
#[serde(try_from = "String")]
pub struct RelayTicket(String);

impl RelayTicket {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RelayTicket {
    type Error = &'static str;

    fn try_from(mut value: String) -> Result<Self, Self::Error> {
        let canonical = URL_SAFE_NO_PAD
            .decode(&value)
            .ok()
            .map(Zeroizing::new)
            .filter(|bytes| {
                bytes.len() == 32
                    && Zeroizing::new(URL_SAFE_NO_PAD.encode(bytes.as_slice())).as_str() == value
            });
        match canonical {
            Some(_) => Ok(Self(value)),
            None => {
                value.zeroize();
                Err("relay ticket is not a 32-byte base64url value")
            }
        }
    }
}

impl Drop for RelayTicket {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for RelayTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RelayTicket(<redacted>)")
    }
}

/// Room and relay ticket for one keyed signaling connection. The ticket expires (12 h)
/// independently of the signaling socket that received it.
#[derive(Clone, Debug)]
pub struct RelayRoute {
    pc: String,
    ticket: RelayTicket,
}

impl RelayRoute {
    pub fn keyed(pc: String, ticket: RelayTicket) -> Self {
        Self { pc, ticket }
    }

    pub fn pc(&self) -> &str {
        &self.pc
    }
}

/// The first frame a host data socket sends.
pub(crate) fn data_hello(role: &str, route: &RelayRoute) -> String {
    serde_json::json!({"type": "hello", "role": role, "ticket": route.ticket.as_str()}).to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCodec {
    Hevc,
    H264,
}

impl MediaCodec {
    pub const fn wire(self) -> u8 {
        match self {
            Self::Hevc => 1,
            Self::H264 => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessUnitMeta {
    pub codec: MediaCodec,
    pub generation: u32,
    pub sequence: u32,
    pub timestamp90k: u32,
    pub key: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    Queued,
    DroppedRequireIdr,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveOutcome {
    /// The receiver has no decodable reference; only a keyframe may be sent.
    RequireKeyframe,
    /// The in-flight budget is full. Skip capture/encode and retry later.
    Backpressured,
    Stopped,
}

/// A reservation is consumed by `submit`; dropping it sends nothing.
pub struct MediaReservation {
    meta: AccessUnitMeta,
    link: Arc<V4Link>,
    refine: bool,
}

impl MediaReservation {
    /// Marks the access unit as a static-screen quality refine.
    pub fn set_refine(&mut self, refine: bool) {
        self.refine = refine;
    }
    pub fn set_meta(&mut self, meta: AccessUnitMeta) {
        self.meta = meta;
    }
    pub fn submit(self, bytes: Vec<u8>) -> SubmitOutcome {
        let meta = self.meta;
        match self.link.submit(
            meta.codec.wire(),
            meta.generation,
            meta.sequence,
            meta.timestamp90k,
            meta.key,
            self.refine,
            &bytes,
        ) {
            V4Submit::Queued => SubmitOutcome::Queued,
            V4Submit::DroppedRequireIdr => SubmitOutcome::DroppedRequireIdr,
            V4Submit::Stopped => SubmitOutcome::Stopped,
        }
    }
}

pub struct WssMediaEgress {
    link: Arc<V4Link>,
}

impl WssMediaEgress {
    pub fn set_activity(&self, activity: crate::activity::Activity) {
        self.link.set_activity(activity);
    }

    /// Submit from the capture thread, independently of video producer work.
    pub fn audio_sink(&self) -> Arc<dyn Fn(crate::audio::AudioFrame) + Send + Sync> {
        let link = self.link.clone();
        Arc::new(move |frame| {
            link.submit_audio(frame);
        })
    }

    /// Connect the media socket for `epoch` in the room `route` names. The route, including its
    /// ticket, is kept for every reconnect the link makes, so it must be the signaling connection's
    /// current route when the link starts. A relay that keeps refusing it ends the link with
    /// [`V4LinkEnd::Refused`](crate::wss_v4::V4LinkEnd) instead of looping.
    pub fn connect_routed(
        worker_url: &str,
        epoch: &str,
        bitrate_bps: u64,
        max_bitrate_bps: u64,
        route: &RelayRoute,
    ) -> Result<Self> {
        if !valid_epoch(epoch) {
            bail!("invalid WSS media epoch");
        }
        // Validate the route before the link thread starts retrying it.
        media_request(worker_url, epoch, route)?;
        let (url, epoch_label) = (worker_url.to_owned(), epoch.to_owned());
        let route = route.clone();
        let link = V4Link::spawn_for_epoch(
            Box::new(move || connect_media_socket(&url, &epoch_label, &route)),
            epoch,
            bitrate_bps,
            max_bitrate_bps,
        )?;
        Ok(Self {
            link: Arc::new(link),
        })
    }

    /// Returns a pre-encode reservation. The metadata is updated after the
    /// encoder has classified the access unit, immediately before submit.
    pub fn reserve(&self, meta: AccessUnitMeta) -> Result<MediaReservation, ReserveOutcome> {
        match self.link.reserve(meta.key) {
            V4Reserve::Send => Ok(MediaReservation {
                meta,
                link: self.link.clone(),
                refine: false,
            }),
            V4Reserve::RequireKeyframe => Err(ReserveOutcome::RequireKeyframe),
            V4Reserve::Backpressured => Err(ReserveOutcome::Backpressured),
            V4Reserve::Stopped => Err(ReserveOutcome::Stopped),
        }
    }

    /// Drain link events (keyframe requests, bitrate changes, ACKs).
    pub fn take_events(&self) -> V4Events {
        self.link.take_events()
    }

    pub fn stats(&self) -> V4Stats {
        self.link.stats()
    }

    pub fn set_bitrate_bounds(&self, bitrate_bps: u64, max_bitrate_bps: u64) {
        self.link.set_bitrate_bounds(bitrate_bps, max_bitrate_bps);
    }

    /// Measured capacity of a constrained link; `None` when unconstrained.
    pub fn capacity(&self) -> Option<crate::link_capacity::Capacity> {
        self.link.capacity()
    }

    pub fn set_frame_size(&self, width: u32, height: u32) {
        self.link.set_frame_size(width, height);
    }

    pub fn set_frame_rate(&self, fps: u32) {
        self.link.set_frame_rate(fps);
    }

    /// Opt the BPA1 audio lane in (`audio:1` from the viewer's start) or out.
    pub fn set_audio_enabled(&self, enabled: bool) {
        self.link.set_audio_enabled(enabled);
    }

    /// Queue one audio frame; it never waits on or affects video admission.
    pub fn submit_audio(&self, frame: AudioFrame) -> V4AudioSubmit {
        self.link.submit_audio(frame)
    }

    pub fn wire_payload_bytes(&self, access_unit_bytes: usize) -> u64 {
        (V4_HEADER_BYTES + access_unit_bytes) as u64
    }

    /// The host moved off this egress's epoch; see [`V4Link::retire_epoch`].
    pub fn retire_epoch(&self) {
        self.link.retire_epoch();
    }

    pub fn shutdown(self) {
        self.link.shutdown();
    }
}

fn valid_epoch(epoch: &str) -> bool {
    epoch.len() == 32
        && epoch
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn set_socket_timeouts(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Duration,
) -> Result<()> {
    let tcp = match ws.get_mut() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::NativeTls(stream) => stream.get_ref(),
        other => bail!("unsupported TLS stream variant: {other:?}"),
    };
    tcp.set_write_timeout(Some(timeout))
        .context("set media socket write timeout")?;
    tcp.set_nodelay(true)
        .context("enable TCP_NODELAY for media socket")?;
    Ok(())
}

fn connect_media_socket(
    worker_url: &str,
    epoch: &str,
    route: &RelayRoute,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let request = media_request(worker_url, epoch, route)?;
    let (mut ws, _) = connect_with_config(request, Some(host_websocket_config()), 3)
        .context("connect media socket")?;
    set_socket_timeouts(&mut ws, Duration::from_millis(500))?;
    ws.send(Message::text(data_hello("media-host", route)))
        .context("send media hello")?;
    ws.flush().context("flush media hello")?;
    Ok(ws)
}

fn media_request(worker_url: &str, epoch: &str, route: &RelayRoute) -> Result<Request<()>> {
    host_socket_request_routed(worker_url, epoch, HostSocket::Media, route)
}

/// The host's epoch-scoped relay sockets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostSocket {
    Media,
    File,
}

impl HostSocket {
    fn path(self) -> &'static str {
        match self {
            Self::Media => "/host-media-ws",
            Self::File => "/host-file-ws",
        }
    }
}

/// Upgrade request for an epoch-scoped host socket (`/host-media-ws` or `/host-file-ws`). The URL
/// names the room `route` holds as `pc=<id>` after `epoch`; the ticket travels only in the hello.
pub fn host_socket_request_routed(
    worker_url: &str,
    epoch: &str,
    kind: HostSocket,
    route: &RelayRoute,
) -> Result<Request<()>> {
    if !valid_epoch(epoch) {
        bail!("invalid relay epoch");
    }
    socket_request(worker_url, epoch, kind, route)
}

fn socket_request(
    worker_url: &str,
    epoch: &str,
    kind: HostSocket,
    route: &RelayRoute,
) -> Result<Request<()>> {
    crate::relay_url::require_secure_relay_url(worker_url)?;
    let mut url = tungstenite::http::Uri::try_from(worker_url)
        .context("invalid Worker URL")?
        .to_string();
    if let Some(rest) = url.strip_prefix("https://") {
        url = format!("wss://{rest}");
    } else if let Some(rest) = url.strip_prefix("http://") {
        url = format!("ws://{rest}");
    }
    let mut parsed: tungstenite::http::Uri = url.parse().context("invalid WebSocket Worker URL")?;
    let authority = parsed
        .authority()
        .context("Worker URL has no authority")?
        .as_str()
        .to_owned();
    let scheme = parsed.scheme_str().unwrap_or("wss").to_owned();
    let media_path = kind.path();
    let pc = route.pc();
    parsed = format!("{scheme}://{authority}{media_path}?epoch={epoch}&pc={pc}")
        .parse()
        .expect("constructed media URL is valid");
    parsed
        .to_string()
        .into_client_request()
        .context("build media websocket request")
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: &str = "0123456789abcdef0123456789abcdef";
    const TICKET: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const PC: &str = "ABCD-EFGH-JKMN-PQRS";

    fn keyed_route() -> RelayRoute {
        RelayRoute::keyed(
            PC.to_owned(),
            RelayTicket::try_from(TICKET.to_owned()).unwrap(),
        )
    }

    #[test]
    fn keyed_media_socket_addresses_the_pc_and_sends_the_ticket_in_hello_only() {
        let route = keyed_route();
        let request = socket_request(
            "https://worker.example/host-ws",
            EPOCH,
            HostSocket::Media,
            &route,
        )
        .unwrap();
        assert_eq!(
            request.uri().to_string(),
            format!("wss://worker.example/host-media-ws?epoch={EPOCH}&pc={PC}")
        );
        assert!(!request.uri().to_string().contains(TICKET));
        assert!(!format!("{:?}", request.headers()).contains(TICKET));
        assert!(request.headers().get("authorization").is_none());
        let hello: serde_json::Value =
            serde_json::from_str(&data_hello("media-host", &route)).unwrap();
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["role"], "media-host");
        assert_eq!(hello["ticket"], TICKET);
    }

    #[test]
    fn route_debug_never_prints_the_ticket() {
        let debug = format!("{:?}", keyed_route());
        assert!(debug.contains("RelayTicket(<redacted>)"), "{debug}");
        assert!(!debug.contains(TICKET), "{debug}");
    }

    #[test]
    fn relay_ticket_rejects_non_canonical_values_and_never_prints() {
        assert!(RelayTicket::try_from("short".to_owned()).is_err());
        assert!(RelayTicket::try_from(format!("{TICKET}=")).is_err());
        let ticket = RelayTicket::try_from(TICKET.to_owned()).unwrap();
        let debug = format!("{ticket:?}");
        assert_eq!(debug, "RelayTicket(<redacted>)");
        assert!(!debug.contains("AAAA"));
        let parsed: RelayTicket = serde_json::from_str(&format!("\"{TICKET}\"")).unwrap();
        assert_eq!(parsed.as_str(), TICKET);
        assert!(serde_json::from_str::<RelayTicket>("\"not-a-ticket\"").is_err());
    }

    #[test]
    fn media_and_file_sockets_address_the_pc_and_send_the_ticket_in_hello() {
        for kind in [HostSocket::Media, HostSocket::File] {
            let path = kind.path();
            let role = if matches!(kind, HostSocket::Media) {
                "media-host"
            } else {
                "file-host"
            };
            let route = keyed_route();
            let request =
                socket_request("wss://worker.example/host-ws", EPOCH, kind, &route).unwrap();
            assert_eq!(
                request.uri().to_string(),
                format!("wss://worker.example{path}?epoch={EPOCH}&pc={PC}")
            );
            assert!(!request.uri().to_string().contains(TICKET));
            assert!(!format!("{:?}", request.headers()).contains(TICKET));
            let hello: serde_json::Value = serde_json::from_str(&data_hello(role, &route)).unwrap();
            assert_eq!(hello["role"], role);
            assert_eq!(hello["ticket"], TICKET);
        }
    }

    #[test]
    fn host_sockets_refuse_cleartext_except_to_a_loopback_relay() {
        for kind in [HostSocket::Media, HostSocket::File] {
            for url in [
                "ws://192.168.1.20:8787/host-ws",
                "http://worker.example/host-ws",
            ] {
                let error = socket_request(url, EPOCH, kind, &keyed_route())
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("loopback"), "{error}");
                assert!(!error.contains(TICKET), "{error}");
            }
        }
        let request =
            media_request("http://127.0.0.1:8787/host-ws", EPOCH, &keyed_route()).unwrap();
        assert_eq!(
            request.uri().to_string(),
            format!("ws://127.0.0.1:8787/host-media-ws?epoch={EPOCH}&pc={PC}")
        );
    }

    #[test]
    fn rejects_non_canonical_epochs() {
        assert!(valid_epoch(EPOCH));
        assert!(!valid_epoch("0123456789ABCDEF0123456789abcdef"));
        assert!(!valid_epoch("0123"));
    }
}
