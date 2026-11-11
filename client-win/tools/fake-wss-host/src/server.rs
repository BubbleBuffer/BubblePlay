//! The loopback WebSocket server: `/ws` (signalling, plays relay + host) and
//! `/media-ws?epoch=E` (media). Connection tasks only move bytes and forward
//! events; one hub task owns the model, the socket registry and the epoch.
//! Everything is bounded: connections by a semaphore, events and outbound
//! queues by fixed-size channels, and tasks by a JoinSet that is shut down at
//! the end of the run.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bp_device_key::phase2::{sdp_sha256_fingerprint, EphemeralSecret};
use bp_device_key::{relay_origin, verify_relay_proof, Role};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message};

use crate::audio::{self, AudioCounters, AudioLane, Script};
use crate::cursor;
use crate::input_record::{Received, Recorder};
use crate::model::{
    Action, Counters, Emitted, Injections, Model, ModelConfig, Recovery, Source, StartOutcome,
};
use crate::monitors;
use crate::phase2::{self, Channel, Handshake, Opened};
use crate::protocol::{self, ClientMessage};
use crate::rtc;
use crate::signal::{self, MediaControl, Signal};

const EVENT_DEPTH: usize = 1024;
const OUTBOUND_DEPTH: usize = 64;
/// Keep running this long past `exit_after` if frames are still in flight.
const HARD_TAIL: Duration = Duration::from_secs(2);
/// How long connection tasks get to send their close frames before abort.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
const MEDIA_PROTOCOL: u64 = 4;

pub type EpochMirror = Arc<Mutex<Option<String>>>;

#[derive(Clone)]
pub struct ServerConfig {
    /// The recordings on offer; `start` selects one by codec.
    pub sources: Vec<Arc<dyn Source>>,
    pub fps: u32,
    pub bitrate_bps: u64,
    pub injections: Injections,
    pub exit_after: Duration,
    /// No new frames in the last `drain` before `exit_after`, so ACKs settle.
    pub drain: Duration,
    pub max_connections: usize,
    /// Record every binary message received on `/ws` to this file.
    pub input_record: Option<PathBuf>,
    /// The audio lane's script, when `--audio` is given.
    pub audio: Option<Script>,
    /// The WebRTC answerer, when `--webrtc` is given.
    pub rtc: Option<rtc::Knobs>,
    /// The monitor list and its scripted failures, when `--monitors` is given.
    pub monitors: monitors::Knobs,
}

/// Everything the report needs, taken once the hub has finished.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub counters: Counters,
    pub recoveries: Vec<Recovery>,
    pub epochs: Vec<String>,
    pub in_flight_at_exit: usize,
    pub elapsed_ms: f64,
    pub hard_stop: bool,
    /// `None` when the run has no audio lane.
    pub audio: Option<AudioCounters>,
    /// One report per RTC session, in the order they ended. Empty without `--webrtc`.
    pub rtc: Vec<rtc::Report>,
}

pub async fn bind(addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Signal,
    Media,
}

