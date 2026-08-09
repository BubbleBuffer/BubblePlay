//! Signaling client for the BubblePlay Cloudflare Worker relay.
//!
//! Protocol (JSON text frames over one WebSocket):
//! - host -> worker: {type:"hello", token, role:"host"}
//! - worker -> host: {type:"hello-ok"} | {type:"error", message}
//! - relayed: {type:"offer", sdp} (client -> host via worker),
//!            {type:"answer", sdp} (host -> client),
//!            {type:"ice", candidate} (both directions),
//!            {type:"peer-left"},
//!            {type:"ping"} (keepalive — consumed, never relayed)
//!
//! The socket runs non-blocking; call `poll()` once per loop iteration.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::net::TcpStream;
use std::time::Duration;
use tracing::{debug, info, warn};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Error as WsError, Message, WebSocket};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SignalMsg {
    Hello {
        token: String,
        role: String,
    },
    #[serde(rename = "hello-ok")]
    HelloOk,
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    Ice {
        candidate: String,
    },
    Diag {
        seq: u64,
        #[serde(rename = "atMs")]
        at_ms: u64,
        profile: serde_json::Value,
    },
    Qos {
        #[serde(rename = "sessionId")]
        session_id: String,
        seq: u64,
        #[serde(rename = "atMs")]
        at_ms: u64,
        #[serde(default)]
        rx: QosRx,
        #[serde(default)]
        presentation: QosPresentation,
    },
    Quality {
        profile: String,
        width: u32,
        height: u32,
        fps: u32,
        #[serde(rename = "targetBitrateBps")]
        target_bitrate_bps: u32,
        #[serde(rename = "bweBps")]
        bwe_bps: Option<u32>,
        reason: String,
    },
    #[serde(rename = "peer-left")]
    PeerLeft,
    Ping,
    Error {
        message: String,
    },
    Bye,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QosRx {
    pub mbps: Option<f64>,
    #[serde(rename = "lossPct")]
    pub loss_pct: Option<f64>,
    #[serde(rename = "rttMs")]
    pub rtt_ms: Option<f64>,
    #[serde(rename = "jitterMs")]
    pub jitter_ms: Option<f64>,
    #[serde(rename = "jitterBufferMs")]
    pub jitter_buffer_ms: Option<f64>,
    #[serde(rename = "nackDelta")]
    pub nack_delta: Option<u64>,
    #[serde(rename = "pliDelta")]
    pub pli_delta: Option<u64>,
    #[serde(rename = "packetsReceivedDelta")]
    pub packets_received_delta: Option<u64>,
    #[serde(rename = "framesReceivedDelta")]
    pub frames_received_delta: Option<u64>,
    #[serde(rename = "framesDecodedDelta")]
    pub frames_decoded_delta: Option<u64>,
    #[serde(rename = "framesDroppedDelta")]
    pub frames_dropped_delta: Option<u64>,
    #[serde(rename = "freezeDelta")]
    pub freeze_delta: Option<u64>,
    #[serde(rename = "qpPerFrame")]
    pub qp_per_frame: Option<f64>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QosPresentation {
    pub fps: Option<f64>,
    #[serde(rename = "recentGapMs")]
    pub recent_gap_ms: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

pub enum SignalEvent {
    /// Worker accepted our hello.
    Registered,
    /// Browser sent an SDP offer.
    Offer(String),
    /// Browser trickled an ICE candidate (SDP string form).
    RemoteCandidate(String),
    /// Authenticated browser profiling snapshot (enabled with ?profile=1).
    ClientDiagnostics {
        seq: u64,
        at_ms: u64,
        profile: serde_json::Value,
    },
    Qos {
        seq: u64,
        at_ms: u64,
        rx: QosRx,
        presentation: QosPresentation,
    },
    /// The client left (or the relay told us so).
    PeerLeft,
}

pub struct Signaling {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    registered: bool,
    peer_seen: bool,
}

impl Signaling {
    /// Connect to the worker (wss://...), upgrade to WS, register as host.
    pub fn connect(url: &str, token: &str) -> Result<Self> {
        info!(url, "connecting to signaling worker");
        let (mut ws, response) = connect(url).context("websocket connect")?;
        debug!(status = %response.status(), "ws handshake done");

        // Send hello, then wait (blocking, one-time) for hello-ok.
        let hello = SignalMsg::Hello {
            token: token.to_string(),
            role: "host".to_string(),
        };
        ws.send(Message::text(serde_json::to_string(&hello)?))
            .context("send hello")?;

        loop {
            let msg = ws.read().context("read hello-ack")?;
            let Message::Text(text) = msg else { continue };
            let parsed: SignalMsg =
                serde_json::from_str(&text).with_context(|| format!("bad hello ack: {text}"))?;
            match parsed {
                SignalMsg::HelloOk => break,
                SignalMsg::Error { message } => bail!("worker rejected hello: {message}"),
                other => bail!("unexpected message while registering: {other:?}"),
            }
        }

        // Flip the underlying TCP stream to non-blocking for the run loop.
        set_nonblocking(&mut ws)?;
        info!("signaling registered as host");
        Ok(Self {
            ws,
            registered: true,
            peer_seen: false,
        })
    }

    pub fn registered(&self) -> bool {
        self.registered
    }

    /// Drain pending messages without blocking. Call once per main-loop tick.
    pub fn poll(&mut self) -> Result<Vec<SignalEvent>> {
        let mut events = Vec::new();
        loop {
            let msg = match self.ws.read() {
                Ok(m) => m,
                Err(WsError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // TLS may surface "want read" as WouldBlock via native-tls too.
                Err(e) => return Err(e).context("ws read"),
            };
            match msg {
                Message::Text(text) => {
                    let parsed: SignalMsg = match serde_json::from_str(&text) {
                        Ok(m) => m,
                        Err(e) => {
                            warn!(?e, %text, "ignoring unparsable signal message");
                            continue;
                        }
                    };
                    match parsed {
                        SignalMsg::Offer { sdp } => {
                            self.peer_seen = true;
                            events.push(SignalEvent::Offer(sdp));
                        }
                        SignalMsg::Ice { candidate } => {
                            self.peer_seen = true;
                            events.push(SignalEvent::RemoteCandidate(candidate));
                        }
                        SignalMsg::Diag {
                            seq,
                            at_ms,
                            profile,
                        } => events.push(SignalEvent::ClientDiagnostics {
                            seq,
                            at_ms,
                            profile,
                        }),
                        SignalMsg::Qos {
                            seq,
                            at_ms,
                            rx,
                            presentation,
                            ..
                        } => events.push(SignalEvent::Qos {
                            seq,
                            at_ms,
                            rx,
                            presentation,
                        }),
                        SignalMsg::PeerLeft => events.push(SignalEvent::PeerLeft),
                        SignalMsg::Ping => {}    // keepalive, ignore
                        SignalMsg::HelloOk => {} // duplicate ack, ignore
                        SignalMsg::Error { message } => {
                            warn!(message, "worker error message");
                        }
                        other => debug!(?other, "ignoring signal message"),
                    }
                }
                Message::Ping(_) | Message::Pong(_) => {} // tungstenite auto-pongs
                Message::Close(_) => {
                    self.registered = false;
                    events.push(SignalEvent::PeerLeft);
                    break;
                }
                _ => {}
            }
        }
        Ok(events)
    }

    pub fn send_answer(&mut self, sdp: &str) -> Result<()> {
        let msg = SignalMsg::Answer {
            sdp: sdp.to_string(),
        };
        self.ws
            .send(Message::text(serde_json::to_string(&msg)?))
            .context("send answer")?;
        self.ws.flush().ok();
        Ok(())
    }

    /// Trickle a local candidate to the browser (SDP string form).
    pub fn send_candidate(&mut self, candidate: &str) -> Result<()> {
        let msg = SignalMsg::Ice {
            candidate: candidate.to_string(),
        };
        self.ws
            .send(Message::text(serde_json::to_string(&msg)?))
            .context("send ice")?;
        self.ws.flush().ok();
        Ok(())
    }

    pub fn send_bye(&mut self) {
        let _ = self.ws.send(Message::text(
            serde_json::to_string(&SignalMsg::Bye).unwrap_or_default(),
        ));
        let _ = self.ws.flush();
    }

    /// Send an application-level keepalive so the relay's WebSocket stays
    /// warm on both sides (the worker consumes pings, never relays them).
    pub fn send_ping(&mut self) {
        let msg = SignalMsg::Ping;
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            self.ws.flush().ok();
        }
    }

    pub fn send_quality(
        &mut self,
        profile: &str,
        width: u32,
        height: u32,
        fps: u32,
        target_bitrate_bps: u32,
        bwe_bps: Option<u32>,
        reason: &str,
    ) {
        let msg = SignalMsg::Quality {
            profile: profile.into(),
            width,
            height,
            fps,
            target_bitrate_bps,
            bwe_bps,
            reason: reason.into(),
        };
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            self.ws.flush().ok();
        }
    }
}

fn set_nonblocking(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> Result<()> {
    let stream = ws.get_mut();
    let tcp: &TcpStream = match stream {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::NativeTls(t) => t.get_ref(),
        other => bail!("unsupported TLS stream variant: {other:?}"),
    };
    tcp.set_nonblocking(true).context("set_nonblocking")?;
    // Give the socket a moment to settle (TLS session state already established
    // during the blocking handshake; reads are now poll-driven).
    std::thread::sleep(Duration::from_millis(0));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SignalMsg;

    #[test]
    fn parses_client_diagnostics_message() {
        let message: SignalMsg = serde_json::from_str(
            r#"{"type":"diag","seq":12,"atMs":1780000000000,"profile":{"rtc":{"inbound":{"packetsLost":3}}}}"#,
        )
        .expect("diagnostics message should deserialize");
        match message {
            SignalMsg::Diag {
                seq,
                at_ms,
                profile,
            } => {
                assert_eq!(seq, 12);
                assert_eq!(at_ms, 1_780_000_000_000);
                assert_eq!(profile["rtc"]["inbound"]["packetsLost"], 3);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
    #[test]
    fn parses_sparse_qos_without_rejecting_missing_numbers() {
        let message: SignalMsg = serde_json::from_str(r#"{"type":"qos","sessionId":"opaque","seq":1,"atMs":2,"rx":{"lossPct":5},"presentation":{"fps":30}}"#).unwrap();
        assert!(matches!(message, SignalMsg::Qos { .. }));
    }
}
