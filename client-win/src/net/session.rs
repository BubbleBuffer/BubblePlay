//! The WSS v4 session: `/ws` signaling, `/media-ws` BPV4 frames, the receive
//! gate, heartbeats, the first-frame watchdog and whole-session reconnects.
//! Ports the WSS paths of `worker/public/receiver.js` and `media-v4.js`.
//!
//! Everything runs on one current-thread tokio runtime owned by [`run`]. Frames
//! and resets leave through a bounded std channel with `try_send`, so the socket
//! loop never blocks on the decoder.

use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};

use super::config::{Codec, NetConfig, RelayEndpoint};
use super::controls::{stream_mode_wire, SessionControls};
use super::error::NetError;
use super::events::{DecoderFeedback, NetEvent, ResetEvent, VideoFrame};
use super::gate::{next_backoff, GateVerdict, KeyframeThrottle, ReceiveGate};
use super::protocol::{
    ack_message, first_frame_keyframe, first_frame_presented, first_frame_request, is_audio_packet,
    keyframe_message, media_hello, mode_control_message, monitor_control_message, new_request_id,
    parse_frame, parse_monitor_error, parse_monitor_list, parse_register_challenge,
    parse_relay_control, ping_message, register_proof, sanitize_reason, start_message, valid_epoch,
    validate_host_control, viewer_hello, RelayControl, V4_HEADER_BYTES,
};
use super::stats::{count, NetState, TransportStats};
use super::{
    webrtc::{auto::Action, Peer},
    TransportChoice,
};
use str0m::{Event as RtcEvent, IceConnectionState};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Sink = SplitSink<Socket, Message>;
type Source = SplitStream<Socket>;

// Keep shutdown independent of handshake/dead timeouts. A stopped host may
// still have a few frames queued on the relay's separate media connection.
const SHUTDOWN_QUIET: Duration = Duration::from_millis(100);
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(1);
const SHUTDOWN_CLOSE: Duration = Duration::from_millis(200);

/// Intervals that drive the session. [`Timing::default`] holds the production
/// values from `net-api.md`; tests shorten them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// How often the loop wakes to drain feedback, flush resets and check the stop flag.
    pub tick: Duration,
    /// Limit for each handshake step (`hello-ok`, the WebSocket upgrade).
    pub handshake: Duration,
    /// Quiet time on a socket before a `ping` is sent.
    pub heartbeat: Duration,
    /// Silence on a socket before it is declared dead.
    pub dead: Duration,
    /// Wait for the first decoded frame, per attempt (the watchdog runs twice).
    pub first_frame: Duration,
    /// Minimum gap between keyframe requests.
    pub keyframe_interval: Duration,
    /// First reconnect delay; doubles up to `backoff_max`.
    pub backoff_min: Duration,
    pub backoff_max: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            tick: Duration::from_millis(25),
            handshake: Duration::from_secs(10),
            heartbeat: Duration::from_secs(15),
            dead: Duration::from_secs(40),
            first_frame: Duration::from_secs(5),
            keyframe_interval: Duration::from_millis(250),
            backoff_min: Duration::from_millis(250),
            backoff_max: Duration::from_secs(5),
        }
    }
}

/// Run the WSS v4 receive session on the calling thread. Returns `Ok(())` once
/// `stop` is set or the `events` receiver is dropped. Returns `Err` only for fatal
/// errors; transient failures reconnect with a new `/ws` epoch.
pub fn run(
    config: NetConfig,
    events: SyncSender<NetEvent>,
    feedback: Receiver<DecoderFeedback>,
    stop: Arc<AtomicBool>,
    stats: Arc<TransportStats>,
) -> Result<(), NetError> {
    run_with_timing(config, events, feedback, stop, stats, Timing::default())
}

pub(crate) fn run_with_timing(
    config: NetConfig,
    events: SyncSender<NetEvent>,
    feedback: Receiver<DecoderFeedback>,
    stop: Arc<AtomicBool>,
    stats: Arc<TransportStats>,
    timing: Timing,
) -> Result<(), NetError> {
    let endpoint = match RelayEndpoint::parse(&config.relay) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            stats.record_error(error.to_string());
            stats.set_state(NetState::Stopped);
            return Err(error);
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| NetError::Socket(error.to_string()))?;
    // The configured start mode seeds the live controls, unless a mode was already set on them.
    config.controls.seed_mode(config.stream_mode.as_deref());
    let mut session = Session::new(config, events, feedback, stop, stats, timing);
    runtime.block_on(session.run_loop(endpoint))
}

/// Why one attempt ended. `Stop` is a clean end; `Transient` reconnects; `Fatal` returns.
#[derive(Debug)]
enum Halt {
    Stop,
    Fatal(NetError),
    Transient(NetError),
}

impl From<NetError> for Halt {
    fn from(error: NetError) -> Self {
        if error.is_fatal() {
            Halt::Fatal(error)
        } else {
            Halt::Transient(error)
        }
    }
}

/// One message from the relay, after the WebSocket layer has taken its frame.
enum Inbound {
    Text(String),
    Binary(Vec<u8>),
    Other,
}

enum Wake {
    Media(Option<Result<Message, tungstenite::Error>>),
    Signal(Option<Result<Message, tungstenite::Error>>),
    Tick,
    Input,
    Udp(Vec<u8>, std::net::SocketAddr),
}

/// Per-socket send and receive times, for heartbeats and dead detection.
struct Lane {
    tx: Sink,
    last_sent: Instant,
    last_seen: Instant,
}

impl Lane {
    fn new(tx: Sink) -> Self {
        let now = Instant::now();
        Self {
            tx,
            last_sent: now,
            last_seen: now,
        }
    }

    async fn send(&mut self, text: String) -> Result<(), Halt> {
        self.tx
            .send(Message::Text(text.into()))
            .await
            .map_err(|error| Halt::from(NetError::Socket(error.to_string())))?;
        self.last_sent = Instant::now();
        Ok(())
    }
    async fn send_binary(&mut self, bytes: Vec<u8>) -> Result<(), Halt> {
        self.tx
            .send(Message::Binary(bytes.into()))
            .await
            .map_err(|error| Halt::from(NetError::Socket(error.to_string())))?;
        self.last_sent = Instant::now();
        Ok(())
    }
}

/// State of one `/ws` epoch: the wire epoch, the first-frame handshake and the
/// `host-joined` tracking. Replaced on every reconnect.
struct Epoch {
    wire: String,
    /// The `/media-ws` ticket from this epoch's `hello-ok`.
    ticket: String,
    request_id: String,
    codec: Codec,
    host_present: bool,
    presented: bool,
    first_frame_deadline: Option<Instant>,
    first_frame_retried: bool,
    first_frame_pending: bool,
    input_generation: Option<u32>,
    desktop_available: bool,
    desktop_enabled: bool,
    desktop_generation: Option<u64>,
    rtc_generation: u32,
    rtc_require_admission: bool,
    rtc_admitted_timestamp: Option<u32>,
}

impl Epoch {
    fn regate_desktop(&mut self, generation: Option<u64>, timeout: Duration) -> bool {
        let changed = generation.is_some() && generation != self.desktop_generation;
        // Reuse only a handshake in flight, matching desktopStateNeedsNewHandshake.
        let restart = changed
            || self.presented
            || !self.desktop_available
            || (self.desktop_available && self.first_frame_deadline.is_none());
        if restart {
            self.input_generation = None;
            self.presented = false;
            self.request_id = new_request_id();
            self.first_frame_retried = false;
            self.first_frame_pending = self.desktop_available;
            self.first_frame_deadline = self.desktop_available.then(|| Instant::now() + timeout);
        }
        if generation.is_some() {
            self.desktop_generation = generation;
        }
        restart
    }
}

struct Link {
    signal: Lane,
    media: Option<Lane>,
    rtc: Option<Peer>,
    epoch: Epoch,
}

struct Session {
    config: NetConfig,
    timing: Timing,
    events: SyncSender<NetEvent>,
    feedback: Receiver<DecoderFeedback>,
    feedback_open: bool,
    stop: Arc<AtomicBool>,
    stats: Arc<TransportStats>,
    started: Instant,
    backoff: Duration,
    ever_opened: bool,
    throttle: KeyframeThrottle,
    /// Per-run counter; bumped for every new `/ws` epoch.
    local_epoch: u64,
    gate: ReceiveGate,
    gate_generation: Option<u32>,
    decoded_seq: Option<u32>,
    /// A reset the decoder has not received yet. Delivered before any later frame.
    pending_reset: Option<String>,
    /// A keyframe request not yet sent (throttled).
    pending_keyframe: Option<String>,
    remembered_rtc_failure: bool,
    rtp_ingress: super::webrtc::ingress::Ingress,
}