#[derive(Debug)]
enum Outbound {
    Text(String),
    Binary(Vec<u8>),
    Close(CloseCode, &'static str),
}

#[derive(Debug)]
enum Event {
    Open {
        id: u64,
        kind: Kind,
        out: mpsc::Sender<Outbound>,
    },
    Text {
        id: u64,
        text: String,
    },
    /// A raw binary frame; after the handshake binary input travels in `ctl`, so it is refused.
    Binary {
        id: u64,
    },
    Closed {
        id: u64,
    },
    /// A media upgrade whose epoch did not match; refused at the handshake.
    StaleMedia,
}

struct Conn {
    kind: Kind,
    out: mpsc::Sender<Outbound>,
    hello: bool,
    /// A viewer hello that got its challenge and waits for `register-proof`.
    pending: Option<Pending>,
}

/// What a `register-proof` must answer, from the viewer's hello and the challenge.
struct Pending {
    id: String,
    public: String,
    nonce: [u8; 32],
}

struct FirstFrame {
    request_id: String,
    /// The answer (admitted, or ready on WSS) has been sent for this request.
    answered: bool,
}

/// The keyframe the RTC session admitted: its first RTP packet has left.
#[derive(Debug, Clone, Copy)]
struct RtcIdr {
    session: u64,
    generation: u32,
    rtp_timestamp: u32,
    codec: &'static str,
}

/// Progress through `cursor::SCRIPT`, armed by an accepted `start`.
#[derive(Debug, Clone, Copy)]
struct CursorScript {
    armed: Instant,
    next: usize,
}

/// What a new RTC session is built from.
struct RtcSettings {
    knobs: rtc::Knobs,
    sources: Vec<Arc<dyn Source>>,
    fps: u32,
}

/// The one RTC session `/ws` currently owns. Its pump is joined by the hub.
struct RtcSession {
    id: u64,
    commands: mpsc::Sender<rtc::Command>,
}

/// The viewer's `ctl` handshake on the signalling socket, from `auth-hello` to the open channel.
enum Link {
    /// `auth-host` is sent; waiting for `auth-finish`.
    Finish(Handshake),
    /// `auth-finish` verified; waiting for the viewer's `caps`.
    Caps(Channel),
    /// The `ctl` channel is up: every endpoint message goes sealed.
    Open(Channel),
}

struct Hub {
    model: Model,
    run_start: Instant,
    exit_after: Duration,
    drain: Duration,
    mirror: EpochMirror,
    epoch: Option<String>,
    /// The media ticket issued by the admitted `/ws` hello. Cleared with the epoch.
    ticket: Option<String>,
    /// The relay origin the viewers sign for: `relay_origin` of this server's `ws://` URL.
    origin: String,
    epochs: Vec<String>,
    conns: HashMap<u64, Conn>,
    signal: Option<u64>,
    media: Option<u64>,
    first_frame: Option<FirstFrame>,
    /// The last start was refused for codec mismatch; a first-frame request
    /// that follows is answered with a failure, as the browser's order implies.
    start_refused: bool,
    draining: bool,
    cursor: Option<CursorScript>,
    recorder: Option<Recorder>,
    /// The first recording failure; recording stops and the run fails.
    record_error: Option<String>,
    /// The audio lane, when the run has one.
    audio: Option<AudioLane>,
    /// A protocol-4 start with `audio:1` opted this media epoch in. It stays
    /// set across media reattaches and is cleared with the epoch.
    audio_wanted: bool,
    /// The RTC answerer, when the run has one.
    rtc: Option<RtcSettings>,
    rtc_session: Option<RtcSession>,
    rtc_next_id: u64,
    rtc_tasks: JoinSet<rtc::Report>,
    rtc_notices: rtc::Notices,
    rtc_reports: Vec<rtc::Report>,
    /// Ends the current RTC session once, this far into the run.
    rtc_drop_at: Option<Duration>,
    /// A WSS `start` took over: the RTC session stops its media but stays up.
    /// Cleared by the next offer.
    rtc_media_stopped: bool,
    /// The keyframe the current RTC session admitted, if one has left.
    rtc_idr: Option<RtcIdr>,
    /// The generation the current RTC session admits under. Keyframes that
    /// notify under another one were written before a monitor switch.
    rtc_generation: u32,
    /// The monitor surface (`--monitors`) and the index of the display in use.
    monitors: monitors::Knobs,
    selected: usize,
    /// The signalling viewer's `ctl` handshake. None until its `auth-hello`.
    link: Option<Link>,
    /// The admitted viewer's device ID, from its `register-proof`.
    viewer_id: Option<String>,
    /// The `a=fingerprint` of the viewer's offer and of the PC's answer, for `auth-bind`.
    offer_fingerprint: Option<String>,
    answer_fingerprint: Option<String>,
}

impl Hub {
    fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.run_start)
    }

    /// Sends one message to a connection. On the signalling socket the framing is
    /// `seal_for_signal`'s; other sockets get the message as it is.
    fn send(&mut self, id: u64, msg: Outbound) -> bool {
        let msg = if self.signal == Some(id) {
            match self.seal_for_signal(msg) {
                Some(msg) => msg,
                None => return false,
            }
        } else {
            msg
        };
        self.conns
            .get(&id)
            .is_some_and(|conn| conn.out.try_send(msg).is_ok())
    }

    /// Plain types go as they are. Every other text, and every binary frame (a cursor),
    /// goes sealed in `ctl` once the channel is open. Nothing else can be sent on `/ws`:
    /// `None` drops the message.
    fn seal_for_signal(&mut self, msg: Outbound) -> Option<Outbound> {
        match msg {
            Outbound::Text(text) if phase2::is_plain(&text) => Some(Outbound::Text(text)),
            Outbound::Text(text) => match self.link.as_mut() {
                Some(Link::Open(channel)) => channel.seal_text(&text).ok().map(Outbound::Text),
                _ => None,
            },
            Outbound::Binary(bytes) => match self.link.as_mut() {
                Some(Link::Open(channel)) => channel
                    .seal_frame(phase2::CURSOR_KIND, &bytes)
                    .ok()
                    .map(Outbound::Text),
                _ => None,
            },
            close @ Outbound::Close(..) => Some(close),
        }
    }

    /// Remove the connection and ask its task to close the socket. Dropping
    /// the sender also ends the task if the close frame cannot be queued.
    fn close_conn(&mut self, id: u64, reason: &'static str) {
        self.close_with(id, CloseCode::Normal, reason);
    }

    /// As `close_conn`, with close code 1008 (Policy): a failed admission.
    fn close_policy(&mut self, id: u64, reason: &'static str) {
        self.close_with(id, CloseCode::Policy, reason);
    }

    fn close_with(&mut self, id: u64, code: CloseCode, reason: &'static str) {
        if let Some(conn) = self.conns.remove(&id) {
            let _ = conn.out.try_send(Outbound::Close(code, reason));
        }
    }

    fn set_epoch(&mut self, value: Option<String>) {
        *self.mirror.lock().unwrap_or_else(|e| e.into_inner()) = value.clone();
        if value.is_none() {
            self.ticket = None;
        }
        self.epoch = value;
    }

    fn reject(&mut self) {
        self.model.counters.rejected_control += 1;
    }

    /// Record one binary message, with the epoch current at receipt. `channel`
    /// is `ws` for `/ws` and the data channel's label for RTC.
    fn record_input(&mut self, bytes: &[u8], channel: &str, now: Instant) {
        if self.recorder.is_none() {
            return;
        }
        let received_ms = self.elapsed(now).as_secs_f64() * 1000.0;
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis());
        let received = Received {
            epoch: self.epoch.as_deref(),
            channel,
            received_ms,
            unix_ms,
            bytes,
        };
        let Some(recorder) = self.recorder.as_mut() else {
            return;
        };
        if let Err(error) = recorder.record(received) {
            self.recorder = None;
            self.record_error = Some(format!("cannot record input: {error}"));
        }
    }

    /// A channel opening or the RTC session ending, as a non-packet line.
    fn record_lifecycle(&mut self, event: &str, channel: Option<&str>, now: Instant) {
        let received_ms = self.elapsed(now).as_secs_f64() * 1000.0;
        let Some(recorder) = self.recorder.as_mut() else {
            return;
        };
        if let Err(error) = recorder.lifecycle(self.epoch.as_deref(), event, channel, received_ms) {
            self.recorder = None;
            self.record_error = Some(format!("cannot record lifecycle: {error}"));
        }
    }
    fn input_disconnected(&mut self, reason: &str, now: Instant) {
        let elapsed = self.elapsed(now).as_secs_f64() * 1000.0;
        if let Some(recorder) = self.recorder.as_mut() {
            if let Err(error) = recorder.disconnect(self.epoch.as_deref(), reason, elapsed) {
                self.record_error = Some(format!("cannot record disconnect: {error}"));
            }
        }
    }

    /// Send every scripted cursor packet that is due, in order, to `/ws`.
    fn send_cursor_script(&mut self, now: Instant) {
        let Some(mut script) = self.cursor else {
            return;
        };
        let elapsed = now.saturating_duration_since(script.armed);
        while let Some(&(at_ms, packet)) = cursor::SCRIPT.get(script.next) {
            if elapsed < Duration::from_millis(at_ms) {
                break;
            }
            script.next += 1;
            if let Some(signal) = self.signal {
                self.send(signal, Outbound::Binary(packet.encode()));
            }
        }
        self.cursor = (script.next < cursor::SCRIPT.len()).then_some(script);
    }

    /// Audio runs only while it is wanted and a media socket is attached.
    fn sync_audio(&mut self, now: Instant) {
        let wanted = self.audio_wanted && self.media.is_some();
        if let Some(lane) = self.audio.as_mut() {
            if wanted {
                lane.start(now);
            } else {
                lane.stop();
            }
        }
    }

    fn stop_audio(&mut self) {
        if let Some(lane) = self.audio.as_mut() {
            lane.stop();
        }
    }

    /// A protocol-4 start with `audio:1`, whatever the video outcome.
    fn opt_in_audio(&mut self, now: Instant) {
        self.audio_wanted = true;
        if let Some(lane) = self.audio.as_mut() {
            lane.note_opt_in();
        }
        self.sync_audio(now);
    }

    /// One 10 ms step: encode what is due, then hand the queue to the media socket.
    fn audio_tick(&mut self, now: Instant) {
        let (Some(lane), Some(media)) = (self.audio.as_mut(), self.media) else {
            return;
        };
        lane.produce(now);
        let conns = &self.conns;
        lane.deliver(|bytes| {
            let Some(conn) = conns.get(&media) else {
                return audio::Delivery::Gone;
            };
            match conn.out.try_send(Outbound::Binary(bytes)) {
                Ok(()) => audio::Delivery::Sent,
                // The writer task has exited: the peer closed its socket, and
                // the hub has not processed that close yet.
                Err(mpsc::error::TrySendError::Closed(_)) => audio::Delivery::Gone,
                Err(mpsc::error::TrySendError::Full(_)) => audio::Delivery::Refused,
            }
        });
    }

    fn detach_media(&mut self, reason: &'static str) {
        if let Some(media) = self.media.take() {
            self.model.media_detach();
            self.stop_audio();
            self.close_conn(media, reason);
            if let Some(signal) = self.signal {
                self.send(signal, Outbound::Text(protocol::media_peer(false)));
            }
        }
    }

    fn handle(&mut self, event: Event, now: Instant) {
        match event {
            Event::Open { id, kind, out } => {
                self.conns.insert(
                    id,
                    Conn {
                        kind,
                        out,
                        hello: false,
                        pending: None,
                    },
                );
            }
            Event::Text { id, text } => match self.conns.get(&id).map(|c| c.kind) {
                Some(Kind::Signal) => self.on_signal_text(id, &text, now),
                Some(Kind::Media) => self.on_media_text(id, &text, now),
                None => {}
            },
            Event::Binary { id, .. } => {
                // Binary input travels as `ctl` kind `input` on /ws, so a raw frame is refused.
                if self.conns.contains_key(&id) {
                    self.reject();
                }
            }
            Event::Closed { id } => self.on_closed(id),
            Event::StaleMedia => self.model.counters.stale_epoch_control += 1,
        }
    }

    fn on_closed(&mut self, id: u64) {
        if self.conns.remove(&id).is_none() {
            return;
        }
        self.lost(id);
    }

    /// The teardown of a connection that is gone: its media or signalling role ends.
    /// Called on a close event, and directly when the hub closes the socket itself
    /// (its close event then finds no connection and does nothing).
    fn lost(&mut self, id: u64) {
        if self.media == Some(id) {
            self.media = None;
            self.model.media_detach();
            self.stop_audio();
            if let Some(signal) = self.signal {
                self.send(signal, Outbound::Text(protocol::media_peer(false)));
            }
        }
        if self.signal == Some(id) {
            self.input_disconnected("signal-closed", Instant::now());
            self.end_rtc("signal-closed");
            self.detach_media("signal-closed");
            self.audio_wanted = false;
            self.signal = None;
            self.model.stop();
            self.first_frame = None;
            self.cursor = None;
            self.set_epoch(None);
        }
    }

    /// Ends the RTC session, if any. The pump reports `reason` as its end and
    /// its socket closes; the browser sees the session go.
    fn end_rtc(&mut self, reason: &'static str) {
        if let Some(session) = self.rtc_session.take() {
            let _ = session.commands.try_send(rtc::Command::End(reason));
        }
    }

    fn count_unhandled(&mut self, kind: &str) {
        *self
            .model
            .counters
            .unhandled_control
            .entry(kind.to_owned())
            .or_insert(0) += 1;
    }

    /// A browser offer starts a new RTC session. Without `--webrtc` it is only
    /// counted, as any other unknown control is.
    fn on_offer(&mut self, sdp: String) {
        if self.rtc.is_none() {
            return self.count_unhandled("offer");
        }
        self.offer_fingerprint = sdp_sha256_fingerprint(&sdp).ok();
        self.answer_fingerprint = None;
        if self.offer_fingerprint.is_none() {
            // Decision 23: an offer without a sha-256 fingerprint cannot be bound.
            return self.refuse_unbindable_offer();
        }
        self.end_rtc("superseded");
        let Some((knobs, fps, sources)) = self
            .rtc
            .as_ref()
            .map(|settings| (settings.knobs, settings.fps, settings.sources.clone()))
        else {
            return;
        };
        let id = self.rtc_next_id;
        self.rtc_next_id += 1;
        self.rtc_generation = rtc::session_generation(id);
        let (commands, command_rx) = mpsc::channel(rtc::COMMAND_DEPTH);
        let session = rtc::Session {
            sources,
            fps,
            knobs,
            offer: sdp,
        };
        let notices = self.rtc_notices.clone();
        self.rtc_tasks
            .spawn(async move { rtc::run(session, id, command_rx, notices).await });
        self.rtc_session = Some(RtcSession { id, commands });
        self.rtc_media_stopped = false;
        self.rtc_idr = None;
    }

    /// WSS took over: the RTC session stops writing media and stays connected,
    /// as the browser keeps its peer and data channels alive.
    fn stop_rtc_media(&mut self) {
        self.rtc_media_stopped = true;
        if let Some(session) = self.rtc_session.as_ref() {
            let _ = session.commands.try_send(rtc::Command::StopMedia);
        }
    }

    /// Whether the first-frame answer belongs to WebRTC: an RTC run whose media
    /// has not been taken over by WSS. Such an answer waits for the keyframe.
    fn rtc_lane(&self) -> bool {
        self.rtc.is_some() && !self.rtc_media_stopped
    }

    /// Answers the pending WebRTC first-frame request with the admitted keyframe,
    /// once. It is sent only when the keyframe's first RTP packet has left, so a
    /// request that arrives earlier waits, and a retry after the admission
    /// joins that same keyframe.
    fn first_frame_admitted_rtc(&mut self, id: u64) {
        let Some(idr) = self.rtc_idr else { return };
        let current = self.rtc_session.as_ref().map(|session| session.id);
        if current != Some(idr.session) {
            return;
        }
        let Some(ff) = self.first_frame.as_mut() else {
            return;
        };
        if ff.answered {
            return;
        }
        ff.answered = true;
        let msg = protocol::first_frame_admitted_rtc(
            &ff.request_id,
            idr.generation,
            idr.rtp_timestamp,
            idr.codec,
            self.epoch.as_deref(),
        );
        self.send(id, Outbound::Text(msg));
    }

    /// A browser candidate goes to the current session. Trickle that arrives
    /// with no session to take it is dropped, as a stale candidate would be.
    fn on_ice(&mut self, candidate: String) {
        if self.rtc.is_none() {
            return self.count_unhandled("ice");
        }
        if let Some(session) = self.rtc_session.as_ref() {
            let _ = session.commands.try_send(rtc::Command::Remote(candidate));
        }
    }

    /// Acts on one notice from a pump. Notices from a session that is no
    /// longer current are dropped.
    fn handle_rtc(&mut self, id: u64, notice: rtc::Notice, now: Instant) {
        if !self
            .rtc_session
            .as_ref()
            .is_some_and(|session| session.id == id)
        {
            return;
        }
        let Some(signal) = self.signal else { return };
        match notice {
            rtc::Notice::Answer(sdp) => {
                self.answer_fingerprint = sdp_sha256_fingerprint(&sdp).ok();
                let text = serde_json::json!({"type": "answer", "sdp": sdp}).to_string();
                self.send(signal, Outbound::Text(text));
                self.send_pc_bind(signal);
            }
            rtc::Notice::Candidate(candidate) => {
                let text = serde_json::json!({"type": "ice", "candidate": candidate}).to_string();
                self.send(signal, Outbound::Text(text));
            }
            rtc::Notice::ChannelOpen(label) => {
                self.record_lifecycle("channel-open", Some(label.as_str()), now)
            }
            rtc::Notice::Input { channel, bytes } => self.record_input(&bytes, &channel, now),
            rtc::Notice::FirstKeyframe {
                generation,
                rtp_timestamp,
                codec,
            } => {
                // Written before a monitor switch: the picture it proves is gone.
                if generation != self.rtc_generation {
                    return;
                }
                self.rtc_idr = Some(RtcIdr {
                    session: id,
                    generation,
                    rtp_timestamp,
                    codec,
                });
                self.first_frame_admitted_rtc(signal);
            }
            rtc::Notice::Ended(_) => self.rtc_session = None,
            rtc::Notice::Connected | rtc::Notice::Disconnected => {}
        }
    }

    /// One pump task finished; its report is kept in the order it ended.
    fn rtc_joined(&mut self, joined: Result<rtc::Report, tokio::task::JoinError>) {
        let report = match joined {
            Ok(report) => report,
            Err(_) => rtc::Report::failed("failed:task"),
        };
        self.rtc_reports.push(report);
    }

    /// Ends every RTC session and joins its pump, with the same grace as the
    /// connections. A pump that outlives the grace is aborted.
    async fn close_rtc(&mut self, notices: &mut mpsc::Receiver<(u64, rtc::Notice)>) {
        self.rtc_session = None;
        let grace = tokio::time::sleep(CLOSE_GRACE);
        tokio::pin!(grace);
        while !self.rtc_tasks.is_empty() {
            tokio::select! {
                joined = self.rtc_tasks.join_next() => {
                    if let Some(joined) = joined {
                        self.rtc_joined(joined);
                    }
                }
                Some(_) = notices.recv() => {}
                _ = &mut grace => break,
            }
        }
        self.rtc_tasks.abort_all();
        while let Some(joined) = self.rtc_tasks.join_next().await {
            self.rtc_joined(joined);
        }
    }

    fn on_signal_text(&mut self, id: u64, text: &str, now: Instant) {
        if self.conns.get(&id).is_some_and(|c| c.pending.is_some()) {
            return self.on_register_proof(id, text, now);
        }
        let hello = self.conns.get(&id).is_some_and(|c| c.hello);
        let message = match signal::parse_signal(text) {
            Ok(message) => message,
            Err(_) => return self.reject(),
        };
        match message {
            Signal::Hello { role, body } => self.on_signal_hello(id, &role, &body),
            _ if !hello => self.reject(),
            Signal::Ping => {
                self.send(id, Outbound::Text(protocol::pong()));
            }
            // The plaintext handshake, then every endpoint message inside `ctl`.
            Signal::Other(kind) if kind == "auth-hello" => self.on_auth_hello(id, text),
            Signal::Other(kind) if kind == "auth-finish" => self.on_auth_finish(id, text),
            Signal::Other(kind) if kind == "auth-bind" => self.on_auth_bind(text),
            Signal::Other(kind) if kind == "ctl" => self.on_ctl(id, text, now),
            // Any other plaintext endpoint message is refused: after hello-ok they go in ctl.
            _ => self.reject(),
        }
    }

    /// The viewer's `auth-hello`, answered with `auth-host`. Only the admitted viewer,
    /// once, before its `auth-finish`.
    fn on_auth_hello(&mut self, id: u64, text: &str) {
        let (Some(viewer_id), Some(epoch)) = (self.viewer_id.clone(), self.epoch.clone()) else {
            return self.reject();
        };
        if self.signal != Some(id) || self.link.is_some() {
            return self.reject();
        }
        let ephemeral = EphemeralSecret::from_bytes(protocol::random32());
        match phase2::answer(
            text,
            &viewer_id,
            &self.origin,
            &epoch,
            protocol::random32(),
            ephemeral,
        ) {
            Ok((host, handshake)) => {
                self.link = Some(Link::Finish(handshake));
                self.send(id, Outbound::Text(host));
            }
            Err(reason) => self.fail_signal(id, reason),
        }
    }

    /// The viewer's `auth-finish`. A signature that does not verify closes the socket.
    fn on_auth_finish(&mut self, id: u64, text: &str) {
        match self.link.take() {
            Some(Link::Finish(handshake)) if self.signal == Some(id) => {
                match handshake.finish(text) {
                    Ok(channel) => self.link = Some(Link::Caps(channel)),
                    Err(reason) => self.fail_signal(id, reason),
                }
            }
            other => {
                self.link = other;
                self.reject();
            }
        }
    }

    /// One viewer `ctl` message. It must open. Its first is the viewer's `caps`; after
    /// that, a binary `input` frame or an endpoint message, handled as it arrives on /ws.
    fn on_ctl(&mut self, id: u64, text: &str, now: Instant) {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return self.reject();
        };
        let awaiting_caps = matches!(self.link, Some(Link::Caps(_)));
        let opened = match self.link.as_mut() {
            Some(Link::Caps(channel) | Link::Open(channel)) => channel.open(&value),
            _ => return self.reject(),
        };
        match opened {
            Err(reason) => self.fail_signal(id, reason),
            Ok(Opened::Text { kind, text }) if awaiting_caps => self.on_caps(id, &kind, &text),
            Ok(_) if awaiting_caps => self.fail_signal(id, "first ctl is not caps"),
            Ok(Opened::Text { text, .. }) => self.on_endpoint(id, &text, now),
            Ok(Opened::Binary { kind, frame }) if kind == phase2::INPUT_KIND => {
                self.on_signal_binary(&frame, now)
            }
            Ok(Opened::Binary { .. }) => self.reject(),
        }
    }

    /// The viewer's first `ctl`: its `caps`, which must offer `control-aead-v1`. The PC
    /// answers with its own `caps`, sealed, then sends its endpoint messages.
    fn on_caps(&mut self, id: u64, kind: &str, text: &str) {
        let offers = serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|value| value.get("caps").and_then(Value::as_array).cloned())
            .is_some_and(|caps| {
                caps.iter()
                    .any(|cap| cap.as_str() == Some(phase2::CAP_CONTROL_AEAD))
            });
        if kind != "caps" || !offers {
            return self.fail_signal(id, "viewer caps do not offer control-aead-v1");
        }
        let Some(Link::Caps(channel)) = self.link.take() else {
            return self.reject();
        };
        self.link = Some(Link::Open(channel));
        let caps = serde_json::json!({
            "type": "caps",
            "caps": [phase2::CAP_CONTROL_AEAD, phase2::CAP_DTLS_BIND],
        })
        .to_string();
        // Sealed by `send`, since the channel is open now.
        self.send(id, Outbound::Text(caps));
        self.send_post_handshake(id);
    }

    /// What the PC sends once the viewer's channel is up: the monitor list and, with
    /// audio, the audio capabilities. Each goes sealed in `ctl`.
    fn send_post_handshake(&mut self, id: u64) {
        self.send_monitor_list(id);
        if self.audio.is_some() {
            self.send(id, Outbound::Text(protocol::host_capabilities_audio()));
        }
    }

    /// An endpoint message from the viewer, inside a verified `ctl`.
    fn on_endpoint(&mut self, id: u64, text: &str, now: Instant) {
        let message = match signal::parse_signal(text) {
            Ok(message) => message,
            Err(_) => return self.reject(),
        };
        match message {
            Signal::Hello { .. } => self.reject(),
            Signal::Ping => {
                self.send(id, Outbound::Text(protocol::pong()));
            }
            Signal::Offer { sdp } => self.on_offer(sdp),
            Signal::Ice { candidate } => self.on_ice(candidate),
            Signal::MediaControl(control) => self.on_media_control(id, control, now),
            Signal::FirstFrame { phase, request_id } => {
                self.on_first_frame(id, &phase, &request_id)
            }
            Signal::Other(kind) => self.count_unhandled(&kind),
        }
    }

    /// A binary input frame from the viewer's `ctl` (kind `input`).
    fn on_signal_binary(&mut self, frame: &[u8], now: Instant) {
        if !crate::input_record::classify(frame).valid {
            self.reject();
        }
        self.record_input(frame, "ws", now);
    }

    /// The viewer's `auth-bind`, checked against its offer and the PC's answer. A failure
    /// is counted, and nothing else changes.
    fn on_auth_bind(&mut self, text: &str) {
        let (Some(local), Some(remote)) = (
            self.offer_fingerprint.clone(),
            self.answer_fingerprint.clone(),
        ) else {
            return self.reject();
        };
        let verified = match self.link.as_ref() {
            Some(Link::Open(channel)) => channel.verify_viewer_bind(text, &local, &remote).is_ok(),
            _ => false,
        };
        if !verified {
            self.reject();
        }
    }

    /// The PC's `auth-bind`, after its answer: signed over the answer's fingerprint, then
    /// the offer's.
    /// Decision 23: a `ctl` error `dtls-bind-required`, then the signalling viewer is closed.
    fn refuse_unbindable_offer(&mut self) {
        let Some(signal) = self.signal else { return };
        let error = serde_json::json!({
            "type": "error",
            "code": "dtls-bind-required",
            "message": "the offer has no sha-256 DTLS fingerprint",
        })
        .to_string();
        // Sealed directly: a plain `error` would read as the relay's own.
        let sealed = match self.link.as_mut() {
            Some(Link::Open(channel)) => channel.seal_text(&error).ok(),
            _ => None,
        };
        if let (Some(sealed), Some(conn)) = (sealed, self.conns.get(&signal)) {
            let _ = conn.out.try_send(Outbound::Text(sealed));
        }
        self.fail_signal(signal, "dtls-bind-required");
    }

    fn send_pc_bind(&mut self, id: u64) {
        let (Some(local), Some(remote)) = (
            self.answer_fingerprint.clone(),
            self.offer_fingerprint.clone(),
        ) else {
            return self.reject();
        };
        let bound = match self.link.as_ref() {
            Some(Link::Open(channel)) => channel.bind(&local, &remote).ok(),
            _ => None,
        };
        match bound {
            Some(text) => {
                self.send(id, Outbound::Text(text));
            }
            None => self.reject(),
        }
    }

    /// Refuses the signalling viewer: counted, closed with 1008, and torn down here, since
    /// its close event finds no connection.
    fn fail_signal(&mut self, id: u64, reason: &'static str) {
        self.reject();
        self.close_policy(id, reason);
        self.lost(id);
    }

    /// The first `/ws` hello. Only a viewer with the right keys gets a
    /// challenge; nothing is admitted, and no epoch rotates, until its proof
    /// verifies (see `on_register_proof`).
    fn on_signal_hello(&mut self, id: u64, role: &str, body: &Value) {
        let already = self.conns.get(&id).is_some_and(|c| c.hello);
        if already {
            return self.reject();
        }
        let hello = match (role, signal::parse_viewer_hello(body)) {
            ("viewer", Ok(hello)) => hello,
            _ => return self.close_policy(id, "bad-hello"),
        };
        let nonce = protocol::random32();
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.pending = Some(Pending {
                id: hello.id,
                public: hello.public,
                nonce,
            });
        }
        self.send(id, Outbound::Text(protocol::register_challenge(&nonce)));
    }

    /// The answer to the challenge. Any other message, or a proof that does not
    /// verify for this viewer, origin and nonce, closes with 1008 and admits nothing.
    fn on_register_proof(&mut self, id: u64, text: &str, now: Instant) {
        let pending = self.conns.get_mut(&id).and_then(|c| c.pending.take());
        let Some(pending) = pending else { return };
        if !self.proof_verifies(&pending, text) {
            return self.close_policy(id, "bad-proof");
        }
        self.viewer_id = Some(pending.id.clone());
        self.admit(id, now);
    }

    fn proof_verifies(&self, pending: &Pending, text: &str) -> bool {
        let Some(signature) = protocol::register_proof_signature(text) else {
            return false;
        };
        let Some(public) = protocol::b64url_decode(&pending.public) else {
            return false;
        };
        verify_relay_proof(
            Role::Viewer,
            &self.origin,
            &pending.nonce,
            &pending.id,
            &public,
            &signature,
        )
        .is_ok()
    }

    /// An admitted viewer rotates the epoch: the previous media socket is
    /// closed, the previous signalling socket is superseded, and the model
    /// starts a new generation that begins at a keyframe. The media ticket is
    /// issued here, for this signalling socket only.
    fn admit(&mut self, id: u64, now: Instant) {
        if self.epoch.is_some() {
            self.input_disconnected("epoch-rotated", now);
        }
        self.end_rtc("epoch-rotated");
        self.detach_media("epoch-rotated");
        self.audio_wanted = false;
        if let Some(old) = self.signal.take() {
            if old != id {
                self.close_conn(old, "superseded");
            }
        }
        self.model.begin_epoch(now);
        let epoch = protocol::new_epoch(self.model.counters.epochs_issued);
        self.set_epoch(Some(epoch.clone()));
        let ticket = protocol::b64url_encode(&protocol::random32());
        self.ticket = Some(ticket.clone());
        self.epochs.push(epoch.clone());
        self.signal = Some(id);
        self.link = None;
        self.offer_fingerprint = None;
        self.answer_fingerprint = None;
        self.first_frame = None;
        self.start_refused = false;
        self.cursor = None;
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.hello = true;
        }
        // The monitor list and the audio capabilities wait for the viewer's `ctl` handshake.
        self.send(
            id,
            Outbound::Text(protocol::hello_ok_signal(&epoch, &ticket)),
        );
    }

    fn on_media_control(&mut self, id: u64, control: MediaControl, now: Instant) {
        if self.signal != Some(id) {
            return self.reject();
        }
        if control.protocol.is_some_and(|p| p != MEDIA_PROTOCOL) {
            return self.reject();
        }
        if control.protocol == Some(MEDIA_PROTOCOL)
            && control.epoch.is_none()
            && !(control.action == "keyframe" && self.rtc_session.is_some() && self.media.is_none())
        {
            return self.reject();
        }
        if let Some(epoch) = &control.epoch {
            if self.epoch.as_deref() != Some(epoch.as_str()) {
                self.model.counters.stale_epoch_control += 1;
                return;
            }
        }
        if control.action == "start" {
            // The browser picked WSS: RTC stops its media, but the session lives on.
            self.stop_rtc_media();
        }
        if control.action == "start"
            && control.protocol == Some(MEDIA_PROTOCOL)
            && audio::opts_in(control.audio)
        {
            self.opt_in_audio(now);
        }
        match control.action.as_str() {
            "start" => match self.model.start(control.codec.as_deref()) {
                StartOutcome::CodecMismatch => {
                    self.start_refused = true;
                    if let Some(ff) = &self.first_frame {
                        let msg = protocol::first_frame(
                            "failed",
                            &ff.request_id,
                            None,
                            Some("codec-mismatch"),
                        );
                        self.send(id, Outbound::Text(msg));
                    }
                }
                StartOutcome::Started => {
                    self.start_refused = false;
                    self.cursor = Some(CursorScript {
                        armed: now,
                        next: 0,
                    });
                    self.send(id,Outbound::Text(serde_json::json!({"type":"input-capabilities","protocol":1,"scancode":true,"clipboard":false}).to_string()));
                }
                StartOutcome::AlreadyStreaming => self.start_refused = false,
            },
            "stop" => {
                self.model.stop();
                self.audio_wanted = false;
                self.sync_audio(now);
            }
            "keyframe" => {
                let reason = control.reason.as_deref().unwrap_or("unspecified");
                self.model.keyframe_request(reason, now);
            }
            "monitor" if self.monitors.enabled => {
                match monitors::bounded_selector(control.monitor.as_deref()) {
                    Some(selector) => self.on_monitor_switch(id, selector, now),
                    None => self.count_unhandled("monitor"),
                }
            }
            "monitors" if self.monitors.enabled && control.protocol == Some(MEDIA_PROTOCOL) => {
                self.model.counters.monitor_refreshes += 1;
                self.send_monitor_list(id);
            }
            "mode" => match control.stream_mode.as_deref() {
                Some(mode @ ("work" | "game")) => self.on_stream_mode(mode),
                _ => self.count_unhandled("mode"),
            },
            other => {
                *self
                    .model
                    .counters
                    .unhandled_control
                    .entry(other.to_owned())
                    .or_insert(0) += 1
            }
        }
    }

    /// The `monitor-list` for the selection in use. Sent only with `--monitors`.
    fn send_monitor_list(&mut self, id: u64) {
        if !self.monitors.enabled {
            return;
        }
        self.model.counters.monitor_list_sent += 1;
        self.model.counters.monitor_selected = self
            .monitors
            .monitors
            .get(self.selected)
            .map(|monitor| monitor.selector.clone());
        let text = monitors::monitor_list_message(&self.monitors.monitors, self.selected);
        self.send(id, Outbound::Text(text));
    }

    /// A `monitor` action, in the host's order: `desktop-state` unavailable,
    /// a generation bump with a keyframe, then the outcome, then `monitor-list`.
    /// A failed switch keeps the previous display, as the host does. The RTC
    /// session keeps its peer and admits the new keyframe under the new generation.
    fn on_monitor_switch(&mut self, id: u64, selector: &str, now: Instant) {
        // `Model::switch_monitor` counts the accepted switch.
        let before = self.model.generation();
        self.send(
            id,
            Outbound::Text(monitors::desktop_state_message(
                "unavailable",
                before,
                "monitor-switch",
            )),
        );
        let generation = self.model.switch_monitor(now);
        self.first_frame = None;
        self.rtc_idr = None;
        self.rtc_generation = generation;
        if let Some(session) = self.rtc_session.as_ref() {
            let _ = session.commands.try_send(rtc::Command::Reseek {
                generation: Some(generation),
            });
        }
        let known = self.monitors.position(selector).is_some();
        match self.monitors.position(selector) {
            Some(index) if !self.monitors.fails(selector) => self.selected = index,
            _ => {
                let reason = if known {
                    monitors::SCRIPTED_ERROR_REASON
                } else {
                    monitors::NOT_FOUND_REASON
                };
                self.model.counters.monitor_switch_failures += 1;
                self.send(
                    id,
                    Outbound::Text(monitors::monitor_error_message(selector, reason)),
                );
            }
        }
        self.send(
            id,
            Outbound::Text(monitors::desktop_state_message(
                "active",
                generation,
                "monitor-switched",
            )),
        );
        self.send_monitor_list(id);
    }

    /// A `mode` action with a known mode. A real change forces a keyframe on
    /// the WSS model and on the RTC session; the same mode is a no-op.
    fn on_stream_mode(&mut self, mode: &str) {
        if !self.model.set_stream_mode(mode) {
            return;
        }
        if let Some(session) = self.rtc_session.as_ref() {
            let _ = session
                .commands
                .try_send(rtc::Command::Reseek { generation: None });
        }
    }

    fn on_first_frame(&mut self, id: u64, phase: &str, request_id: &str) {
        match phase {
            "request" => {
                self.model.counters.first_frame_requests += 1;
                self.first_frame = Some(FirstFrame {
                    request_id: request_id.to_owned(),
                    answered: false,
                });
                if self.start_refused {
                    let msg =
                        protocol::first_frame("failed", request_id, None, Some("codec-mismatch"));
                    self.send(id, Outbound::Text(msg));
                } else if self.rtc_lane() {
                    // WebRTC admits only once the keyframe's RTP is on the wire.
                    self.first_frame_admitted_rtc(id);
                } else {
                    let msg = protocol::first_frame(
                        "admitted",
                        request_id,
                        Some(self.model.generation()),
                        None,
                    );
                    self.send(id, Outbound::Text(msg));
                }
            }
            "presented" => {
                if self
                    .first_frame
                    .as_ref()
                    .is_some_and(|ff| ff.request_id == request_id)
                {
                    self.model.counters.first_frame_presented += 1;
                } else {
                    self.reject();
                }
            }
            _ => self.reject(),
        }
    }

    fn on_media_text(&mut self, id: u64, text: &str, now: Instant) {
        let hello = self.conns.get(&id).is_some_and(|c| c.hello);
        if !hello {
            return match signal::parse_signal(text) {
                Ok(Signal::Hello { role, body }) => self.on_media_hello(id, &role, &body, now),
                _ => self.reject(),
            };
        }
        if self.media != Some(id) {
            return;
        }
        if !signal::media_text_fits(text) {
            return self.reject();
        }
        match protocol::parse_client_message(text) {
            Ok(ClientMessage::Ack {
                generation, seq, ..
            }) => {
                self.model.ack(now, generation, seq);
            }
            Ok(ClientMessage::Keyframe { reason }) => self.model.keyframe_request(&reason, now),
            Ok(ClientMessage::Ping) => {
                self.model.counters.media_pings += 1;
                self.send(id, Outbound::Text(protocol::pong()));
            }
            Err(_) => self.reject(),
        }
    }

    /// The media hello: role `media-viewer` and the ticket of the current
    /// signalling socket. Anything else is closed as before.
    fn on_media_hello(&mut self, id: u64, role: &str, body: &Value, now: Instant) {
        let ticket = body.get("ticket").and_then(Value::as_str);
        let admitted = role == "media-viewer"
            && self.epoch.is_some()
            && ticket.is_some()
            && ticket == self.ticket.as_deref();
        if !admitted {
            self.reject();
            self.close_conn(id, "bad-media-hello");
            return;
        }
        self.detach_media("superseded");
        self.media = Some(id);
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.hello = true;
        }
        self.model.media_attach();
        self.sync_audio(now);
        let registered = self.elapsed(now).as_millis() as u64;
        self.send(id, Outbound::Text(protocol::media_hello_ok(registered)));
        self.send(id, Outbound::Text(protocol::media_peer(true)));
        if let Some(signal) = self.signal {
            self.send(signal, Outbound::Text(protocol::media_peer(true)));
        }
    }

    /// The socket-drop injection: both sockets go, the epoch is cleared, and
    /// the client has to say hello again to get a new one.
    fn drop_sockets(&mut self) {
        self.input_disconnected("socket-drop", Instant::now());
        self.end_rtc("socket-drop");
        if let Some(signal) = self.signal.take() {
            self.close_conn(signal, "socket-drop");
        }
        if let Some(media) = self.media.take() {
            self.close_conn(media, "socket-drop");
        }
        self.audio_wanted = false;
        self.stop_audio();
        self.first_frame = None;
        self.cursor = None;
        self.link = None;
        self.viewer_id = None;
        self.offer_fingerprint = None;
        self.answer_fingerprint = None;
        self.set_epoch(None);
    }

    fn emit(&mut self, emitted: Emitted) {
        let Emitted {
            packet,
            key,
            generation,
            ..
        } = emitted;
        let Some(media) = self.media else { return };
        if !self.send(media, Outbound::Binary(packet)) {
            self.model.counters.outbound_overflow += 1;
        }
        if !key {
            return;
        }
        self.first_frame_ready(generation);
    }

    /// Answers the pending WSS first-frame request once, on the first keyframe.
    fn first_frame_ready(&mut self, generation: u32) {
        let ready = match &mut self.first_frame {
            Some(ff) if !ff.answered => {
                ff.answered = true;
                Some(ff.request_id.clone())
            }
            _ => None,
        };
        if let (Some(request_id), Some(signal)) = (ready, self.signal) {
            let msg = protocol::first_frame("ready", &request_id, Some(generation), None);
            self.send(signal, Outbound::Text(msg));
        }
    }

    /// One pacing step. Returns true when the run is over.
    fn tick(&mut self, now: Instant) -> bool {
        let elapsed = self.elapsed(now);
        if !self.draining && elapsed + self.drain >= self.exit_after {
            self.draining = true;
            self.model.set_draining();
        }
        if self.rtc_drop_at.is_some_and(|at| elapsed >= at) {
            self.rtc_drop_at = None;
            self.end_rtc("rtc-dropped");
        }
        self.send_cursor_script(now);
        let in_flight = self.model.in_flight();
        if elapsed >= self.exit_after && (in_flight == 0 || elapsed >= self.exit_after + HARD_TAIL)
        {
            return true;
        }
        match self.model.tick(now) {
            Action::Emit(emitted) => self.emit(emitted),
            Action::SocketDrop => self.drop_sockets(),
            Action::Idle | Action::Held => {}
        }
        false
    }

    fn finish(mut self, now: Instant, rtc: Vec<rtc::Report>) -> Snapshot {
        self.input_disconnected("run-complete", now);
        // Stopping counts whatever is still queued or held as discarded, so the
        // audio accounting identity holds at exit. It also counts as a stop.
        self.stop_audio();
        let audio = self.audio.as_ref().map(|lane| lane.counters().clone());
        let in_flight_at_exit = self.model.in_flight();
        let elapsed_ms = self.elapsed(now).as_secs_f64() * 1000.0;
        let ids: Vec<u64> = self.conns.keys().copied().collect();
        for id in ids {
            self.close_conn(id, "run-complete");
        }
        Snapshot {
            hard_stop: in_flight_at_exit > 0,
            counters: self.model.counters.clone(),
            recoveries: self.model.recoveries.clone(),
            epochs: self.epochs,
            in_flight_at_exit,
            elapsed_ms,
            audio,
            rtc,
        }
    }
}

