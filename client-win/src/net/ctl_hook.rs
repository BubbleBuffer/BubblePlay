//! A generic hook for log and feedback upload over the authenticated `/ws` ctl channel
//! (wire protocol: docs/architecture/client-log-upload.md).
//!
//! The session owns the sockets and the sealing; the hook is a small mailbox between it
//! and one consumer (the Flutter app's upload queue, through the frb layer). The consumer:
//!
//! - calls [`CtlLogHook::send`] with a JSON message (`logs-batch`, `feedback-begin` or
//!   `feedback-chunk`); the session seals it as a ctl kind and writes it with the lowest
//!   priority (see `Session::drain_logs`);
//! - receives [`CtlLogEvent`]s: `Opened` after each authenticated connection (a reconnect
//!   gives a new `Opened`), `Message` for the host's `logs-state`, `logs-ack` and
//!   `feedback-ack`, and `Closed` when that connection ends;
//! - reads [`CtlLogHook::buffered_bytes`] and [`CtlLogHook::since_last_input`] to apply the
//!   wire doc's priority rules (send buffer at most 16 KiB, no input for 250 ms).
//!
//! A hook configured in `NetConfig` makes the session advertise `client-logs-v1`. The
//! hook accepts and delivers messages only while connected to a PC that advertised the
//! capability too; otherwise nothing is sent and nothing is routed.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// The capability string (both sides advertise it in their ctl hello).
pub const CAP_CLIENT_LOGS: &str = "client-logs-v1";

/// Most messages waiting in the outbound queue.
pub const MAX_QUEUE_MESSAGES: usize = 64;
/// Most bytes waiting in the outbound queue.
pub const MAX_QUEUE_BYTES: usize = 1 << 20;
/// Largest outbound message: the wire doc keeps `feedback-begin` under 128 KiB.
pub const MAX_MESSAGE_BYTES: usize = 128 * 1024;
/// Largest inbound message the session routes to the hook.
pub const MAX_INBOUND_BYTES: usize = 64 * 1024;
/// Events a [`CtlLogHook::channel`] receiver may fall behind before events are dropped.
pub const CHANNEL_EVENTS: usize = 1024;

/// The client-to-host kinds the hook may send. Anything else is refused, so the hook
/// cannot be used to put other ctl kinds (`input`, `media-control`, ...) on the wire.
const OUTBOUND_KINDS: [&str; 3] = ["logs-batch", "feedback-begin", "feedback-chunk"];
/// The host-to-client kinds the session routes to the hook.
const INBOUND_KINDS: [&str; 3] = ["logs-state", "logs-ack", "feedback-ack"];

/// What the hook tells its consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlLogEvent {
    /// An authenticated connection to `pc_id` is up. `peer_caps` is the PC's cap list; the
    /// hook sends and delivers only when it contains [`CAP_CLIENT_LOGS`].
    Opened {
        pc_id: String,
        peer_caps: Vec<String>,
    },
    /// A routed host message (JSON text of `logs-state`, `logs-ack` or `feedback-ack`).
    Message(String),
    /// The connection that last produced `Opened` ended. Queued messages were dropped.
    Closed,
}

type Sink = Arc<dyn Fn(CtlLogEvent) + Send + Sync>;

#[derive(Default)]
struct State {
    /// `Opened` was emitted and `Closed` has not been yet.
    open: bool,
    /// Open to a PC that has the capability: the queue accepts and the session drains.
    connected: bool,
    queue: VecDeque<String>,
}

struct Shared {
    sink: Sink,
    state: Mutex<State>,
    queued_bytes: AtomicUsize,
    /// Milliseconds since `origin` of the last input send, `u64::MAX` before the first.
    last_input_ms: AtomicU64,
    origin: Instant,
}

/// The cloneable handle: the consumer keeps one, `NetConfig` holds another.
#[derive(Clone)]
pub struct CtlLogHook {
    shared: Arc<Shared>,
}

impl fmt::Debug for CtlLogHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CtlLogHook")
            .field("connected", &self.is_connected())
            .field("buffered_bytes", &self.buffered_bytes())
            .finish()
    }
}

#[derive(Deserialize)]
struct Head<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
}

fn kind_of(json: &str) -> Option<&str> {
    serde_json::from_str::<Head>(json)
        .ok()
        .map(|head| head.kind)
}