impl Session {
    fn new(
        config: NetConfig,
        events: SyncSender<NetEvent>,
        feedback: Receiver<DecoderFeedback>,
        stop: Arc<AtomicBool>,
        stats: Arc<TransportStats>,
        timing: Timing,
    ) -> Self {
        Self {
            config,
            timing,
            events,
            feedback,
            feedback_open: true,
            stop,
            stats,
            started: Instant::now(),
            backoff: Duration::ZERO,
            ever_opened: false,
            throttle: KeyframeThrottle::new(timing.keyframe_interval),
            local_epoch: 0,
            gate: ReceiveGate::new(),
            gate_generation: None,
            decoded_seq: None,
            pending_reset: None,
            pending_keyframe: None,
            remembered_rtc_failure: false,
            rtp_ingress: super::webrtc::ingress::Ingress::new(Instant::now()),
        }
    }

    fn stop_requested(&self) -> bool {
        self.stop.load(Relaxed)
    }

    async fn wait_stop(stop: Arc<AtomicBool>, tick: Duration) {
        while !stop.load(Relaxed) {
            sleep(tick).await;
        }
    }

    /// Race one handshake step against its time limit and the stop flag.
    async fn handshake<T>(
        &self,
        future: impl Future<Output = Result<T, NetError>>,
    ) -> Result<T, Halt> {
        tokio::select! {
            result = timeout(self.timing.handshake, future) => {
                let value = result.unwrap_or_else(|_| Err(NetError::Handshake("timed out".to_owned())));
                Ok(value?)
            }
            _ = Self::wait_stop(self.stop.clone(), self.timing.tick) => Err(Halt::Stop),
        }
    }

    /// Sleep for `delay` in ticks. Returns true when stop was requested first.
    async fn pause(&self, delay: Duration) -> bool {
        let until = Instant::now() + delay;
        loop {
            if self.stop_requested() {
                return true;
            }
            let now = Instant::now();
            if now >= until {
                return false;
            }
            sleep(self.timing.tick.min(until - now)).await;
        }
    }

    async fn run_loop(&mut self, endpoint: RelayEndpoint) -> Result<(), NetError> {
        loop {
            let attempt = self.attempt(&endpoint).await;
            // Pending monitor picks and queued mode targets end with the socket they were sent on.
            self.config.controls.epoch_restart();
            // Clear audio before backoff, including failures on either socket.
            if let Some(audio) = &self.config.audio {
                audio.reset_epoch(self.local_epoch);
            }
            match attempt {
                Err(Halt::Stop) => break,
                Err(Halt::Fatal(error)) => {
                    self.stats.record_error(error.to_string());
                    self.stats.set_state(NetState::Stopped);
                    return Err(error);
                }
                Err(Halt::Transient(error)) => {
                    self.stats.record_error(error.to_string());
                    self.stats.set_state(NetState::Backoff);
                    let delay = next_backoff(
                        self.backoff,
                        self.timing.backoff_min,
                        self.timing.backoff_max,
                    );
                    self.backoff = delay;
                    if self.pause(delay).await {
                        break;
                    }
                }
                Ok(()) => break,
            }
        }
        self.stats.set_state(NetState::Stopped);
        Ok(())
    }

    /// Start a new `/ws` epoch. Frames from older epochs are stale from here on.
    fn begin_epoch(&mut self) {
        self.rtp_ingress = super::webrtc::ingress::Ingress::new(Instant::now());
        self.local_epoch += 1;
        if let Some(audio) = &self.config.audio {
            audio.reset_epoch(self.local_epoch);
        }
        if let Some(input) = &self.config.input {
            input.begin_epoch(self.local_epoch);
        }
        self.stats.local_epoch.store(self.local_epoch, Relaxed);
        count(&self.stats.attempts);
        let reason = if self.ever_opened {
            "reconnect"
        } else {
            "connect"
        };
        self.gate.reset(reason);
        self.pending_reset = Some(reason.to_owned());
        self.pending_keyframe = None;
        self.gate_generation = None;
        self.decoded_seq = None;
        self.stats.set_state(NetState::Signaling);
    }

    /// The `pc=<id>` query of both sockets, when a PC is chosen.
    fn pc_query(&self) -> Option<String> {
        self.config.viewer.pc().map(|pc| format!("pc={pc}"))
    }

