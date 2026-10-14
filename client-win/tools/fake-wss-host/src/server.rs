//! The loopback WebSocket server: `/ws` (signalling, plays relay + host) and
//! `/media-ws?epoch=E` (media). Connection tasks only move bytes and forward
//! events; one hub task owns the model, the socket registry and the epoch.
//! Everything is bounded: connections by a semaphore, events and outbound
//! queues by fixed-size channels, and tasks by a JoinSet that is shut down at
//! the end of the run.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message};

use crate::model::{
    Action, Counters, Emitted, Injections, Model, ModelConfig, Recovery, Source, StartOutcome,
};
use crate::protocol::{self, ClientMessage};
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
    Close(&'static str),
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
}

struct FirstFrame {
    request_id: String,
    ready_sent: bool,
}

struct Hub {
    model: Model,
    run_start: Instant,
    exit_after: Duration,
    drain: Duration,
    mirror: EpochMirror,
    epoch: Option<String>,
    epochs: Vec<String>,
    conns: HashMap<u64, Conn>,
    signal: Option<u64>,
    media: Option<u64>,
    first_frame: Option<FirstFrame>,
    /// The last start was refused for codec mismatch; a first-frame request
    /// that follows is answered with a failure, as the browser's order implies.
    start_refused: bool,
    draining: bool,
}

