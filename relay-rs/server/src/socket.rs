//! The WebSocket routes. An upgrade the core accepts gets one reader task, which feeds messages to
//! the relay, and one writer task, which drains the socket's queue. A close from the core stops
//! both: the writer sends the close frame, the reader stops reading.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bp_relay_core::limits::MEDIA_LIMIT;
use bp_relay_core::relay::{ConnId, Reject, Route, UpgradeRequest};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};

use crate::runtime::{CLOSE_TIMEOUT, Close, OsRandom, Out, SEND_TIMEOUT, Shared, Wiring, now_ms};

/// The WebSocket route for a request path, if there is one.
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

/// Accepts an upgrade the gate allowed. A refusal by the core is HTTP 404, and the socket is never
/// upgraded.
pub fn accept(state: Arc<Shared>, upgrade: WebSocketUpgrade, req: UpgradeRequest) -> Response {
    let result = state.relay().upgrade(req, now_ms());
    let id = match result {
        Ok(id) => id,
        Err(Reject) => return StatusCode::NOT_FOUND.into_response(),
    };
    let wiring = state.register(id);
    upgrade
        .max_message_size(MEDIA_LIMIT)
        .max_frame_size(MEDIA_LIMIT)
        .on_upgrade(move |socket| run_socket(state, id, socket, wiring))
}

async fn run_socket(state: Arc<Shared>, id: ConnId, socket: WebSocket, wiring: Wiring) {
    let (sink, stream) = socket.split();
    let writer = tokio::spawn(write_loop(
        sink,
        wiring.out,
        wiring.bytes,
        wiring.stop.clone(),
    ));
    read_loop(&state, id, stream, wiring.stop).await;
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

async fn send_close(sink: &mut SplitSink<WebSocket, Message>, close: Close) {
    let frame = CloseFrame {
        code: close.code,
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
    mut sink: SplitSink<WebSocket, Message>,
    mut queue: mpsc::Receiver<Out>,
    queued_bytes: Arc<AtomicUsize>,
    mut stop: watch::Receiver<Option<Close>>,
) {
    loop {
        let next = tokio::select! {
            biased;
            close = closing(&mut stop) => {
                if let Some(close) = close {
                    send_close(&mut sink, close).await;
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
                    send_close(&mut sink, close).await;
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
    mut stream: SplitStream<WebSocket>,
    mut stop: watch::Receiver<Option<Close>>,
) {
    loop {
        let next = tokio::select! {
            _ = stop.wait_for(Option::is_some) => break,
            next = stream.next() => next,
        };
        let Some(Ok(message)) = next else {
            break;
        };
        let now = now_ms();
        let actions = match message {
            Message::Text(text) => state.relay().on_text(id, text.as_str(), now, &mut OsRandom),
            Message::Binary(bytes) => state.relay().on_binary(id, &bytes, now),
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => continue,
        };
        if state.apply(Some(id), actions).await.is_err() {
            break;
        }
    }
}