    /// One full attempt: signed `/ws` hello and proof, start and first-frame request,
    /// `/media-ws` ticket hello, then the stream loop. Always ends with `Err`.
    async fn attempt(&mut self, endpoint: &RelayEndpoint) -> Result<(), Halt> {
        self.begin_epoch();
        let query = self.pc_query();
        let socket = self
            .handshake(open_socket(endpoint, "/ws", query.as_deref()))
            .await?;
        let route_ip = match socket.get_ref() {
            MaybeTlsStream::Plain(tcp) => tcp.local_addr().ok().map(|addr| addr.ip()),
            MaybeTlsStream::Rustls(tls) => tls.get_ref().0.local_addr().ok().map(|addr| addr.ip()),
            _ => None,
        };
        let (tx, mut signal_rx) = socket.split();
        let mut signal = Lane::new(tx);
        // A cheap clone (the key sits behind an Arc): `handshake` borrows `self` mutably.
        let viewer = self.config.viewer.clone();
        signal.send(viewer_hello(viewer.identity())).await?;
        let nonce = self
            .handshake(await_register_challenge(&mut signal_rx, &self.stats))
            .await?;
        let proof_origin = bp_device_key::relay_origin(endpoint.origin())
            .unwrap_or_else(|| endpoint.origin().to_owned());
        signal
            .send(register_proof(viewer.identity(), &proof_origin, &nonce))
            .await?;
        let (epoch, ticket, ice_servers) = self
            .handshake(await_hello_ok(&mut signal_rx, &self.stats))
            .await?;
        let wire = epoch.filter(|epoch| valid_epoch(epoch)).ok_or_else(|| {
            Halt::from(NetError::Protocol(
                "hello-ok without a valid mediaEpoch".into(),
            ))
        })?;
        let ticket = ticket.ok_or_else(|| {
            Halt::from(NetError::Protocol("hello-ok without a media ticket".into()))
        })?;
        self.config.controls.epoch_registered(&wire);
        // A live mode the user chose (also before connect) is the start mode on reconnect.
        if let Some(mode) = self.config.controls.configured_mode() {
            self.config.stream_mode = Some(stream_mode_wire(mode).to_owned());
        }
        if let Some(mode) = &self.config.stream_mode {
            signal.send(json!({"type":"media-control","action":"mode","protocol":4,"epoch":wire,"streamMode":mode,"reason":"signaling-start"}).to_string()).await?;
        }
        let request_id = new_request_id();
        let rtc_selected = self.config.transport != TransportChoice::Wss
            && !(self.config.transport == TransportChoice::Auto && self.remembered_rtc_failure);
        let mut link = Link {
            signal,
            media: None,
            rtc: None,
            epoch: Epoch {
                wire,
                ticket,
                request_id,
                codec: self.config.codec,
                host_present: true,
                presented: false,
                first_frame_deadline: None,
                first_frame_retried: false,
                first_frame_pending: false,
                input_generation: None,
                desktop_available: true,
                desktop_enabled: true,
                desktop_generation: None,
                rtc_generation: 1,
                rtc_require_admission: false,
                rtc_admitted_timestamp: None,
            },
        };
        let mut media_rx = None;
        if rtc_selected {
            let loopback = url::Url::parse(endpoint.origin())
                .is_ok_and(|u| matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")));
            let mut peer = Peer::new(
                self.config.transport,
                self.config.ice_checking_timeout,
                self.config.codec == Codec::Hevc,
                &ice_servers,
                loopback,
                route_ip,
            )
            .await?;
            link.signal
                .send(json!({"type":"offer","sdp":peer.offer}).to_string())
                .await?;
            peer.offer_sent(Instant::now());
            link.rtc = Some(peer);
            link.signal
                .send(rtc_first_frame_request(&link.epoch.request_id))
                .await?;
            let mut stats = self.stats.webrtc.lock().unwrap();
            stats.enabled = true;
            stats.ice_checking_timeout_ms =
                Some(self.config.ice_checking_timeout.as_millis() as u64);
            stats.selected_transport = Some("webrtc".into());
            stats.state(self.local_epoch, "new", Instant::now(), self.started);
        } else {
            self.start_wss(endpoint, &mut link, &mut media_rx).await?;
        }
        count(&self.stats.established);
        self.backoff = Duration::ZERO;
        self.ever_opened = true;
        self.stats.set_state(NetState::Starting);
        self.serve(endpoint, &mut link, signal_rx, media_rx).await
    }

    async fn start_wss(
        &mut self,
        endpoint: &RelayEndpoint,
        link: &mut Link,
        media_rx: &mut Option<Source>,
    ) -> Result<(), Halt> {
        self.stats.set_state(NetState::MediaHandshake);
        let query = match self.pc_query() {
            Some(pc) => format!("{pc}&epoch={}", link.epoch.wire),
            None => format!("epoch={}", link.epoch.wire),
        };
        let socket = self
            .handshake(open_socket(endpoint, "/media-ws", Some(&query)))
            .await?;
        let (tx, mut rx) = socket.split();
        let mut media = Lane::new(tx);
        media.send(media_hello(&link.epoch.ticket)).await?;
        link.epoch.host_present = self
            .handshake(await_media_ready(&mut rx, &self.stats))
            .await?;
        let mut value: Value =
            serde_json::from_str(&start_message(&link.epoch.wire, link.epoch.codec))
                .expect("locally encoded start");
        if let Some(mode) = &self.config.stream_mode {
            value["streamMode"] = json!(mode);
        }
        if self.config.audio.is_some() {
            value["audio"] = json!(1);
        }
        link.signal.send(value.to_string()).await?;
        link.epoch.request_id = new_request_id();
        link.epoch.presented = false;
        link.epoch.input_generation = None;
        link.epoch.first_frame_retried = false;
        link.signal
            .send(first_frame_request(
                &link.epoch.request_id,
                &link.epoch.wire,
            ))
            .await?;
        link.epoch.first_frame_deadline = self
            .config
            .video_watchdog
            .then(|| Instant::now() + self.timing.first_frame);
        link.media = Some(media);
        *media_rx = Some(rx);
        self.stats.set_state(NetState::Starting);
        self.stats.webrtc.lock().unwrap().selected_transport = Some("wss".into());
        Ok(())
    }

    async fn fallback_wss(
        &mut self,
        endpoint: &RelayEndpoint,
        link: &mut Link,
        media_rx: &mut Option<Source>,
        reason: &str,
    ) -> Result<(), Halt> {
        if link.media.is_some() {
            return Ok(());
        }
        let Some(peer) = link.rtc.as_mut() else {
            return Ok(());
        };
        peer.startup.select_wss();
        self.stats.webrtc.lock().unwrap().fallback(
            reason,
            Instant::now(),
            self.started,
            peer.offer_at,
        );
        self.remembered_rtc_failure = true;
        if let Some(input) = self.config.input.clone() {
            input.set_ready(false);
        }
        if let Some(audio) = &self.config.audio {
            audio.reset_epoch(self.local_epoch);
        }
        self.reset_gate("webrtc-fallback");
        // Keep the RTC peer as lifecycle/control anchor, as receiver.js:1472-1491.
        self.start_wss(endpoint, link, media_rx).await
    }

    async fn serve(
        &mut self,
        endpoint: &RelayEndpoint,
        link: &mut Link,
        mut signal_rx: Source,
        mut media_rx: Option<Source>,
    ) -> Result<(), Halt> {
        let mut udp_buffer = vec![0; 65536];
        let result=async {
            loop {
                if self.stop_requested(){return Err(Halt::Stop);}
                // A terminal peer must stop receiving UDP even while its
                // bookkeeping survives WSS takeover. In particular, Windows
                // UDP connection-reset errors from the old endpoint must not
                // tear down the surviving WebSocket session.
                let udp=link.rtc.as_ref().filter(|p|p.rtc.is_alive()).map(|p|p.socket.clone());
                let wait=link.rtc.as_ref().filter(|p|p.rtc.is_alive()).map_or(self.timing.tick,|p|p.wait(Instant::now()).min(self.timing.tick));
                let wake=tokio::select! {
                    item=async {match media_rx.as_mut(){Some(rx)=>rx.next().await,None=>std::future::pending().await}}=>Wake::Media(item),
                    item=signal_rx.next()=>Wake::Signal(item),
                    received=async {if let Some(socket)=udp {let (length,source)=socket.recv_from(&mut udp_buffer).await.map_err(|e|NetError::Socket(e.to_string()))?;Ok::<_,NetError>((udp_buffer[..length].to_vec(),source))}else{std::future::pending().await}}=>{
                        let (bytes,source)=received?;Wake::Udp(bytes,source)
                    },
                    _=sleep(wait)=>Wake::Tick,
                    _=async {if let Some(input)=&self.config.input {input.notify.notified().await;} else {std::future::pending::<()>().await;}}=>Wake::Input,
                };
                match wake {
                    Wake::Media(item)=> {let inbound=classify(item,"media")?;if let Some(media)=&mut link.media {media.last_seen=Instant::now();}self.on_media(link,inbound).await?;},
                    Wake::Signal(item)=> {let inbound=classify(item,"signal")?;link.signal.last_seen=Instant::now();self.on_signal(link,inbound)?;},
                    Wake::Udp(bytes,source)=> {if let Some(peer)=&mut link.rtc {if !peer.datagram(&bytes,source)? {self.stats.webrtc.lock().unwrap().rejected_datagrams+=1;}}},
                    Wake::Tick|Wake::Input=>{},
                }
                if let Some(peer)=&mut link.rtc {
                    let events=peer.pump().await?;
                    for event in events {
                        if let Some(reason)=self.on_rtc_event(link,event).await? {self.fallback_wss(endpoint,link,&mut media_rx,reason).await?;}
                    }
                }
                if let Some(peer)=&mut link.rtc {
                    for candidate in std::mem::take(&mut peer.trickle) {link.signal.send(json!({"type":"ice","candidate":candidate}).to_string()).await?;}
                    let mut stats=self.stats.webrtc.lock().unwrap();stats.stun_requests=peer.stun_requests;stats.srflx_candidates=peer.srflx_count;
                }
                if link.media.is_none() {
                    let actions=link.rtc.as_mut().map(|p|p.startup.tick(Instant::now())).unwrap_or_default();
                    for action in actions {
                        match action {
                            Action::Fallback(reason)=>self.fallback_wss(endpoint,link,&mut media_rx,reason).await?,
                            Action::RetryFirstFrame=> {
                                self.stats.webrtc.lock().unwrap().first_frame_retries+=1;
                                link.signal.send(rtc_first_frame_request(&link.epoch.request_id)).await?;
                                link.signal.send(rtc_first_frame_keyframe(link.epoch.codec)).await?;
                                if let Some(peer)=&mut link.rtc {let _=peer.keyframe();}
                            },
                            Action::FirstFrameFailed=>self.stats.webrtc.lock().unwrap().first_frame_failures+=1,
                        }
                    }
                }
                self.tick(link).await?;
            }
        }.await;
        if let Some(input) = self.config.input.clone() {
            input.set_ready(false);
            if self
                .send_input_packet(link, vec![6])
                .await
                .is_ok_and(|sent| sent)
            {
                input.sent(true);
            }
        }
        if matches!(result, Err(Halt::Stop)) {
            if let Some(rx) = media_rx.as_mut() {
                self.shutdown(link, &mut signal_rx, rx).await;
            } else {
                close_lane(&mut link.signal, &mut signal_rx).await;
            }
        }
        if let Some(peer) = &mut link.rtc {
            peer.rtc.disconnect();
        }
        result
    }

    /// Stop production before draining the independent media lane. Shutdown
    /// deliberately ignores decoder feedback and never offers another frame.
    async fn shutdown(&mut self, link: &mut Link, signal_rx: &mut Source, media_rx: &mut Source) {
        self.pending_keyframe = None;
        self.pending_reset = None;
        link.epoch.first_frame_deadline = None;
        // Every await, including sends to an unresponsive relay, shares this
        // absolute cap. Valid video extends quiet time; other traffic does not.
        let _ = timeout(SHUTDOWN_DRAIN, async {
            let _ = link.signal.send(json!({
                "type": "media-control", "action": "stop", "protocol": 4,
                "epoch": link.epoch.wire,
            }).to_string()).await;
            let mut quiet_until = tokio::time::Instant::now() + SHUTDOWN_QUIET;
            let mut signal_open = true;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(quiet_until) => break,
                    item = signal_rx.next(), if signal_open => {
                        if matches!(item, None | Some(Err(_)) | Some(Ok(Message::Close(_)))) {
                            signal_open = false;
                        }
                    }
                    item = media_rx.next() => {
                        match item {
                            Some(Ok(Message::Binary(data))) => {
                                let valid = !is_audio_packet(&data) && parse_frame(&data).is_ok();
                                if self.on_media_packet(link, Vec::from(data), false).await.is_err() {
                                    break;
                                }
                                if valid {
                                    quiet_until = tokio::time::Instant::now() + SHUTDOWN_QUIET;
                                }
                            }
                            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                            _ => {}
                        }
                    }
                }
            }
        }).await;
        // Send normal close frames, then read the peer's close replies. Both
        // lanes close concurrently, adding at most SHUTDOWN_CLOSE to exit.
        tokio::join!(
            close_lane(link.media.as_mut().expect("WSS media lane"), media_rx),
            close_lane(&mut link.signal, signal_rx),
        );
    }

    async fn on_media(&mut self, link: &mut Link, inbound: Inbound) -> Result<(), Halt> {
        match inbound {
            Inbound::Text(text) => {
                self.on_media_text(link, &text);
                Ok(())
            }
            Inbound::Binary(data) => self.on_media_packet(link, data, true).await,
            Inbound::Other => Ok(()),
        }
    }

    /// Relay controls on the media socket. Host text is only accepted in the
    /// shapes `validate_host_control` allows.
    fn on_media_text(&mut self, link: &mut Link, text: &str) {
        match parse_relay_control(text) {
            Some(RelayControl::MediaPeer { present }) => {
                if present != link.epoch.host_present {
                    if let Some(audio) = &self.config.audio {
                        audio.reset_epoch(self.local_epoch);
                    }
                }
                if present && !link.epoch.host_present {
                    // Same as the browser: the host sends a keyframe once it sees the peer.
                    self.reset_gate("host-joined");
                }
                link.epoch.host_present = present;
            }
            Some(RelayControl::Pong) | Some(RelayControl::HelloOk { .. }) => {}
            None if validate_host_control(text) => {}
            None => count(&self.stats.rejected_controls),
        }
    }

    /// Demux BPA1 before BPV4: audio never ACKs or changes the video receive gate.
    async fn on_media_packet(
        &mut self,
        link: &mut Link,
        mut data: Vec<u8>,
        decode: bool,
    ) -> Result<(), Halt> {
        let received_at = Instant::now();
        if is_audio_packet(&data) {
            if decode && link.epoch.host_present {
                if let Some(audio) = &self.config.audio {
                    audio.push(self.local_epoch, &data, received_at);
                } else {
                    count(&self.stats.bpa1_ignored);
                }
            } else {
                count(&self.stats.bpa1_ignored);
            }
            return Ok(());
        }
        let header = match parse_frame(&data) {
            Ok((header, _)) => header,
            Err(_reason) => {
                count(&self.stats.rejected_packets);
                return Ok(());
            }
        };
        self.stats.record_received(received_at);
        if let Some(input) = &self.config.input {
            input.dimensions(header.width as u32, header.height as u32);
        }

        // ACK before the gate: every valid frame is acknowledged, dropped or not.
        let recv_ts_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        link.media
            .as_mut()
            .expect("WSS media lane")
            .send(ack_message(
                header.generation,
                header.seq,
                recv_ts_ms,
                self.decoded_seq,
            ))
            .await?;
        count(&self.stats.acks_sent);
        if !decode || self.stop_requested() {
            return Ok(());
        }

        data.drain(..V4_HEADER_BYTES);
        match self.gate.accept(&header) {
            GateVerdict::Decode => {
                self.gate_generation = Some(header.generation);
                if header.key {
                    self.pending_keyframe = None;
                }
                if self.stats.state() == NetState::Starting {
                    self.stats.set_state(NetState::Streaming);
                }
                let frame = VideoFrame {
                    local_epoch: self.local_epoch,
                    codec: header.codec,
                    key: header.key,
                    refine: header.refine,
                    generation: header.generation,
                    seq: header.seq,
                    capture_ts_us: header.capture_ts_us,
                    width: header.width,
                    height: header.height,
                    data,
                    received_at,
                };
                self.offer_frame(frame)
            }
            GateVerdict::Drop {
                invalidated,
                request,
            } => {
                self.stats.record_gate_drop();
                if invalidated {
                    self.gate_generation = None;
                    self.decoded_seq = None;
                    self.pending_reset = Some(request.clone());
                }
                self.pending_keyframe = Some(request);
                Ok(())
            }
        }
    }

    fn on_signal(&mut self, link: &mut Link, inbound: Inbound) -> Result<(), Halt> {
        match inbound {
            Inbound::Text(text) => {
                if let Some(message) = relay_error(&text) {
                    return Err(Halt::from(NetError::RelayError(message)));
                }
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    match value["type"].as_str() {
                        Some("answer") => {
                            if let (Some(peer), Some(sdp)) = (&mut link.rtc, value["sdp"].as_str())
                            {
                                peer.answer(sdp)?;
                            }
                            return Ok(());
                        }
                        Some("ice") => {
                            if let (Some(peer), Some(candidate)) =
                                (&mut link.rtc, value["candidate"].as_str())
                            {
                                if peer.candidate(candidate).is_err() {
                                    self.stats.webrtc.lock().unwrap().malformed_candidates += 1;
                                }
                            }
                            return Ok(());
                        }
                        Some("first-frame") if value["requestId"] == link.epoch.request_id => {
                            if value["phase"] == "admitted" {
                                if let Some(peer) = &mut link.rtc {
                                    peer.startup.progress(Instant::now());
                                }
                                link.epoch.rtc_admitted_timestamp = value["rtpTimestamp"]
                                    .as_u64()
                                    .and_then(|v| u32::try_from(v).ok());
                                if let Some(generation) = value["generation"]
                                    .as_u64()
                                    .and_then(|v| u32::try_from(v).ok())
                                {
                                    link.epoch.rtc_generation = generation;
                                }
                            }
                            return Ok(());
                        }
                        Some("peer-left") => {
                            return Err(Halt::from(NetError::Socket("host left session".into())))
                        }
                        // Handled before input, so a session without native input still regates.
                        Some("desktop-state")
                            if text.len() <= 16384
                                && matches!(
                                    value["state"].as_str(),
                                    Some("active" | "unavailable")
                                ) =>
                        {
                            self.desktop_state(link, &value);
                            return Ok(());
                        }
                        Some("monitor-list" | "monitor-error") => {
                            if !monitor_message(&self.config.controls, &link.epoch.wire, &value) {
                                count(&self.stats.rejected_controls);
                            }
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                if self.input_control(&text) {
                    return Ok(());
                }
                match parse_relay_control(&text) {
                    Some(RelayControl::Pong) => {}
                    _ => count(&self.stats.rejected_controls),
                }
            }
            Inbound::Binary(data) => {
                if !self
                    .config
                    .input
                    .as_ref()
                    .is_some_and(|input| input.receive(&data))
                {
                    count(&self.stats.rejected_controls);
                }
            }
            Inbound::Other => {}
        }
        Ok(())
    }
    fn input_control(&mut self, text: &str) -> bool {
        if self.config.input.is_none() || text.len() > 16384 {
            return false;
        }
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return false;
        };
        match value["type"].as_str() {
            Some("input-capabilities") => {
                self.config
                    .input
                    .as_ref()
                    .unwrap()
                    .capabilities(value["protocol"] == 1 && value["scancode"] == true);
                true
            }
            _ => false,
        }
    }

    /// `desktop-state` regates the first frame whether or not native input is configured.
    /// A switch to `unavailable` and back reopens the RTP admission gate on the same peer.
    fn desktop_state(&mut self, link: &mut Link, value: &Value) {
        link.epoch.desktop_available = value["state"] == "active";
        if let Some(input) = value["input"].as_str() {
            if matches!(input, "enabled" | "suspended" | "read-only") {
                link.epoch.desktop_enabled = input == "enabled";
            }
        }
        if link
            .epoch
            .regate_desktop(value["generation"].as_u64(), self.timing.first_frame)
        {
            // Close the producer gate now: unavailable and active can arrive
            // before the next tick, and a fast decode must not preserve input
            // queued for the old desktop through that transition.
            if let Some(input) = &self.config.input {
                // A monitor switch keeps held input (browser inputTransitionHold);
                // the transport gate still closes now. Other changes release.
                if is_monitor_transition(value) {
                    input.hold_for_monitor();
                } else {
                    input.set_ready(false);
                }
            }
            self.reset_gate("desktop-state");
            if let Some(peer) = &mut link.rtc {
                peer.startup.ready = false;
                peer.startup.request_sent(Instant::now());
                // A new desktop generation requires the host's RTP
                // admission boundary; buffered old desktop media must
                // never reopen native input on the new desktop.
                link.epoch.rtc_require_admission = link.epoch.desktop_generation.is_some();
                link.epoch.rtc_admitted_timestamp = None;
            }
        }
    }

    /// Write the user's queued mode and monitor commands on the `/ws` socket.
    /// Commands were already taken out of the shared state, so no lock is held here.
    async fn drain_controls(&mut self, link: &mut Link) -> Result<(), Halt> {
        let outbound = self.config.controls.take_outbound(&link.epoch.wire);
        if let Some(mode) = outbound.mode {
            // As `releaseInput` before `sendStreamMode("user-mode")`: held keys and buttons go first.
            if let Some(input) = self.config.input.clone() {
                input.release(self.local_epoch);
                // Flush the release barrier before the signaling mode command,
                // as the browser does on its input lane. When gated, next()
                // retains the barrier for the next admission.
                if let Some(release) = input.next() {
                    if self.send_input_packet(link, release.clone()).await? {
                        input.sent(true);
                    } else {
                        input.retry_front(release);
                    }
                }
            }
            let wire = stream_mode_wire(mode);
            link.signal
                .send(mode_control_message(&link.epoch.wire, wire))
                .await?;
            // Reconnects start in the mode the user last chose.
            self.config.stream_mode = Some(wire.to_owned());
            self.config
                .controls
                .mark_mode_written(outbound.mode_request);
        }
        if let Some(selector) = outbound.pick {
            link.signal
                .send(monitor_control_message(&link.epoch.wire, &selector))
                .await?;
        }
        Ok(())
    }

    async fn drain_input(&mut self, link: &mut Link) -> Result<(), Halt> {
        let Some(input) = self.config.input.clone() else {
            return Ok(());
        };
        let ready = link.epoch.host_present
            && link.epoch.desktop_available
            && link.epoch.desktop_enabled
            && link.epoch.input_generation.is_some()
            && link.epoch.input_generation == self.gate_generation
            && (link.media.is_some() || link.rtc.as_ref().is_some_and(Peer::lanes_ready));
        // Monitor carry (browser inputTransitionHold): the gate stays closed and held
        // input is kept until the new frame reopens it. Losing the host or the
        // desktop input still releases through set_ready below.
        if !ready && input.monitor_hold() && link.epoch.host_present && link.epoch.desktop_enabled {
            return Ok(());
        }
        let was_ready = input.snapshot().ready;
        input.set_ready(ready);
        if was_ready && !ready {
            self.send_input_packet(link, vec![6]).await?;
            input.sent(true);
        }
        if ready {
            for _ in 0..128 {
                let Some(packet) = input.next() else {
                    break;
                };
                let release = packet == [6];
                let sent_at = Instant::now();
                let absolute = packet.first() == Some(&8);
                if absolute {
                    input.absolute_sent(&packet, sent_at);
                }
                if self.send_input_packet(link, packet.clone()).await? {
                    input.sent(release);
                } else {
                    if absolute {
                        input.absolute_unsent(&packet);
                    }
                    input.retry_front(packet);
                    break;
                }
            }
        }
        Ok(())
    }

    async fn send_input_packet(&mut self, link: &mut Link, packet: Vec<u8>) -> Result<bool, Halt> {
        let framed = super::webrtc::channels::frame_outbound(&packet)
            .map_err(|e| Halt::from(NetError::Protocol(e.to_string())))?;
        let motion = framed.lane == super::webrtc::channels::Lane::Motion;
        if let Some(peer) = &mut link.rtc {
            if peer.send_input(framed.bytes, motion)? {
                let mut stats = self.stats.webrtc.lock().unwrap();
                if motion {
                    stats.input_motion_sent += 1;
                } else {
                    stats.input_control_sent += 1;
                }
                return Ok(true);
            }
        }
        if link.media.is_some() {
            link.signal.send_binary(packet).await?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn on_rtc_event(
        &mut self,
        link: &mut Link,
        event: RtcEvent,
    ) -> Result<Option<&'static str>, Halt> {
        let now = Instant::now();
        match event {
            RtcEvent::IceConnectionStateChange(state) => {
                let name = match state {
                    IceConnectionState::New => "new",
                    IceConnectionState::Checking => "checking",
                    IceConnectionState::Connected => "connected",
                    IceConnectionState::Completed => "completed",
                    IceConnectionState::Disconnected => "disconnected",
                };
                self.stats
                    .webrtc
                    .lock()
                    .unwrap()
                    .state(self.local_epoch, name, now, self.started);
                if let Some(peer) = &mut link.rtc {
                    peer.startup.ice_connected = matches!(
                        state,
                        IceConnectionState::Connected | IceConnectionState::Completed
                    );
                    peer.startup.progress(now);
                    if state == IceConnectionState::Disconnected {
                        peer.control = None;
                        peer.motion = None;
                        if let Some(Action::Fallback(reason)) = peer.startup.terminal() {
                            peer.rtc.disconnect();
                            return Ok(Some(reason));
                        }
                        if link.media.is_none() {
                            return Err(Halt::from(NetError::Socket("RTC disconnected".into())));
                        }
                        peer.rtc.disconnect();
                    }
                }
            }
            RtcEvent::Connected => {
                if let Some(peer) = &mut link.rtc {
                    peer.startup.peer_connected = true;
                    peer.startup.ice_connected = true;
                    if peer.startup.connected(now) {
                        link.epoch.request_id = new_request_id();
                        link.signal
                            .send(rtc_first_frame_request(&link.epoch.request_id))
                            .await?;
                    }
                }
            }
            RtcEvent::ChannelOpen(id, label) => {
                if let Some(peer) = &mut link.rtc {
                    match super::webrtc::channels::lane_for_label(&label) {
                        Some(super::webrtc::channels::Lane::Control) => peer.control = Some(id),
                        Some(super::webrtc::channels::Lane::Motion) => peer.motion = Some(id),
                        _ => {}
                    }
                    peer.startup.progress(now);
                }
            }
            RtcEvent::ChannelClose(id) => {
                if let Some(peer) = &mut link.rtc {
                    if peer.control == Some(id) {
                        peer.control = None;
                    }
                    if peer.motion == Some(id) {
                        peer.motion = None;
                    }
                    if link.media.is_some() {
                        peer.rtc.disconnect();
                    } else {
                        return Err(Halt::from(NetError::Socket("RTC input lane closed".into())));
                    }
                }
            }
            RtcEvent::ChannelData(data) => {
                let label = link.rtc.as_ref().and_then(|p| {
                    if p.control == Some(data.id) {
                        Some(super::webrtc::channels::Lane::Control.label())
                    } else if p.motion == Some(data.id) {
                        Some(super::webrtc::channels::Lane::Motion.label())
                    } else {
                        None
                    }
                });
                if label.is_some_and(|label| {
                    super::webrtc::channels::accept_inbound(label, data.binary).is_some()
                }) {
                    if let Some(input) = &self.config.input {
                        let _ = input.receive(&data.data);
                    }
                }
            }
            RtcEvent::PeerStats(stats) => {
                if let Some(peer) = &mut link.rtc {
                    peer.startup.stats_available = true;
                    let mut output = self.stats.webrtc.lock().unwrap();
                    output.rtt_ms = stats.rtt.map(|t| t.as_secs_f64() * 1000.0);
                    if let Some(pair) = stats.selected_candidate_pair {
                        output.selected_candidate_pair_type =
                            Some(peer.pair_type(pair.local.addr, pair.remote.addr));
                        output.rtt_ms = pair
                            .current_round_trip_time
                            .map(|t| t.as_secs_f64() * 1000.0)
                            .or(output.rtt_ms);
                    }
                }
            }
            RtcEvent::MediaIngressStats(stats) => {
                if let Some(peer) = &mut link.rtc {
                    let mut output = self.stats.webrtc.lock().unwrap();
                    let target = if stats.mid == peer.video_mid {
                        &mut output.video
                    } else if stats.mid == peer.audio_mid {
                        &mut output.audio
                    } else {
                        return Ok(None);
                    };
                    target.nacks = target.nacks.max(stats.nacks);
                    target.plis = target.plis.max(stats.plis);
                    target.firs = target.firs.max(stats.firs);
                    target.loss_fraction = stats.loss;
                }
            }
            RtcEvent::RtpPacket(packet) => self.on_rtp(link, packet)?,
            RtcEvent::Closed => {
                if let Some(peer) = &mut link.rtc {
                    peer.control = None;
                    peer.motion = None;
                    if let Some(Action::Fallback(reason)) = peer.startup.terminal() {
                        peer.rtc.disconnect();
                        return Ok(Some(reason));
                    }
                    if link.media.is_none() {
                        return Err(Halt::from(NetError::Socket("RTC closed".into())));
                    }
                    peer.rtc.disconnect();
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn on_rtp(&mut self, link: &mut Link, packet: str0m::rtp::RtpPacket) -> Result<(), Halt> {
        let Some(peer) = &mut link.rtc else {
            return Ok(());
        };
        if peer.audio_pt == Some(packet.header.payload_type) {
            {
                let mut stats = self.stats.webrtc.lock().unwrap();
                stats.audio.packets += 1;
                stats.audio.bytes += packet.payload.len() as u64;
                self.rtp_ingress.audio_tracker.packet(
                    *packet.seq_no,
                    packet.header.timestamp,
                    48000.0,
                    packet.timestamp,
                    &mut stats.audio,
                );
            }
            if link.media.is_none() {
                if let Some(audio) = &self.config.audio {
                    audio.push_rtp(
                        self.local_epoch,
                        *packet.seq_no,
                        packet
                            .time
                            .rebase(str0m::media::Frequency::FORTY_EIGHT_KHZ)
                            .numer(),
                        &packet.payload,
                        packet.timestamp,
                    );
                }
            }
            return Ok(());
        }
        let Some(codec) = peer.video_codecs.get(&packet.header.payload_type).copied() else {
            return Ok(());
        };
        {
            let mut stats = self.stats.webrtc.lock().unwrap();
            stats.video.packets += 1;
            stats.video.bytes += packet.payload.len() as u64;
            self.rtp_ingress.video_tracker.packet(
                *packet.seq_no,
                packet.header.timestamp,
                90000.0,
                packet.timestamp,
                &mut stats.video,
            );
        }
        peer.startup.video_packets += 1;
        peer.startup.video_bytes += packet.payload.len() as u64;
        if link.media.is_some() {
            return Ok(());
        }
        link.epoch.codec = codec;
        let (units, changed, dropped) = self.rtp_ingress.push(codec, &packet);
        if changed {
            self.reset_gate("rtp-stream-change");
        }
        if dropped > 0 {
            self.stats.webrtc.lock().unwrap().depacketization_drops += dropped;
            self.reset_gate("rtp-loss");
            self.pending_keyframe = Some("rtp-loss".into());
        }
        for unit in units {
            self.admit_rtc_unit(link, codec, unit)?;
        }
        Ok(())
    }

    fn admit_rtc_unit(
        &mut self,
        link: &mut Link,
        codec: Codec,
        unit: super::webrtc::ingress::Unit,
    ) -> Result<(), Halt> {
        self.stats.record_received(unit.received_at);
        if link.epoch.rtc_require_admission
            && !link
                .epoch
                .rtc_admitted_timestamp
                .is_some_and(|admitted| (unit.au.rtp_timestamp.wrapping_sub(admitted) as i32) >= 0)
        {
            self.stats.record_gate_drop();
            self.pending_keyframe = Some("rtc-admission".into());
            return Ok(());
        }
        let Some(geometry) = unit.au.geometry else {
            self.stats.record_gate_drop();
            self.pending_keyframe = Some("rtp-no-sps".into());
            return Ok(());
        };
        let (Ok(width), Ok(height)) = (
            u16::try_from(geometry.display_width),
            u16::try_from(geometry.display_height),
        ) else {
            return Ok(());
        };
        let header = super::protocol::FrameHeader {
            codec,
            key: unit.au.keyframe,
            refine: false,
            generation: link.epoch.rtc_generation,
            seq: unit.seq,
            capture_ts_us: u64::from(unit.au.rtp_timestamp) * 1_000_000 / 90_000,
            width,
            height,
        };
        match self.gate.accept(&header) {
            GateVerdict::Decode => {
                self.gate_generation = Some(header.generation);
                if header.key {
                    self.pending_keyframe = None;
                }
                if let Some(input) = &self.config.input {
                    input.dimensions(u32::from(width), u32::from(height));
                }
                self.stats.set_state(NetState::Streaming);
                self.offer_frame(VideoFrame {
                    local_epoch: self.local_epoch,
                    codec,
                    key: header.key,
                    refine: false,
                    generation: header.generation,
                    seq: header.seq,
                    capture_ts_us: header.capture_ts_us,
                    width,
                    height,
                    data: unit.au.annex_b,
                    received_at: unit.received_at,
                })
            }
            GateVerdict::Drop {
                invalidated,
                request,
            } => {
                self.stats.record_gate_drop();
                if invalidated {
                    self.gate_generation = None;
                    self.decoded_seq = None;
                    self.pending_reset = Some(request.clone());
                }
                self.pending_keyframe = Some(request);
                Ok(())
            }
        }
    }

    /// Per-tick work: feedback, pending reset and keyframe, heartbeats, dead
    /// detection and the first-frame watchdog.
    async fn tick(&mut self, link: &mut Link) -> Result<(), Halt> {
        if self.stop_requested() {
            return Err(Halt::Stop);
        }
        // A bounded packet reorder window must also release an idle, incomplete
        // access unit. Live frames normally resolve gaps on the next timestamp;
        // this only expires assembly when RTP itself has gone quiet.
        if link.media.is_none()
            && self
                .rtp_ingress
                .last_video
                .is_some_and(|at| at.elapsed() >= Duration::from_millis(100))
        {
            let (units, dropped) = self.rtp_ingress.flush();
            if dropped > 0 {
                self.stats.webrtc.lock().unwrap().depacketization_drops += dropped;
                self.reset_gate("rtp-stalled-assembly");
                self.pending_keyframe = Some("rtp-stalled-assembly".into());
            }
            for unit in units {
                self.admit_rtc_unit(link, link.epoch.codec, unit)?;
            }
        }
        if std::mem::take(&mut link.epoch.first_frame_pending) {
            let request = if link.media.is_some() {
                first_frame_request(&link.epoch.request_id, &link.epoch.wire)
            } else {
                rtc_first_frame_request(&link.epoch.request_id)
            };
            link.signal.send(request).await?;
            self.pending_keyframe = Some("desktop-state".into());
        }
        self.drain_controls(link).await?;
        self.drain_feedback(link).await?;
        self.drain_input(link).await?;
        self.flush_reset()?;
        self.send_pending_keyframe(link).await?;

        let now = Instant::now();
        if now.duration_since(link.signal.last_seen) >= self.timing.dead {
            return Err(Halt::from(NetError::Stalled("signal")));
        }
        if link
            .media
            .as_ref()
            .is_some_and(|m| now.duration_since(m.last_seen) >= self.timing.dead)
        {
            return Err(Halt::from(NetError::Stalled("media")));
        }
        if now.duration_since(link.signal.last_sent) >= self.timing.heartbeat {
            link.signal.send(ping_message()).await?;
            count(&self.stats.heartbeats_sent);
        }
        if let Some(media) = &mut link.media {
            if now.duration_since(media.last_sent) >= self.timing.heartbeat {
                media.send(ping_message()).await?;
                count(&self.stats.heartbeats_sent);
            }
        }
        self.check_first_frame(link).await
    }

    async fn drain_feedback(&mut self, link: &mut Link) -> Result<(), Halt> {
        while self.feedback_open {
            match self.feedback.try_recv() {
                Ok(feedback) => self.on_feedback(link, feedback).await?,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => self.feedback_open = false,
            }
        }
        Ok(())
    }

    async fn on_feedback(
        &mut self,
        link: &mut Link,
        feedback: DecoderFeedback,
    ) -> Result<(), Halt> {
        if feedback.local_epoch() != self.local_epoch {
            count(&self.stats.stale_feedback);
            return Ok(());
        }
        match feedback {
            DecoderFeedback::Decoded {
                generation, seq, ..
            } => {
                if self.gate_generation != Some(generation) {
                    count(&self.stats.stale_feedback);
                    return Ok(());
                }
                count(&self.stats.decoded);
                self.stats.last_decoded_seq.store(u64::from(seq), Relaxed);
                self.decoded_seq = Some(seq);
                if let Some(peer) = &mut link.rtc {
                    if link.media.is_none() {
                        peer.startup.decoded();
                        self.remembered_rtc_failure = false;
                    }
                }
                if self.config.headless_decoded_ready {
                    link.epoch.input_generation = Some(generation);
                    link.epoch.first_frame_deadline = None;
                    if let Some(peer) = &mut link.rtc {
                        peer.startup.presented();
                    }
                }
            }
            DecoderFeedback::Presented { generation, .. } => {
                if self.gate_generation != Some(generation) {
                    return Ok(());
                }
                link.epoch.input_generation = Some(generation);
                if link.media.is_none() {
                    if let Some(peer) = &mut link.rtc {
                        peer.startup.presented();
                        self.remembered_rtc_failure = false;
                    }
                }
                link.epoch.first_frame_deadline = None;
                if !link.epoch.presented {
                    link.epoch.presented = true;
                    link.signal
                        .send(first_frame_presented(&link.epoch.request_id, generation))
                        .await?;
                }
            }
            DecoderFeedback::DecodeError { reason, .. }
            | DecoderFeedback::KeyframeNeeded { reason, .. } => {
                let reason = sanitize_reason(&reason);
                self.gate.invalidate(&reason);
                self.gate_generation = None;
                self.decoded_seq = None;
                self.pending_reset = Some(reason.clone());
                self.pending_keyframe = Some(reason);
            }
        }
        Ok(())
    }

    /// Send the pending keyframe request if the throttle allows.
    async fn send_pending_keyframe(&mut self, link: &mut Link) -> Result<(), Halt> {
        if self.pending_keyframe.is_none() || !self.throttle.allow(Instant::now()) {
            return Ok(());
        }
        if let Some(reason) = self.pending_keyframe.take() {
            if let Some(media) = &mut link.media {
                media.send(keyframe_message(&reason)).await?;
            } else {
                link.signal.send(json!({"type":"media-control","action":"keyframe","protocol":4,"epoch":link.epoch.wire,"reason":reason,"codec":link.epoch.codec.wire_name()}).to_string()).await?;
                if let Some(peer) = &mut link.rtc {
                    let _ = peer.keyframe();
                }
            }
            self.stats.record_keyframe();
        }
        Ok(())
    }

    /// The first-frame watchdog: one resend of the request and a
    /// `first-frame-timeout` keyframe, then a reconnect.
    async fn check_first_frame(&mut self, link: &mut Link) -> Result<(), Halt> {
        if link.media.is_none() {
            return Ok(());
        }
        let Some(deadline) = link.epoch.first_frame_deadline else {
            return Ok(());
        };
        let now = Instant::now();
        if now < deadline {
            return Ok(());
        }
        if link.epoch.first_frame_retried {
            return Err(Halt::from(NetError::FirstFrameTimeout));
        }
        link.epoch.first_frame_retried = true;
        link.epoch.first_frame_deadline = Some(now + self.timing.first_frame);
        link.signal
            .send(first_frame_request(
                &link.epoch.request_id,
                &link.epoch.wire,
            ))
            .await?;
        link.signal
            .send(first_frame_keyframe(&link.epoch.wire, link.epoch.codec))
            .await?;
        Ok(())
    }

    /// Restart the gate with `reason` and tell the decoder to discard its state.
    fn reset_gate(&mut self, reason: &str) {
        self.gate.reset(reason);
        self.gate_generation = None;
        self.decoded_seq = None;
        self.pending_reset = Some(reason.to_owned());
    }

    /// Deliver a frame if no reset is waiting and the queue has room. A full
    /// queue drops the frame and invalidates the gate.
    fn offer_frame(&mut self, frame: VideoFrame) -> Result<(), Halt> {
        if !self.flush_reset()? {
            self.overflow();
            return Ok(());
        }
        match self.events.try_send(NetEvent::Frame(frame)) {
            Ok(()) => {
                count(&self.stats.frames_accepted);
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.overflow();
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => Err(Halt::Stop),
        }
    }

    fn overflow(&mut self) {
        self.stats.record_overflow();
        self.gate.invalidate("queue-overflow");
        self.gate_generation = None;
        self.decoded_seq = None;
        self.pending_reset = Some("queue-overflow".to_owned());
        self.pending_keyframe = Some("queue-overflow".to_owned());
    }

    /// Deliver the pending reset if the queue has room. Returns false when it did not.
    fn flush_reset(&mut self) -> Result<bool, Halt> {
        let Some(reason) = self.pending_reset.take() else {
            return Ok(true);
        };
        let event = NetEvent::Reset(ResetEvent {
            local_epoch: self.local_epoch,
            reason: reason.clone(),
        });
        match self.events.try_send(event) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => {
                self.pending_reset = Some(reason);
                Ok(false)
            }
            Err(TrySendError::Disconnected(_)) => Err(Halt::Stop),
        }
    }
}

/// Finish our own WebSocket without letting an absent close reply hang exit.
async fn close_lane(lane: &mut Lane, source: &mut Source) {
    let _ = timeout(SHUTDOWN_CLOSE, async {
        if lane.tx.send(Message::Close(None)).await.is_err() {
            return;
        }
        while let Some(Ok(message)) = source.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
}

/// Open one WebSocket. No `Origin` header and no cookies: the relay admits a native
/// viewer by its signed hello alone.
async fn open_socket(
    endpoint: &RelayEndpoint,
    path: &str,
    query: Option<&str>,
) -> Result<Socket, NetError> {
    let url = endpoint.ws_url(path, query);
    let request = url
        .as_str()
        .into_client_request()
        .map_err(|error| NetError::Handshake(error.to_string()))?;
    // Keep media access units and small ACK/control messages from waiting for
    // TCP's delayed-ACK/Nagle interaction before the decoder can see them.
    match connect_async_with_config(request, None, true).await {
        Ok((socket, _response)) => Ok(socket),
        Err(tungstenite::Error::Http(response)) => {
            let status = response.status().as_u16();
            match status {
                401 | 403 => Err(NetError::Auth(status)),
                _ => Err(NetError::Handshake(format!("HTTP {status}"))),
            }
        }
        Err(error) => Err(NetError::Handshake(error.to_string())),
    }
}

/// Both media controls must arrive before start; controls can arrive in either
/// order. A binary frame before registration is a broken handshake.
async fn await_media_ready(source: &mut Source, stats: &TransportStats) -> Result<bool, NetError> {
    let mut hello = false;
    let mut peer = None;
    loop {
        match classify(source.next().await, "media")? {
            Inbound::Text(text) => {
                if let Some(message) = relay_error(&text) {
                    return Err(NetError::RelayError(message));
                }
                match parse_relay_control(&text) {
                    Some(RelayControl::HelloOk { .. }) => hello = true,
                    Some(RelayControl::MediaPeer { present }) => peer = Some(present),
                    Some(RelayControl::Pong) => {}
                    _ => count(&stats.rejected_controls),
                }
                if hello {
                    if let Some(present) = peer {
                        return Ok(present);
                    }
                }
            }
            Inbound::Binary(_) => {
                return Err(NetError::Protocol("video before media registration".into()))
            }
            Inbound::Other => {}
        }
    }
}

/// Read until the relay's `register-challenge` and return its nonce. A 1008 close here
/// means the hello was refused.
async fn await_register_challenge(
    source: &mut Source,
    stats: &TransportStats,
) -> Result<[u8; 32], NetError> {
    loop {
        match admission(source.next().await)? {
            Inbound::Text(text) => {
                if let Some(message) = relay_error(&text) {
                    return Err(NetError::RelayError(message));
                }
                match parse_register_challenge(&text) {
                    Some(nonce) => return Ok(nonce),
                    None => count(&stats.rejected_controls),
                }
            }
            Inbound::Binary(_) => count(&stats.rejected_controls),
            Inbound::Other => {}
        }
    }
}

/// [`classify`] for the signalling socket before `hello-ok`: the relay refuses a viewer
/// (unknown key, not admitted to the PC, bad proof) by closing with 1008, which is fatal.
fn admission(item: Option<Result<Message, tungstenite::Error>>) -> Result<Inbound, NetError> {
    if let Some(Ok(Message::Close(Some(frame)))) = &item {
        if frame.code == CloseCode::Policy {
            return Err(NetError::NotAdmitted);
        }
    }
    classify(item, "signal")
}

/// Read until the relay's `hello-ok`. Returns its `mediaEpoch` and `ticket`, if present,
/// and its `iceServers`.
async fn await_hello_ok(
    source: &mut Source,
    stats: &TransportStats,
) -> Result<(Option<String>, Option<String>, Value), NetError> {
    loop {
        match admission(source.next().await)? {
            Inbound::Text(text) => {
                if let Some(message) = relay_error(&text) {
                    return Err(NetError::RelayError(message));
                }
                match parse_relay_control(&text) {
                    Some(RelayControl::HelloOk {
                        media_epoch,
                        ticket,
                    }) => {
                        return Ok((
                            media_epoch,
                            ticket,
                            serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)
                                ["iceServers"]
                                .clone(),
                        ))
                    }
                    _ => count(&stats.rejected_controls),
                }
            }
            Inbound::Binary(_) => count(&stats.rejected_controls),
            Inbound::Other => {}
        }
    }
}

/// Map one WebSocket item to what the session needs. A closed or broken socket
/// is transient.
fn classify(
    item: Option<Result<Message, tungstenite::Error>>,
    label: &str,
) -> Result<Inbound, NetError> {
    match item {
        None => Err(NetError::Socket(format!("{label} socket closed"))),
        Some(Err(error)) => Err(NetError::Socket(error.to_string())),
        Some(Ok(Message::Text(text))) => Ok(Inbound::Text(text.as_str().to_owned())),
        Some(Ok(Message::Binary(data))) => Ok(Inbound::Binary(Vec::from(data))),
        Some(Ok(Message::Close(_))) => {
            Err(NetError::Socket(format!("{label} socket closed by relay")))
        }
        Some(Ok(_)) => Ok(Inbound::Other),
    }
}

/// The message of a relay `{"type":"error"}` (the browser reads `message`).
fn relay_error(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    if object.get("type")?.as_str()? != "error" {
        return None;
    }
    Some(
        object
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("relay error")
            .to_owned(),
    )
}

fn rtc_first_frame_request(request_id: &str) -> String {
    json!({"type":"first-frame","protocol":1,"phase":"request","requestId":request_id,"transport":"webrtc"}).to_string()
}
fn rtc_first_frame_keyframe(codec: Codec) -> String {
    // receiver.js:342 uses firstFrameHandshake.epoch, which is null for RTC.
    json!({"type":"media-control","action":"keyframe","protocol":4,"epoch":null,
        "reason":"first-frame-timeout","codec":codec.wire_name()})
    .to_string()
}

/// Apply `monitor-list` or `monitor-error` from the `/ws` socket of `wire`.
/// Returns false only for a malformed `monitor-list`. An explicit `epoch` that is
/// not `wire` (including a non-string one) is ignored and still counts as handled,
/// so a stale error never clears the pending pick. A message with no `epoch`
/// belongs to the current socket.
fn monitor_message(controls: &SessionControls, wire: &str, value: &Value) -> bool {
    if value
        .get("epoch")
        .is_some_and(|epoch| epoch.as_str() != Some(wire))
    {
        return true;
    }
    match value["type"].as_str() {
        Some("monitor-list") => match parse_monitor_list(value) {
            Some(monitors) => {
                controls.apply_monitor_list(monitors, wire);
                true
            }
            None => false,
        },
        Some("monitor-error") => {
            controls.apply_monitor_error(parse_monitor_error(value), wire);
            true
        }
        _ => false,
    }
}

/// A monitor switch carries held input through the closed gate (browser
/// `desktopTransitionPlan`). Other desktop changes release, as before.
fn is_monitor_transition(value: &Value) -> bool {
    matches!(
        value["reason"].as_str(),
        Some("monitor-switch" | "monitor-switched")
    )
}

#[cfg(test)]
mod monitor_message_tests {
    use super::*;
    use crate::net::controls::PickOutcome;

    const EPOCH: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210";

    fn entry(selector: &str, selected: bool) -> Value {
        json!({ "selector": selector, "name": selector, "selected": selected })
    }

    /// Registered on EPOCH with DISPLAY1 selected and DISPLAY2 available.
    fn connected() -> SessionControls {
        let controls = SessionControls::new();
        controls.epoch_registered(EPOCH);
        let list = json!({ "type": "monitor-list", "monitors": [entry("DISPLAY1", true), entry("DISPLAY2", false)] });
        assert!(monitor_message(&controls, EPOCH, &list));
        controls
    }

    #[test]
    fn stale_epoch_error_does_not_clear_a_queued_pick() {
        let controls = connected();
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        let stale = json!({ "type": "monitor-error", "epoch": OTHER, "selector": "DISPLAY2", "reason": "busy" });
        assert!(
            monitor_message(&controls, EPOCH, &stale),
            "a stale epoch counts as handled"
        );
        let snapshot = controls.snapshot();
        assert_eq!(snapshot.pending.as_deref(), Some("DISPLAY2"));
        assert_eq!(snapshot.error, None);
        assert_eq!(
            controls.take_outbound(EPOCH).pick.as_deref(),
            Some("DISPLAY2")
        );
    }

    #[test]
    fn stale_epoch_error_does_not_clear_a_written_pick() {
        let controls = connected();
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        assert_eq!(
            controls.take_outbound(EPOCH).pick.as_deref(),
            Some("DISPLAY2")
        );
        let stale = json!({ "type": "monitor-error", "epoch": OTHER, "reason": "busy" });
        assert!(monitor_message(&controls, EPOCH, &stale));
        assert_eq!(
            controls.snapshot().pending.as_deref(),
            Some("DISPLAY2"),
            "still waiting for the host"
        );
        let current = json!({ "type": "monitor-error", "epoch": EPOCH, "selector": "DISPLAY2", "reason": "busy" });
        assert!(monitor_message(&controls, EPOCH, &current));
        let snapshot = controls.snapshot();
        assert_eq!(
            snapshot.pending, None,
            "the matching error answers the pick"
        );
        assert_eq!(
            snapshot.error.map(|error| error.reason).as_deref(),
            Some("busy")
        );
    }

    #[test]
    fn stale_epoch_list_is_not_applied_and_keeps_the_pending_pick() {
        let controls = connected();
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        let stale = json!({ "type": "monitor-list", "epoch": OTHER, "monitors": [entry("DISPLAY1", false), entry("DISPLAY2", true)] });
        assert!(monitor_message(&controls, EPOCH, &stale));
        let snapshot = controls.snapshot();
        assert_eq!(
            snapshot.selected.as_deref(),
            Some("DISPLAY1"),
            "the stale selection is ignored"
        );
        assert_eq!(snapshot.pending.as_deref(), Some("DISPLAY2"));
    }

    #[test]
    fn non_string_epoch_is_treated_as_stale() {
        let controls = connected();
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        let odd = json!({ "type": "monitor-error", "epoch": 7, "reason": "busy" });
        assert!(monitor_message(&controls, EPOCH, &odd));
        assert_eq!(controls.snapshot().pending.as_deref(), Some("DISPLAY2"));
    }

    #[test]
    fn matching_epoch_list_applies_and_clears_the_pending_pick() {
        let controls = connected();
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        let current = json!({ "type": "monitor-list", "epoch": EPOCH, "monitors": [entry("DISPLAY1", false), entry("DISPLAY2", true)] });
        assert!(monitor_message(&controls, EPOCH, &current));
        let snapshot = controls.snapshot();
        assert_eq!(snapshot.selected.as_deref(), Some("DISPLAY2"));
        assert_eq!(snapshot.pending, None);
    }

    #[test]
    fn messages_without_an_epoch_belong_to_the_current_socket() {
        let controls = connected();
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        let error = json!({ "type": "monitor-error", "selector": "DISPLAY2", "reason": "busy" });
        assert!(monitor_message(&controls, EPOCH, &error));
        assert_eq!(controls.snapshot().pending, None);
        assert_eq!(
            controls
                .snapshot()
                .error
                .map(|error| error.reason)
                .as_deref(),
            Some("busy")
        );
    }

    #[test]
    fn malformed_list_is_refused_and_changes_nothing() {
        let controls = connected();
        assert_eq!(controls.pick_monitor("DISPLAY2"), PickOutcome::Queued);
        let malformed = json!({ "type": "monitor-list", "monitors": "DISPLAY2" });
        assert!(
            !monitor_message(&controls, EPOCH, &malformed),
            "the caller counts it as rejected"
        );
        let snapshot = controls.snapshot();
        assert_eq!(snapshot.pending.as_deref(), Some("DISPLAY2"));
        assert_eq!(snapshot.selected.as_deref(), Some("DISPLAY1"));
    }

    #[test]
    fn only_monitor_switch_reasons_carry_held_input() {
        assert!(is_monitor_transition(
            &json!({ "reason": "monitor-switch" })
        ));
        assert!(is_monitor_transition(
            &json!({ "reason": "monitor-switched" })
        ));
        assert!(!is_monitor_transition(
            &json!({ "reason": "desktop-switched" })
        ));
        assert!(!is_monitor_transition(&json!({ "state": "unavailable" })));
    }
}

#[cfg(test)]
mod input_readiness_tests {
    use super::*;
    #[test]
    fn rtc_first_frame_messages_match_browser_epoch_and_retry_reason() {
        let request: Value =
            serde_json::from_str(&rtc_first_frame_request("test-request")).unwrap();
        assert_eq!(request["transport"], "webrtc");
        assert!(request.get("epoch").is_none());
        let retry: Value = serde_json::from_str(&rtc_first_frame_keyframe(Codec::H264)).unwrap();
        assert!(retry["epoch"].is_null());
        assert_eq!(retry["reason"], "first-frame-timeout");
        assert_eq!(retry["protocol"], 4);
        assert_eq!(retry["codec"], "h264");
    }
    #[test]
    fn desktop_state_reuses_inflight_but_never_completed_or_unavailable_presentation() {
        let mut epoch = Epoch {
            wire: "epoch".into(),
            ticket: "ticket".into(),
            request_id: "original".into(),
            codec: Codec::H264,
            host_present: true,
            presented: false,
            first_frame_deadline: Some(Instant::now() + Duration::from_secs(10)),
            first_frame_retried: false,
            first_frame_pending: false,
            input_generation: None,
            desktop_available: true,
            desktop_enabled: true,
            desktop_generation: Some(7),
            rtc_generation: 1,
            rtc_require_admission: false,
            rtc_admitted_timestamp: None,
        };
        assert!(!epoch.regate_desktop(Some(7), Duration::from_secs(10)));
        assert_eq!(epoch.request_id, "original");
        epoch.presented = true;
        epoch.input_generation = Some(7);
        epoch.first_frame_deadline = None;
        assert!(epoch.regate_desktop(Some(7), Duration::from_secs(10)));
        assert!(epoch.input_generation.is_none() && epoch.first_frame_pending && !epoch.presented);
        assert_ne!(epoch.request_id, "original");
        epoch.desktop_available = false;
        assert!(epoch.regate_desktop(Some(7), Duration::from_secs(10)));
        assert!(!epoch.first_frame_pending && epoch.first_frame_deadline.is_none());
        epoch.desktop_available = true;
        assert!(epoch.regate_desktop(Some(7), Duration::from_secs(10)));
        assert!(epoch.first_frame_pending && epoch.input_generation.is_none());
        assert!(epoch.regate_desktop(Some(8), Duration::from_secs(10)));
        assert_eq!(epoch.desktop_generation, Some(8));
    }
}