/// Returns the snapshot and the first input-recording error, if any.
async fn run_hub(
    mut hub: Hub,
    mut events: mpsc::Receiver<Event>,
    mut notices: mpsc::Receiver<(u64, rtc::Notice)>,
) -> (Snapshot, Option<String>) {
    let mut ticker = tokio::time::interval(hub.model.frame_period());
    // Windows timer wakes can be several milliseconds late. Preserve the
    // recording's 60 Hz cadence instead of silently skipping those frames;
    // the model's four-frame/150 ms in-flight limits bound catch-up work.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let mut audio_ticker = tokio::time::interval(audio::FRAME_PERIOD);
    audio_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => hub.handle(event, Instant::now()),
                None => break,
            },
            _ = ticker.tick() => {
                if hub.tick(Instant::now()) {
                    break;
                }
            }
            _ = audio_ticker.tick(), if hub.audio.is_some() => hub.audio_tick(Instant::now()),
            Some((id, notice)) = notices.recv() => hub.handle_rtc(id, notice, Instant::now()),
            Some(joined) = hub.rtc_tasks.join_next(), if !hub.rtc_tasks.is_empty() => {
                hub.rtc_joined(joined);
            }
        }
    }
    hub.close_rtc(&mut notices).await;
    let record_error = hub.record_error.take();
    let reports = std::mem::take(&mut hub.rtc_reports);
    (hub.finish(Instant::now(), reports), record_error)
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| value.to_owned())
    })
}

