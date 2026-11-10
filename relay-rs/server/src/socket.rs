//! WebSocket transport with a small receive budget until the relay authenticates a socket.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::{StatusCode, request::Parts};
use axum::response::{IntoResponse, Response};
use bp_relay_core::relay::{ConnId, Reject, Route, UpgradeRequest};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::server::create_response,
        protocol::{CloseFrame, Role, WebSocketConfig},
    },
};

use crate::runtime::{CLOSE_TIMEOUT, Close, OsRandom, Out, SEND_TIMEOUT, Shared, Wiring, now_ms};

type Socket = WebSocketStream<FrameLimit<TokioIo<Upgraded>>>;

pub fn route_for_path(path: &str) -> Option<Route> {
    match path {
        "/host-ws" => Some(Route::HostWs),
        "/ws" => Some(Route::Ws),
        "/host-version-ws" => Some(Route::HostVersionWs),
        "/host-media-ws" => Some(Route::HostMediaWs),
        "/media-ws" => Some(Route::MediaWs),
        "/host-file-ws" => Some(Route::HostFileWs),
        "/file-ws" => Some(Route::FileWs),
        _ => None,
    }
}

/// Validate the HTTP handshake before reserving a pre-auth slot. The framing
/// guard keeps the receive limit small until the core authenticates.
pub fn accept(
    state: Arc<Shared>,
    parts: &mut Parts,
    req: UpgradeRequest,
    client: IpAddr,
) -> Response {
    accept_inner(state, parts, req, client, false)
}

pub fn accept_refused(
    state: Arc<Shared>,
    parts: &mut Parts,
    req: UpgradeRequest,
    client: IpAddr,
) -> Response {
    accept_inner(state, parts, req, client, true)
}

fn accept_inner(
    state: Arc<Shared>,
    parts: &mut Parts,
    req: UpgradeRequest,
    client: IpAddr,
    refused: bool,
) -> Response {
    let mut request = axum::http::Request::new(());
    *request.method_mut() = parts.method.clone();
    *request.version_mut() = parts.version;
    *request.headers_mut() = parts.headers.clone();
    let response = match create_response(&request) {
        Ok(response) => response,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let Some(upgrade) = parts.extensions.remove::<hyper::upgrade::OnUpgrade>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let id = match state.relay().upgrade(req, client, now_ms()) {
        Ok(id) => id,
        Err(Reject) => return StatusCode::NOT_FOUND.into_response(),
    };
    let wiring = state.register(id);
    tokio::spawn(async move {
        if let Ok(upgraded) = upgrade.await {
            let config = WebSocketConfig::default()
                .read_buffer_size(4096)
                .max_message_size(Some(bp_relay_core::limits::MEDIA_LIMIT))
                .max_frame_size(Some(bp_relay_core::limits::MEDIA_LIMIT));
            let mut socket = WebSocketStream::from_raw_socket(
                FrameLimit::new(TokioIo::new(upgraded)),
                Role::Server,
                Some(config),
            )
            .await;
            if refused {
                let frame = CloseFrame {
                    code: 1008.into(),
                    reason: bp_relay_core::relay::reasons::NOT_APPROVED.into(),
                };
                let _ =
                    tokio::time::timeout(CLOSE_TIMEOUT, socket.send(Message::Close(Some(frame))))
                        .await;
            } else {
                run_socket(state.clone(), id, socket, wiring).await;
            }
        }
        state.close_socket(id).await;
    });
    response.map(|_| axum::body::Body::empty())
}

async fn run_socket(state: Arc<Shared>, id: ConnId, socket: Socket, wiring: Wiring) {
    let read_limit = socket.get_ref().limit.clone();
    let (sink, stream) = socket.split();
    let writer = tokio::spawn(write_loop(
        sink,
        wiring.out,
        wiring.bytes,
        wiring.stop.clone(),
    ));
    read_loop(&state, id, stream, wiring.stop, read_limit).await;
    state.close_socket(id).await;
    let _ = writer.await;
}

/// Waits for a close request. `Some` carries the close frame to send; `None` means the socket was
/// dropped from the registry without a close and needs no frame.
async fn closing(stop: &mut watch::Receiver<Option<Close>>) -> Option<Close> {
    match stop.wait_for(Option::is_some).await {
        Ok(value) => (*value).clone(),
        Err(_) => None,
    }
}

/// Sends the frames still in the queue, then the close frame. The core queues a reply (hello-ok,
/// a forwarded frame) and then a close in the same action batch, but the close arrives on a
/// separate watch channel and can be seen first, so a plain close would drop that reply. The
/// drain shares CLOSE_TIMEOUT and stops at the first failed write, so a stalled peer still
/// closes promptly. Only frames that were queued before the close are sent.
async fn flush_then_close(
    sink: &mut SplitSink<Socket, Message>,
    queue: &mut mpsc::Receiver<Out>,
    queued_bytes: &AtomicUsize,
    close: Close,
) {
    let deadline = tokio::time::Instant::now() + CLOSE_TIMEOUT;
    while let Ok(out) = queue.try_recv() {
        let len = out.len();
        let sent = tokio::time::timeout_at(deadline, sink.send(into_message(out))).await;
        queued_bytes.fetch_sub(len, Ordering::SeqCst);
        if !matches!(sent, Ok(Ok(()))) {
            break;
        }
    }
    send_close(sink, close).await;
}

async fn send_close(sink: &mut SplitSink<Socket, Message>, close: Close) {
    let frame = CloseFrame {
        code: close.code.into(),
        reason: close.reason.into(),
    };
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, sink.send(Message::Close(Some(frame)))).await;
}