impl Hub {
    fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.run_start)
    }

    fn send(&self, id: u64, msg: Outbound) -> bool {
        self.conns
            .get(&id)
            .is_some_and(|conn| conn.out.try_send(msg).is_ok())
    }

    /// Remove the connection and ask its task to close the socket. Dropping
    /// the sender also ends the task if the close frame cannot be queued.
    fn close_conn(&mut self, id: u64, reason: &'static str) {
        if let Some(conn) = self.conns.remove(&id) {
            let _ = conn.out.try_send(Outbound::Close(reason));
        }
    }

    fn set_epoch(&mut self, value: Option<String>) {
        *self.mirror.lock().unwrap_or_else(|e| e.into_inner()) = value.clone();
        self.epoch = value;
    }

    fn reject(&mut self) {
        self.model.counters.rejected_control += 1;
    }

    fn detach_media(&mut self, reason: &'static str) {
        if let Some(media) = self.media.take() {
            self.model.media_detach();
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
                    },
                );
            }
            Event::Text { id, text } => match self.conns.get(&id).map(|c| c.kind) {
                Some(Kind::Signal) => self.on_signal_text(id, &text, now),
                Some(Kind::Media) => self.on_media_text(id, &text, now),
                None => {}
            },
            Event::Binary { id } => {
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
        if self.media == Some(id) {
            self.media = None;
            self.model.media_detach();
            if let Some(signal) = self.signal {
                self.send(signal, Outbound::Text(protocol::media_peer(false)));
            }
        }
        if self.signal == Some(id) {
            self.detach_media("signal-closed");
            self.signal = None;
            self.model.stop();
            self.first_frame = None;
            self.set_epoch(None);
        }
    }

    fn on_signal_text(&mut self, id: u64, text: &str, now: Instant) {
        let hello = self.conns.get(&id).is_some_and(|c| c.hello);
        let message = match signal::parse_signal(text) {
            Ok(message) => message,
            Err(_) => return self.reject(),
        };
        match message {
            Signal::Hello { role } => self.on_signal_hello(id, &role, now),
            _ if !hello => self.reject(),
            Signal::Ping => {
                self.send(id, Outbound::Text(protocol::pong()));
            }
            Signal::MediaControl(control) => self.on_media_control(id, control, now),
            Signal::FirstFrame { phase, request_id } => {
                self.on_first_frame(id, &phase, &request_id)
            }
            Signal::Other(kind) => {
                *self
                    .model
                    .counters
                    .unhandled_control
                    .entry(kind)
                    .or_insert(0) += 1
            }
        }
    }

    /// A client hello on `/ws` rotates the epoch: the previous media socket
    /// is closed, the previous signalling socket is superseded, and the model
    /// starts a new generation that begins at a keyframe.
    fn on_signal_hello(&mut self, id: u64, role: &str, now: Instant) {
        let already = self.conns.get(&id).is_some_and(|c| c.hello);
        if role != "client" || already {
            return self.reject();
        }
        self.detach_media("epoch-rotated");
        if let Some(old) = self.signal.take() {
            if old != id {
                self.close_conn(old, "superseded");
            }
        }
        self.model.begin_epoch(now);
        let epoch = protocol::new_epoch(self.model.counters.epochs_issued);
        self.set_epoch(Some(epoch.clone()));
        self.epochs.push(epoch.clone());
        self.signal = Some(id);
        self.first_frame = None;
        self.start_refused = false;
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.hello = true;
        }
        self.send(id, Outbound::Text(protocol::hello_ok_signal(&epoch)));
    }

    fn on_media_control(&mut self, id: u64, control: MediaControl, now: Instant) {
        if self.signal != Some(id) {
            return self.reject();
        }
        if control.protocol.is_some_and(|p| p != MEDIA_PROTOCOL) {
            return self.reject();
        }
        if control.protocol == Some(MEDIA_PROTOCOL) && control.epoch.is_none() {
            return self.reject();
        }
        if let Some(epoch) = &control.epoch {
            if self.epoch.as_deref() != Some(epoch.as_str()) {
                self.model.counters.stale_epoch_control += 1;
                return;
            }
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
                StartOutcome::Started | StartOutcome::AlreadyStreaming => {
                    self.start_refused = false
                }
            },
            "stop" => self.model.stop(),
            "keyframe" => {
                let reason = control.reason.as_deref().unwrap_or("unspecified");
                self.model.keyframe_request(reason, now);
            }
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

    fn on_first_frame(&mut self, id: u64, phase: &str, request_id: &str) {
        match phase {
            "request" => {
                self.model.counters.first_frame_requests += 1;
                self.first_frame = Some(FirstFrame {
                    request_id: request_id.to_owned(),
                    ready_sent: false,
                });
                let msg = if self.start_refused {
                    protocol::first_frame("failed", request_id, None, Some("codec-mismatch"))
                } else {
                    protocol::first_frame(
                        "admitted",
                        request_id,
                        Some(self.model.generation()),
                        None,
                    )
                };
                self.send(id, Outbound::Text(msg));
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
                Ok(Signal::Hello { role }) => self.on_media_hello(id, &role, now),
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

    fn on_media_hello(&mut self, id: u64, role: &str, now: Instant) {
        if role != "media-client" || self.epoch.is_none() {
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
        if let Some(signal) = self.signal.take() {
            self.close_conn(signal, "socket-drop");
        }
        if let Some(media) = self.media.take() {
            self.close_conn(media, "socket-drop");
        }
        self.first_frame = None;
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
        let ready = match &mut self.first_frame {
            Some(ff) if !ff.ready_sent => {
                ff.ready_sent = true;
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

    fn finish(mut self, now: Instant) -> Snapshot {
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
        }
    }
}

async fn run_hub(mut hub: Hub, mut events: mpsc::Receiver<Event>) -> Snapshot {
    let mut ticker = tokio::time::interval(hub.model.frame_period());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
        }
    }
    hub.finish(Instant::now())
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
                Some(Outbound::Close(reason)) => {
                    let frame = CloseFrame { code: CloseCode::Normal, reason: reason.into() };
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
    let mirror: EpochMirror = Arc::new(Mutex::new(None));
    let (events_tx, events_rx) = mpsc::channel(EVENT_DEPTH);
    let run_start = Instant::now();
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
        epochs: Vec::new(),
        conns: HashMap::new(),
        signal: None,
        media: None,
        first_frame: None,
        start_refused: false,
        draining: false,
    };
    let mut hub_task = tokio::spawn(run_hub(hub, events_rx));
    let slots = Arc::new(Semaphore::new(config.max_connections));
    let mut conns: JoinSet<()> = JoinSet::new();
    let mut next_id = 0u64;

    let snapshot = loop {
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