fn refuse(status: StatusCode, body: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(body.to_owned()));
    *response.status_mut() = status;
    response
}

async fn connection(
    tcp: TcpStream,
    _permit: OwnedSemaphorePermit,
    id: u64,
    events: mpsc::Sender<Event>,
    mirror: EpochMirror,
) {
    let mut kind: Option<Kind> = None;
    let stale_events = events.clone();
    // tungstenite fixes the callback's Err type, so ErrorResponse cannot be boxed here.
    #[allow(clippy::result_large_err)]
    let callback = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        match request.uri().path() {
            "/ws" => {
                kind = Some(Kind::Signal);
                Ok(response)
            }
            "/media-ws" => {
                let query = request.uri().query().unwrap_or("");
                let asked = query_value(query, "epoch");
                let current = mirror.lock().unwrap_or_else(|e| e.into_inner()).clone();
                match (asked, current) {
                    (Some(asked), Some(current)) if asked == current => {
                        kind = Some(Kind::Media);
                        Ok(response)
                    }
                    _ => {
                        let _ = stale_events.try_send(Event::StaleMedia);
                        Err(refuse(StatusCode::CONFLICT, "stale-epoch"))
                    }
                }
            }
            _ => Err(refuse(StatusCode::NOT_FOUND, "not-found")),
        }
    };
    let ws = match tokio_tungstenite::accept_hdr_async(tcp, callback).await {
        Ok(ws) => ws,
        Err(_) => return,
    };
    let Some(kind) = kind else { return };

    let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_DEPTH);
    if events
        .send(Event::Open {
            id,
            kind,
            out: out_tx,
        })
        .await
        .is_err()
    {
        return;
    }
    let (mut sink, mut stream) = ws.split();
    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if events.send(Event::Text { id, text: text.to_string() }).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Binary(_))) => {
                    let _ = events.send(Event::Binary { id }).await;
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            outgoing = out_rx.recv() => match outgoing {
                Some(Outbound::Text(text)) => {
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Some(Outbound::Binary(bytes)) => {
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                Some(Outbound::Close(code, reason)) => {
                    let frame = CloseFrame { code, reason: reason.into() };
                    let _ = sink.send(Message::Close(Some(frame))).await;
                    break;
                }
                None => break,
            },
        }
    }
    let _ = events.send(Event::Closed { id }).await;
}