fn into_message(out: Out) -> Message {
    match out {
        Out::Text(text) => Message::Text(text.into()),
        Out::Binary(bytes) => Message::Binary(bytes.into()),
    }
}

async fn write_loop(
    mut sink: SplitSink<Socket, Message>,
    mut queue: mpsc::Receiver<Out>,
    queued_bytes: Arc<AtomicUsize>,
    mut stop: watch::Receiver<Option<Close>>,
) {
    loop {
        let next = tokio::select! {
            biased;
            close = closing(&mut stop) => {
                if let Some(close) = close {
                    flush_then_close(&mut sink, &mut queue, &queued_bytes, close).await;
                }
                return;
            }
            next = queue.recv() => next,
        };
        let Some(out) = next else {
            break;
        };
        let len = out.len();
        let message = into_message(out);
        let sent = tokio::select! {
            biased;
            close = closing(&mut stop) => {
                queued_bytes.fetch_sub(len, Ordering::SeqCst);
                if let Some(close) = close {
                    flush_then_close(&mut sink, &mut queue, &queued_bytes, close).await;
                }
                return;
            }
            sent = tokio::time::timeout(SEND_TIMEOUT, sink.send(message)) => sent,
        };
        queued_bytes.fetch_sub(len, Ordering::SeqCst);
        if !matches!(sent, Ok(Ok(()))) {
            break;
        }
    }
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, sink.close()).await;
}

async fn read_loop(
    state: &Shared,
    id: ConnId,
    mut stream: SplitStream<Socket>,
    mut stop: watch::Receiver<Option<Close>>,
    read_limit: Arc<AtomicUsize>,
) {
    loop {
        let Some(limit) = state.relay().read_limit(id) else {
            break;
        };
        read_limit.store(limit, Ordering::SeqCst);
        let next = tokio::select! {
            _ = stop.wait_for(Option::is_some) => break,
            next = stream.next() => next,
        };
        let Some(Ok(message)) = next else {
            break;
        };
        let now = now_ms();
        let actions = match message {
            Message::Text(text) => state.relay().on_text_with_gate(
                id,
                text.as_str(),
                now,
                &mut OsRandom,
                &mut state.gate(),
            ),
            Message::Binary(bytes) => state.relay().on_binary(id, &bytes, now),
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
        };
        if state.apply(Some(id), actions).await.is_err() {
            break;
        }
    }
}

/// Inspect only the at-most-14-byte wire header before allowing Tungstenite to read
/// a frame payload. Never read ahead across frame boundaries: the hello can change
/// the budget before the next frame. Fragmented messages share one byte budget.
struct FrameLimit<S> {
    inner: S,
    limit: Arc<AtomicUsize>,
    header: [u8; 14],
    have: usize,
    need: usize,
    emitted: usize,
    ready: bool,
    remaining: u64,
    message_bytes: u64,
}

impl<S> FrameLimit<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            limit: Arc::new(AtomicUsize::new(bp_relay_core::limits::PREAUTH_READ_LIMIT)),
            header: [0; 14],
            have: 0,
            need: 2,
            emitted: 0,
            ready: false,
            remaining: 0,
            message_bytes: 0,
        }
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for FrameLimit<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        out: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::{
            pin::Pin,
            task::{Poll, ready},
        };
        use tokio::io::ReadBuf;
        let this = self.get_mut();
        if out.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if !this.ready {
                while this.have < this.need {
                    let mut buffer = ReadBuf::new(&mut this.header[this.have..this.need]);
                    ready!(Pin::new(&mut this.inner).poll_read(cx, &mut buffer))?;
                    let count = buffer.filled().len();
                    if count == 0 {
                        return Poll::Ready(Ok(()));
                    }
                    this.have += count;
                    if this.have == 2 {
                        this.need =
                            2 + match this.header[1] & 127 {
                                126 => 2,
                                127 => 8,
                                _ => 0,
                            } + if this.header[1] & 128 != 0 { 4 } else { 0 };
                    }
                }
                let length = match this.header[1] & 127 {
                    126 => u16::from_be_bytes(this.header[2..4].try_into().unwrap()) as u64,
                    127 => u64::from_be_bytes(this.header[2..10].try_into().unwrap()),
                    length => u64::from(length),
                };
                let opcode = this.header[0] & 15;
                if opcode < 8 {
                    if opcode != 0 {
                        this.message_bytes = 0;
                    }
                    this.message_bytes = this.message_bytes.saturating_add(length);
                }
                if length > this.limit.load(Ordering::SeqCst) as u64
                    || (opcode < 8 && this.message_bytes > this.limit.load(Ordering::SeqCst) as u64)
                {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "WebSocket receive limit exceeded",
                    )));
                }
                if opcode < 8 && this.header[0] & 128 != 0 {
                    this.message_bytes = 0;
                }
                this.remaining = length;
                this.ready = true;
            }
            if this.emitted < this.have {
                let count = out.remaining().min(this.have - this.emitted);
                out.put_slice(&this.header[this.emitted..this.emitted + count]);
                this.emitted += count;
                return Poll::Ready(Ok(()));
            }
            if this.remaining > 0 {
                let mut bytes = [0u8; 4096];
                let count = out
                    .remaining()
                    .min(this.remaining as usize)
                    .min(bytes.len());
                let mut buffer = ReadBuf::new(&mut bytes[..count]);
                ready!(Pin::new(&mut this.inner).poll_read(cx, &mut buffer))?;
                let count = buffer.filled().len();
                out.put_slice(buffer.filled());
                this.remaining -= count as u64;
                return Poll::Ready(Ok(()));
            }
            this.have = 0;
            this.need = 2;
            this.emitted = 0;
            this.ready = false;
        }
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for FrameLimit<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, bytes)
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
