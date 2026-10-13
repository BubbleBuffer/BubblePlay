//! WSS media egress for the Worker media relay (protocol v4 only).
//!
//! One media socket per session carries frames host->client and ACK /
//! keyframe / heartbeat messages client->host. The [`V4Link`] owns the
//! in-flight budget and the delay-based bitrate controller; this module only
//! derives the relay route and adapts the producer's reserve/submit calls.
//! The producer reserves before capture/encode, so a full link is a skip
//! signal, never a reason to wait in the capture path.

use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use crate::audio::AudioFrame;
use crate::wss_v4::{
    V4AudioSubmit, V4Events, V4Link, V4Reserve, V4Stats, V4Submit, V4_HEADER_BYTES,
};
use anyhow::{bail, Context, Result};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::{header::AUTHORIZATION, HeaderName, HeaderValue, Request};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

const STREAM_LAB_COMPATIBILITY_ID: &str = "wss-v4-bench2-hevc-tiles-bpip4-v16";

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

    /// Connect the single media socket for `epoch`, authenticating the HTTP
    /// upgrade with `Authorization: Bearer <bearer>`.
    pub fn connect(
        worker_url: &str,
        bearer: &str,
        epoch: &str,
        bitrate_bps: u64,
        max_bitrate_bps: u64,
    ) -> Result<Self> {
        if !valid_epoch(epoch) {
            bail!("invalid WSS media epoch");
        }
        // Validate the route before the link thread starts retrying it.
        media_request(worker_url, bearer, epoch)?;
        let (url, bearer, epoch) = (worker_url.to_owned(), bearer.to_owned(), epoch.to_owned());
        let link = V4Link::spawn(
            Box::new(move || connect_media_socket(&url, &bearer, &epoch)),
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
    bearer: &str,
    epoch: &str,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let request = media_request(worker_url, bearer, epoch)?;
    let (mut ws, _) = connect(request).context("connect media socket")?;
    set_socket_timeouts(&mut ws, Duration::from_millis(500))?;
    ws.send(Message::text(r#"{"type":"hello","role":"media-host"}"#))
        .context("send media hello")?;
    ws.flush().context("flush media hello")?;
    Ok(ws)
}

fn media_request(worker_url: &str, bearer: &str, epoch: &str) -> Result<Request<()>> {
    host_socket_request(worker_url, bearer, epoch, HostSocket::Media)
}

/// The host's epoch-scoped relay sockets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostSocket {
    Media,
    File,
}

impl HostSocket {
    fn paths(self) -> (&'static str, &'static str) {
        match self {
            Self::Media => ("/host-media-ws", "/lab-host-media-ws"),
            Self::File => ("/host-file-ws", "/lab-host-file-ws"),
        }
    }
}

/// Upgrade request for an epoch-scoped host socket (`/host-media-ws` or
/// `/host-file-ws`, or their lab twins when the Worker URL is a lab route),
/// authenticated with `Authorization: Bearer <bearer>`.
pub fn host_socket_request(
    worker_url: &str,
    bearer: &str,
    epoch: &str,
    kind: HostSocket,
) -> Result<Request<()>> {
    if !valid_epoch(epoch) {
        bail!("invalid relay epoch");
    }
    let worker_name = std::env::var("BUBBLEPLAY_LAB_WORKER_NAME")
        .ok()
        .filter(|value| !value.is_empty());
    let version_id = std::env::var("BUBBLEPLAY_LAB_WORKER_VERSION_ID")
        .ok()
        .filter(|value| !value.is_empty());
    let worker_version = match (worker_name.as_deref(), version_id.as_deref()) {
        (None, None) => None,
        (Some(worker_name), Some(version_id)) => Some((worker_name, version_id)),
        _ => bail!(
            "Stream Lab Worker version override requires both worker name and version identifier"
        ),
    };
    socket_request_with_worker_version(worker_url, bearer, epoch, worker_version, kind)
}

#[cfg(test)]
fn media_request_with_worker_version(
    worker_url: &str,
    bearer: &str,
    epoch: &str,
    worker_version: Option<(&str, &str)>,
) -> Result<Request<()>> {
    socket_request_with_worker_version(worker_url, bearer, epoch, worker_version, HostSocket::Media)
}

fn socket_request_with_worker_version(
    worker_url: &str,
    bearer: &str,
    epoch: &str,
    worker_version: Option<(&str, &str)>,
    kind: HostSocket,
) -> Result<Request<()>> {
    let (production_path, lab_path) = kind.paths();
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
    let lab_route = if parsed.path() == "/lab-host-ws" {
        let query = parsed.query().unwrap_or_default();
        let run = query
            .split('&')
            .find_map(|field| field.strip_prefix("run="))
            .filter(|run| canonical_uuid(run))
            .map(str::to_owned)
            .context("lab Worker URL requires a canonical run id")?;
        let compatibility = query
            .split('&')
            .find_map(|field| field.strip_prefix("compat="))
            .filter(|value| *value == STREAM_LAB_COMPATIBILITY_ID)
            .map(str::to_owned)
            .context("lab Worker URL requires the current compatibility identifier")?;
        Some((run, compatibility))
    } else {
        None
    };
    let media_path = if lab_route.is_some() {
        lab_path
    } else {
        production_path
    };
    let lab_query = lab_route
        .map(|(run, compatibility)| format!("&run={run}&compat={compatibility}"))
        .unwrap_or_default();
    parsed = format!("{scheme}://{authority}{media_path}?epoch={epoch}{lab_query}")
        .parse()
        .expect("constructed media URL is valid");
    let mut request = parsed
        .to_string()
        .into_client_request()
        .context("build media websocket request")?;
    let value = HeaderValue::from_str(&format!("Bearer {bearer}"))
        .context("host bearer is not a valid HTTP header value")?;
    request.headers_mut().insert(AUTHORIZATION, value);
    if parsed.path() == lab_path {
        if let Some((worker_name, version_id)) = worker_version {
            if !canonical_worker_name(worker_name) || !canonical_worker_version_id(version_id) {
                bail!("Stream Lab Worker version override is not canonical");
            }
            let value = HeaderValue::from_str(&format!("{worker_name}=\"{version_id}\""))
                .context("Stream Lab Worker version override is not a valid HTTP header value")?;
            request.headers_mut().insert(
                HeaderName::from_static("cloudflare-workers-version-overrides"),
                value,
            );
        }
    }
    Ok(request)
}

fn canonical_worker_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn canonical_worker_version_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
        && matches!(bytes[14], b'1'..=b'5')
        && matches!(bytes[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b')
}

fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && value
            .as_bytes()
            .iter()
            .enumerate()
            .all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => *byte == b'-',
                14 => matches!(*byte, b'1'..=b'5'),
                19 => matches!(byte.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b'),
                _ => byte.is_ascii_hexdigit(),
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: &str = "0123456789abcdef0123456789abcdef";
    const LAB_URL: &str = "wss://worker.example/lab-host-ws?run=123e4567-e89b-42d3-a456-426614174000&compat=wss-v4-bench2-hevc-tiles-bpip4-v16";

    #[test]
    fn derives_the_single_media_endpoint_and_sends_bearer_only_in_upgrade_header() {
        let request = media_request_with_worker_version(
            "https://worker.example/host-ws",
            "host-bearer",
            EPOCH,
            None,
        )
        .unwrap();
        assert_eq!(
            request.uri().to_string(),
            "wss://worker.example/host-media-ws?epoch=0123456789abcdef0123456789abcdef"
        );
        assert!(!request.uri().to_string().contains("host-bearer"));
        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer host-bearer"
        );
    }

    #[test]
    fn derives_the_file_socket_endpoints() {
        let request = socket_request_with_worker_version(
            "https://worker.example/host-ws",
            "host-bearer",
            EPOCH,
            None,
            HostSocket::File,
        )
        .unwrap();
        assert_eq!(
            request.uri().to_string(),
            "wss://worker.example/host-file-ws?epoch=0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer host-bearer"
        );
        let lab = socket_request_with_worker_version(LAB_URL, "b", EPOCH, None, HostSocket::File)
            .unwrap();
        assert!(lab
            .uri()
            .to_string()
            .starts_with("wss://worker.example/lab-host-file-ws?epoch="));
        assert!(host_socket_request(LAB_URL, "b", "BAD", HostSocket::File).is_err());
    }

    #[test]
    fn derives_run_scoped_lab_media_endpoint_from_lab_signaling_url() {
        let request =
            media_request_with_worker_version(LAB_URL, "host-bearer", EPOCH, None).unwrap();
        assert_eq!(
            request.uri().to_string(),
            "wss://worker.example/lab-host-media-ws?epoch=0123456789abcdef0123456789abcdef&run=123e4567-e89b-42d3-a456-426614174000&compat=wss-v4-bench2-hevc-tiles-bpip4-v16"
        );
        assert!(media_request_with_worker_version(
            "wss://worker.example/lab-host-ws?run=not-a-uuid&compat=wss-v4-bench2-hevc-tiles-bpip4-v16",
            "host-bearer",
            EPOCH,
            None,
        )
        .is_err());
    }

    #[test]
    fn lab_media_worker_version_override_is_header_only_and_never_reaches_production() {
        let version = Some(("playfull-cat", "1a10a2ec-6b6a-4d6a-a7e6-5fd5696360d1"));
        let request =
            media_request_with_worker_version(LAB_URL, "host-bearer", EPOCH, version).unwrap();
        assert_eq!(
            request
                .headers()
                .get("Cloudflare-Workers-Version-Overrides")
                .unwrap(),
            r#"playfull-cat="1a10a2ec-6b6a-4d6a-a7e6-5fd5696360d1""#
        );
        let production = media_request_with_worker_version(
            "wss://worker.example/host-ws",
            "host-bearer",
            EPOCH,
            version,
        )
        .unwrap();
        assert!(production
            .headers()
            .get("Cloudflare-Workers-Version-Overrides")
            .is_none());
    }

    #[test]
    fn host_sockets_refuse_cleartext_except_to_a_loopback_relay() {
        for kind in [HostSocket::Media, HostSocket::File] {
            for url in [
                "ws://192.168.1.20:8787/host-ws",
                "http://worker.example/host-ws",
            ] {
                let error =
                    socket_request_with_worker_version(url, "host-bearer", EPOCH, None, kind)
                        .unwrap_err()
                        .to_string();
                assert!(error.contains("loopback"), "{error}");
                assert!(!error.contains("host-bearer"), "{error}");
            }
        }
        let request = media_request_with_worker_version(
            "http://127.0.0.1:8787/host-ws",
            "host-bearer",
            EPOCH,
            None,
        )
        .unwrap();
        assert_eq!(
            request.uri().to_string(),
            "ws://127.0.0.1:8787/host-media-ws?epoch=0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn rejects_non_canonical_epochs() {
        assert!(valid_epoch(EPOCH));
        assert!(!valid_epoch("0123456789ABCDEF0123456789abcdef"));
        assert!(!valid_epoch("0123"));
    }
}
