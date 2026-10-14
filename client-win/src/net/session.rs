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
use tokio_tungstenite::tungstenite::http::header::{COOKIE, ORIGIN};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};

use super::config::{Codec, NetConfig, RelayEndpoint};
use super::error::NetError;
use super::events::{DecoderFeedback, NetEvent, ResetEvent, VideoFrame};
use super::gate::{next_backoff, GateVerdict, KeyframeThrottle, ReceiveGate};
use super::protocol::{
    ack_message, first_frame_keyframe, first_frame_presented, first_frame_request, hello_message,
    is_audio_packet, keyframe_message, new_request_id, parse_frame, parse_relay_control,
    ping_message, sanitize_reason, start_message, valid_epoch, validate_host_control, RelayControl,
    V4_HEADER_BYTES,
};
use super::stats::{count, NetState, TransportStats};

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
}

/// State of one `/ws` epoch: the wire epoch, the first-frame handshake and the
/// `host-joined` tracking. Replaced on every reconnect.
struct Epoch {
    wire: String,
    request_id: String,
    codec: Codec,
    host_present: bool,
    presented: bool,
    first_frame_deadline: Option<Instant>,
    first_frame_retried: bool,
}

struct Link {
    signal: Lane,
    media: Lane,
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
            match self.attempt(&endpoint).await {
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
        self.local_epoch += 1;
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

    /// One full attempt: `/ws` hello, start and first-frame request, `/media-ws`
    /// hello, then the stream loop. Always ends with `Err`.
    async fn attempt(&mut self, endpoint: &RelayEndpoint) -> Result<(), Halt> {
        let handshake_started = Instant::now();
        self.begin_epoch();
        let cookie = self.config.cookies.cookie_header();
        let codec = self.config.codec;

        let signal_socket = self
            .handshake(open_socket(endpoint, cookie.clone(), "/ws", None))
            .await?;
        let (signal_tx, mut signal_rx) = signal_socket.split();
        let mut signal = Lane::new(signal_tx);
        signal.send(hello_message("client")).await?;
        let media_epoch = self
            .handshake(await_hello_ok(&mut signal_rx, &self.stats, "signal"))
            .await?;
        let wire = match media_epoch {
            Some(epoch) if valid_epoch(&epoch) => epoch,
            _ => {
                return Err(Halt::from(NetError::Protocol(
                    "hello-ok without a valid mediaEpoch".to_owned(),
                )))
            }
        };

        eprintln!(
            "BP_TIMING signal_ready: {:.3}ms",
            handshake_started.elapsed().as_secs_f64() * 1000.0
        );
        let request_id = new_request_id();
        self.stats.set_state(NetState::MediaHandshake);

        let query = format!("epoch={wire}");
        let media_socket = self
            .handshake(open_socket(endpoint, cookie, "/media-ws", Some(&query)))
            .await?;
        let (media_tx, mut media_rx) = media_socket.split();
        let mut media = Lane::new(media_tx);
        media.send(hello_message("media-client")).await?;
        let host_present = self
            .handshake(await_media_ready(&mut media_rx, &self.stats))
            .await?;

        eprintln!(
            "BP_TIMING media_ready_total: {:.3}ms",
            handshake_started.elapsed().as_secs_f64() * 1000.0
        );
        // Media must be registered before the host is asked to start. The
        // relay can otherwise discard start or frames on an unpaired socket.
        signal.send(start_message(&wire, codec)).await?;
        signal.send(first_frame_request(&request_id, &wire)).await?;
        let first_frame_deadline = Instant::now() + self.timing.first_frame;

        count(&self.stats.established);
        self.backoff = Duration::ZERO;
        self.ever_opened = true;
        self.stats.set_state(NetState::Starting);

        let mut link = Link {
            signal,
            media,
            epoch: Epoch {
                wire,
                request_id,
                codec,
                host_present,
                presented: false,
                first_frame_deadline: Some(first_frame_deadline),
                first_frame_retried: false,
            },
        };
        self.serve(&mut link, signal_rx, media_rx).await
    }

    async fn serve(
        &mut self,
        link: &mut Link,
        mut signal_rx: Source,
        mut media_rx: Source,
    ) -> Result<(), Halt> {
        let result = async {
            loop {
                if self.stop_requested() {
                    return Err(Halt::Stop);
                }
                let wake = tokio::select! {
                    item = media_rx.next() => Wake::Media(item),
                    item = signal_rx.next() => Wake::Signal(item),
                    _ = sleep(self.timing.tick) => Wake::Tick,
                };
                match wake {
                    Wake::Media(item) => {
                        let inbound = classify(item, "media")?;
                        link.media.last_seen = Instant::now();
                        self.on_media(link, inbound).await?;
                    }
                    Wake::Signal(item) => {
                        let inbound = classify(item, "signal")?;
                        link.signal.last_seen = Instant::now();
                        self.on_signal(inbound)?;
                    }
                    Wake::Tick => {}
                }
                self.tick(link).await?;
            }
        }
        .await;
        if matches!(result, Err(Halt::Stop)) {
            self.shutdown(link, &mut signal_rx, &mut media_rx).await;
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
            close_lane(&mut link.media, media_rx),
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

    /// A BPV4 frame: parse, ACK, then gate and deliver. BPA1 is ignored.
    async fn on_media_packet(
        &mut self,
        link: &mut Link,
        mut data: Vec<u8>,
        decode: bool,
    ) -> Result<(), Halt> {
        let received_at = Instant::now();
        if is_audio_packet(&data) {
            count(&self.stats.bpa1_ignored);
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

        // ACK before the gate: every valid frame is acknowledged, dropped or not.
        let recv_ts_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        link.media
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

    fn on_signal(&self, inbound: Inbound) -> Result<(), Halt> {
        match inbound {
            Inbound::Text(text) => {
                if let Some(message) = relay_error(&text) {
                    return Err(Halt::from(NetError::RelayError(message)));
                }
                match parse_relay_control(&text) {
                    Some(RelayControl::Pong) => {}
                    _ => count(&self.stats.rejected_controls),
                }
            }
            Inbound::Binary(_) => count(&self.stats.rejected_controls),
            Inbound::Other => {}
        }
        Ok(())
    }

    /// Per-tick work: feedback, pending reset and keyframe, heartbeats, dead
    /// detection and the first-frame watchdog.
    async fn tick(&mut self, link: &mut Link) -> Result<(), Halt> {
        if self.stop_requested() {
            return Err(Halt::Stop);
        }
        self.drain_feedback(link).await?;
        self.flush_reset()?;
        self.send_pending_keyframe(link).await?;

        let now = Instant::now();
        if now.duration_since(link.signal.last_seen) >= self.timing.dead {
            return Err(Halt::from(NetError::Stalled("signal")));
        }
        if now.duration_since(link.media.last_seen) >= self.timing.dead {
            return Err(Halt::from(NetError::Stalled("media")));
        }
        if now.duration_since(link.signal.last_sent) >= self.timing.heartbeat {
            link.signal.send(ping_message()).await?;
            count(&self.stats.heartbeats_sent);
        }
        if now.duration_since(link.media.last_sent) >= self.timing.heartbeat {
            link.media.send(ping_message()).await?;
            count(&self.stats.heartbeats_sent);
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
            }
            DecoderFeedback::Presented { generation, .. } => {
                if self.gate_generation != Some(generation) {
                    return Ok(());
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
            link.media.send(keyframe_message(&reason)).await?;
            self.stats.record_keyframe();
        }
        Ok(())
    }

    /// The first-frame watchdog: one resend of the request and a
    /// `first-frame-timeout` keyframe, then a reconnect.
    async fn check_first_frame(&mut self, link: &mut Link) -> Result<(), Halt> {
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

/// Open one WebSocket with `Origin` and the cookie header.
async fn open_socket(
    endpoint: &RelayEndpoint,
    cookie: Option<String>,
    path: &str,
    query: Option<&str>,
) -> Result<Socket, NetError> {
    let url = endpoint.ws_url(path, query);
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|error| NetError::Handshake(error.to_string()))?;
    let headers = request.headers_mut();
    headers.insert(ORIGIN, header_value(endpoint.origin())?);
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, header_value(&cookie)?);
    }
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

fn header_value(value: &str) -> Result<HeaderValue, NetError> {
    HeaderValue::from_str(value).map_err(|error| NetError::Handshake(error.to_string()))
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

/// Read until the relay's `hello-ok`. Returns its `mediaEpoch`, if it has one.
async fn await_hello_ok(
    source: &mut Source,
    stats: &TransportStats,
    label: &str,
) -> Result<Option<String>, NetError> {
    loop {
        match classify(source.next().await, label)? {
            Inbound::Text(text) => {
                if let Some(message) = relay_error(&text) {
                    return Err(NetError::RelayError(message));
                }
                match parse_relay_control(&text) {
                    Some(RelayControl::HelloOk { media_epoch }) => return Ok(media_epoch),
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