impl CtlLogHook {
    /// A hook that reports events to `sink`. The sink runs on the net thread (or the
    /// caller's thread for `send`-side effects: none today) and must not block.
    pub fn new(sink: impl Fn(CtlLogEvent) + Send + Sync + 'static) -> Self {
        Self {
            shared: Arc::new(Shared {
                sink: Arc::new(sink),
                state: Mutex::new(State::default()),
                queued_bytes: AtomicUsize::new(0),
                last_input_ms: AtomicU64::new(u64::MAX),
                origin: Instant::now(),
            }),
        }
    }

    /// A hook plus a bounded receiver of its events. If the receiver falls
    /// [`CHANNEL_EVENTS`] behind, further events are dropped.
    pub fn channel() -> (Self, Receiver<CtlLogEvent>) {
        let (tx, rx): (SyncSender<CtlLogEvent>, _) = mpsc::sync_channel(CHANNEL_EVENTS);
        let hook = Self::new(move |event| {
            let _ = tx.try_send(event);
        });
        (hook, rx)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit(&self, event: CtlLogEvent) {
        (self.shared.sink)(event);
    }

    /// Queue `json` (a `logs-batch`, `feedback-begin` or `feedback-chunk` message) for the
    /// session. Returns false, and queues nothing, when not connected to a PC with the
    /// capability, when the queue is full, or when the message is too large, is not JSON
    /// or has another `type`.
    pub fn send(&self, json: String) -> bool {
        if json.len() > MAX_MESSAGE_BYTES {
            return false;
        }
        if !kind_of(&json).is_some_and(|kind| OUTBOUND_KINDS.contains(&kind)) {
            return false;
        }
        let mut state = self.state();
        if !state.connected
            || state.queue.len() >= MAX_QUEUE_MESSAGES
            || self.shared.queued_bytes.load(Ordering::Relaxed) + json.len() > MAX_QUEUE_BYTES
        {
            return false;
        }
        self.shared
            .queued_bytes
            .fetch_add(json.len(), Ordering::Relaxed);
        state.queue.push_back(json);
        true
    }

    /// Bytes waiting in the hook's outbound queue. The socket's own send queue is not
    /// visible through the WebSocket layer, so it is not included; the session writes at
    /// most one hook message per loop turn, so at most one message is in the socket queue.
    pub fn buffered_bytes(&self) -> usize {
        self.shared.queued_bytes.load(Ordering::Relaxed)
    }

    /// Time since the session last sent an input packet (`Duration::MAX` if it never has).
    pub fn since_last_input(&self) -> Duration {
        match self.shared.last_input_ms.load(Ordering::Relaxed) {
            u64::MAX => Duration::MAX,
            at => self
                .shared
                .origin
                .elapsed()
                .saturating_sub(Duration::from_millis(at)),
        }
    }

    /// True between `Opened` and `Closed` with a PC that has the capability.
    pub fn is_connected(&self) -> bool {
        self.state().connected
    }

    /// The capability list the session advertises with this hook configured.
    pub(crate) fn advertise(base: &[&'static str]) -> Vec<&'static str> {
        let mut caps = base.to_vec();
        caps.push(CAP_CLIENT_LOGS);
        caps
    }

    /// The session: an authenticated connection to `pc_id` is up, `peer_caps` checked.
    pub(crate) fn opened(&self, pc_id: String, peer_caps: Vec<String>) {
        let enabled = peer_caps.iter().any(|cap| cap == CAP_CLIENT_LOGS);
        let stale = {
            let mut state = self.state();
            let was_open = std::mem::replace(&mut state.open, true);
            state.connected = enabled;
            state.queue.clear();
            self.shared.queued_bytes.store(0, Ordering::Relaxed);
            was_open
        };
        if stale {
            self.emit(CtlLogEvent::Closed);
        }
        self.emit(CtlLogEvent::Opened { pc_id, peer_caps });
    }

    /// The session: the connection ended. Does nothing unless `Opened` came before.
    pub(crate) fn closed(&self) {
        let was_open = {
            let mut state = self.state();
            state.connected = false;
            state.queue.clear();
            self.shared.queued_bytes.store(0, Ordering::Relaxed);
            std::mem::replace(&mut state.open, false)
        };
        if was_open {
            self.emit(CtlLogEvent::Closed);
        }
    }

    /// The session: route a host message to the consumer. True when `text` was one of the
    /// routed kinds (and consumed); false leaves it to the normal handling.
    pub(crate) fn deliver(&self, text: &str) -> bool {
        if !kind_of(text).is_some_and(|kind| INBOUND_KINDS.contains(&kind)) {
            return false;
        }
        // A routed kind is consumed even when it is dropped here: with the capability
        // off, or oversized, nothing else in the session would know it either.
        if text.len() <= MAX_INBOUND_BYTES && self.state().connected {
            self.emit(CtlLogEvent::Message(text.to_owned()));
        }
        true
    }

    /// The session: the next message to write, if any.
    pub(crate) fn take_outbound(&self) -> Option<String> {
        let mut state = self.state();
        if !state.connected {
            return None;
        }
        let json = state.queue.pop_front()?;
        self.shared
            .queued_bytes
            .fetch_sub(json.len(), Ordering::Relaxed);
        Some(json)
    }

    /// The session: an input packet went out now.
    pub(crate) fn stamp_input(&self) {
        let ms = self
            .shared
            .origin
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX - 1)) as u64;
        self.shared.last_input_ms.store(ms, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(id: u64) -> String {
        format!(
            r#"{{"type":"logs-batch","v":1,"stream":"app-win","id":{id},"from":"","to":"c","live":false,"lines":[]}}"#
        )
    }

    fn open(hook: &CtlLogHook) {
        hook.opened(
            "PC".into(),
            vec!["control-aead-v1".into(), CAP_CLIENT_LOGS.into()],
        );
    }

    #[test]
    fn refuses_until_opened_and_after_closed() {
        let (hook, events) = CtlLogHook::channel();
        assert!(!hook.send(batch(1)), "not connected");
        assert_eq!(hook.buffered_bytes(), 0);
        open(&hook);
        assert!(hook.send(batch(1)));
        assert_eq!(hook.buffered_bytes(), batch(1).len());
        hook.closed();
        assert_eq!(hook.buffered_bytes(), 0, "closing drops the queue");
        assert!(!hook.send(batch(2)));
        assert_eq!(hook.take_outbound(), None);
        assert!(matches!(events.try_recv(), Ok(CtlLogEvent::Opened { .. })));
        assert_eq!(events.try_recv(), Ok(CtlLogEvent::Closed));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn a_peer_without_the_capability_is_never_connected() {
        let (hook, events) = CtlLogHook::channel();
        hook.opened("PC".into(), vec!["control-aead-v1".into()]);
        assert!(!hook.is_connected());
        assert!(!hook.send(batch(1)));
        assert!(
            hook.deliver(r#"{"type":"logs-ack","v":1}"#),
            "consumed, not delivered"
        );
        let Ok(CtlLogEvent::Opened { peer_caps, .. }) = events.try_recv() else {
            panic!("Opened is still reported, so the consumer can see the missing cap");
        };
        assert_eq!(peer_caps, ["control-aead-v1"]);
        assert!(events.try_recv().is_err(), "no Message without the cap");
    }

    #[test]
    fn closed_without_opened_is_silent_and_reopen_closes_the_old_epoch_first() {
        let (hook, events) = CtlLogHook::channel();
        hook.closed();
        assert!(events.try_recv().is_err());
        open(&hook);
        assert!(hook.send(batch(1)));
        open(&hook);
        assert_eq!(hook.buffered_bytes(), 0, "a new epoch starts empty");
        let kinds: Vec<_> = events.try_iter().collect();
        assert!(matches!(kinds[0], CtlLogEvent::Opened { .. }));
        assert_eq!(kinds[1], CtlLogEvent::Closed);
        assert!(matches!(kinds[2], CtlLogEvent::Opened { .. }));
    }

    #[test]
    fn the_queue_is_bounded_in_messages_and_bytes() {
        let (hook, _events) = CtlLogHook::channel();
        open(&hook);
        for id in 0..MAX_QUEUE_MESSAGES as u64 {
            assert!(hook.send(batch(id)), "message {id}");
        }
        assert!(!hook.send(batch(999)), "queue full by count");
        assert_eq!(hook.take_outbound().as_deref(), Some(batch(0).as_str()));
        assert!(hook.send(batch(999)), "room again after a take");

        let (hook, _events) = CtlLogHook::channel();
        open(&hook);
        let pad = "x".repeat(MAX_MESSAGE_BYTES - 200);
        let big = format!(r#"{{"type":"feedback-chunk","v":1,"id":"a","i":0,"data":"{pad}"}}"#);
        assert!(big.len() <= MAX_MESSAGE_BYTES);
        let fits = MAX_QUEUE_BYTES / big.len();
        for _ in 0..fits {
            assert!(hook.send(big.clone()));
        }
        assert!(!hook.send(big.clone()), "queue full by bytes");
        assert_eq!(hook.buffered_bytes(), fits * big.len());
        let too_big = format!(
            r#"{{"type":"feedback-chunk","data":"{}"}}"#,
            "x".repeat(MAX_MESSAGE_BYTES)
        );
        assert!(!hook.send(too_big), "a single message over the cap");
    }

    #[test]
    fn only_the_upload_kinds_may_be_sent() {
        let (hook, _events) = CtlLogHook::channel();
        open(&hook);
        for bad in [
            r#"{"type":"media-control","action":"start"}"#,
            r#"{"type":"input"}"#,
            r#"{"type":"caps","caps":[]}"#,
            r#"{"type":"logs-ack","v":1}"#,
            r#"{"v":1}"#,
            "not json",
            "",
        ] {
            assert!(!hook.send(bad.to_owned()), "{bad}");
        }
        assert!(hook.send(r#"{"type":"feedback-begin","v":1}"#.into()));
        assert!(hook.send(r#"{"type":"feedback-chunk","v":1}"#.into()));
        assert!(hook.send(batch(1)));
        // FIFO.
        assert!(hook.take_outbound().unwrap().contains("feedback-begin"));
        assert!(hook.take_outbound().unwrap().contains("feedback-chunk"));
        assert!(hook.take_outbound().unwrap().contains("logs-batch"));
        assert_eq!(hook.take_outbound(), None);
        assert_eq!(hook.buffered_bytes(), 0);
    }

    #[test]
    fn inbound_routing_is_limited_to_the_three_kinds() {
        let (hook, events) = CtlLogHook::channel();
        open(&hook);
        let _ = events.try_recv();
        for kind in ["logs-state", "logs-ack", "feedback-ack"] {
            let text = format!(r#"{{"type":"{kind}","v":1}}"#);
            assert!(hook.deliver(&text), "{kind}");
            assert_eq!(events.try_recv(), Ok(CtlLogEvent::Message(text)));
        }
        for other in [
            r#"{"type":"monitor-list"}"#,
            r#"{"type":"first-frame"}"#,
            r#"{"type":"logs-batch","v":1}"#,
            "not json",
        ] {
            assert!(!hook.deliver(other), "{other}");
        }
        assert!(events.try_recv().is_err());
        // Oversized routed messages are consumed but not delivered.
        let big = format!(
            r#"{{"type":"logs-ack","pad":"{}"}}"#,
            "x".repeat(MAX_INBOUND_BYTES)
        );
        assert!(hook.deliver(&big));
        assert!(events.try_recv().is_err());
        hook.closed();
        let _ = events.try_recv();
        assert!(hook.deliver(r#"{"type":"logs-ack","v":1}"#), "consumed");
        assert!(events.try_recv().is_err(), "but not delivered when closed");
    }

    #[test]
    fn since_last_input_is_max_until_stamped_then_counts_up() {
        let (hook, _events) = CtlLogHook::channel();
        assert_eq!(hook.since_last_input(), Duration::MAX);
        hook.stamp_input();
        let first = hook.since_last_input();
        assert!(first < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(30));
        assert!(hook.since_last_input() >= Duration::from_millis(25));
        hook.stamp_input();
        assert!(hook.since_last_input() < Duration::from_millis(25));
    }

    #[test]
    fn clones_share_one_queue() {
        let (hook, _events) = CtlLogHook::channel();
        let other = hook.clone();
        open(&hook);
        assert!(other.send(batch(1)));
        assert_eq!(hook.buffered_bytes(), other.buffered_bytes());
        assert!(hook.take_outbound().is_some());
        assert_eq!(other.buffered_bytes(), 0);
    }

    #[test]
    fn advertise_appends_the_capability() {
        assert_eq!(
            CtlLogHook::advertise(&["a", "b"]),
            ["a", "b", "client-logs-v1"]
        );
    }
}