/// Serve until `exit_after`, then drain, close every socket and return.
pub async fn serve(listener: TcpListener, config: ServerConfig) -> Result<Snapshot> {
    // The origin a viewer signs for is the one of the `ws://` URL this server
    // is reachable at, as the client computes it (`relay_origin`).
    let bound = listener
        .local_addr()
        .context("the listener has no local address")?;
    let origin =
        relay_origin(&format!("ws://{bound}")).context("no relay origin for the listener")?;
    let mirror: EpochMirror = Arc::new(Mutex::new(None));
    let (events_tx, events_rx) = mpsc::channel(EVENT_DEPTH);
    let recorder = config
        .input_record
        .as_deref()
        .map(|path| {
            Recorder::create(path)
                .with_context(|| format!("cannot create input record {}", path.display()))
        })
        .transpose()?;
    let run_start = Instant::now();
    let audio = config
        .audio
        .map(|script| AudioLane::new_wss(script, run_start))
        .transpose()
        .context("cannot start the audio encoder")?;
    let rtc_drop_at = config.rtc.and_then(|knobs| knobs.drop_at);
    let rtc = config.rtc.map(|knobs| RtcSettings {
        knobs,
        sources: config.sources.clone(),
        fps: config.fps,
    });
    let (notices_tx, notices_rx) = mpsc::channel(rtc::NOTICE_DEPTH);
    let model = Model::new(ModelConfig {
        sources: config.sources,
        fps: config.fps,
        bitrate_bps: config.bitrate_bps,
        injections: config.injections,
        run_start,
    });
    let hub = Hub {
        model,
        run_start,
        exit_after: config.exit_after,
        drain: config.drain,
        mirror: mirror.clone(),
        epoch: None,
        ticket: None,
        origin,
        epochs: Vec::new(),
        conns: HashMap::new(),
        signal: None,
        media: None,
        first_frame: None,
        start_refused: false,
        draining: false,
        cursor: None,
        recorder,
        record_error: None,
        audio,
        audio_wanted: false,
        rtc,
        rtc_session: None,
        rtc_next_id: 0,
        rtc_tasks: JoinSet::new(),
        rtc_notices: notices_tx,
        rtc_reports: Vec::new(),
        rtc_drop_at,
        rtc_media_stopped: false,
        rtc_idr: None,
        rtc_generation: 0,
        monitors: config.monitors.clone(),
        selected: 0,
        link: None,
        viewer_id: None,
        offer_fingerprint: None,
        answer_fingerprint: None,
    };
    let mut hub_task = tokio::spawn(run_hub(hub, events_rx, notices_rx));
    let slots = Arc::new(Semaphore::new(config.max_connections));
    let mut conns: JoinSet<()> = JoinSet::new();
    let mut next_id = 0u64;

    let (snapshot, record_error) = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((tcp, _)) = accepted else { continue };
                if tcp.set_nodelay(true).is_err() {
                    continue;
                }
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    drop(tcp);
                    continue;
                };
                next_id += 1;
                conns.spawn(connection(tcp, permit, next_id, events_tx.clone(), mirror.clone()));
            }
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            joined = &mut hub_task => break joined.context("hub task failed")?,
        }
    };

    drop(listener);
    drop(events_tx);
    let _ = tokio::time::timeout(CLOSE_GRACE, async {
        while conns.join_next().await.is_some() {}
    })
    .await;
    conns.shutdown().await;
    if let Some(error) = record_error {
        anyhow::bail!(error);
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_are_read_by_exact_key() {
        assert_eq!(
            query_value("epoch=abc&x=1", "epoch").as_deref(),
            Some("abc")
        );
        assert_eq!(query_value("xepoch=abc", "epoch"), None);
        assert_eq!(query_value("", "epoch"), None);
        assert_eq!(
            query_value("epoch", "epoch"),
            None,
            "a bare key has no value"
        );
    }

    #[test]
    fn refusals_carry_their_status() {
        let response = refuse(StatusCode::CONFLICT, "stale-epoch");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(response.body().as_deref(), Some("stale-epoch"));
    }
}
