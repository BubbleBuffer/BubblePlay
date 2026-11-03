//! The glue between the client core's [`CtlLogHook`] and the Dart upload queue.
//!
//! One hook lives for the whole process; the streaming session gets a clone in its
//! `NetConfig` (see `stream_host`), so the session advertises `client-logs-v1`. Hook events
//! are encoded as one JSON string each and pushed to the single Dart subscriber:
//!
//! - `{"kind":"opened","pcId":"..","peerCaps":[".."]}`
//! - `{"kind":"message","json":{..}}` (a `logs-state`, `logs-ack` or `feedback-ack`)
//! - `{"kind":"closed"}`
//!
//! Only one subscription exists at a time: subscribing again replaces the previous one (its
//! stream simply stops receiving). If a connection is open at that moment, the new
//! subscriber first gets a replayed `opened`, so it never misses the epoch it joins.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use bubbleplay_client::net::{CtlLogEvent, CtlLogHook};
use serde_json::{json, Value};

/// What `ctl_log_state` reports (the Dart bridge polls it while a connection is open).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtlLogSnapshot {
    /// Connected to a PC that advertised `client-logs-v1`: sends are accepted.
    pub open: bool,
    /// Bytes waiting in the hook's outbound queue.
    pub buffered_bytes: i64,
    /// Milliseconds since the last input packet; `i64::MAX` if there was none yet.
    pub since_last_input_ms: i64,
}

type Subscriber = Box<dyn Fn(String) -> bool + Send>;

#[derive(Default)]
struct Link {
    subscriber: Option<Subscriber>,
    /// `(pc_id, peer_caps)` of the open connection, for the replay on subscribe.
    open: Option<(String, Vec<String>)>,
}

/// A hook and its single subscriber.
pub struct CtlLog {
    hook: CtlLogHook,
    link: Arc<Mutex<Link>>,
}

fn lock(link: &Mutex<Link>) -> MutexGuard<'_, Link> {
    link.lock().unwrap_or_else(|p| p.into_inner())
}

/// One hook event as the JSON string Dart decodes; `None` for a message that is not JSON.
pub fn encode_event(event: &CtlLogEvent) -> Option<String> {
    let value = match event {
        CtlLogEvent::Opened { pc_id, peer_caps } => {
            json!({"kind": "opened", "pcId": pc_id, "peerCaps": peer_caps})
        }
        CtlLogEvent::Message(text) => {
            let parsed: Value = serde_json::from_str(text).ok()?;
            json!({"kind": "message", "json": parsed})
        }
        CtlLogEvent::Closed => json!({"kind": "closed"}),
    };
    Some(value.to_string())
}

fn deliver(link: &Mutex<Link>, event: &CtlLogEvent) {
    let mut link = lock(link);
    match event {
        CtlLogEvent::Opened { pc_id, peer_caps } => {
            link.open = Some((pc_id.clone(), peer_caps.clone()));
        }
        CtlLogEvent::Closed => link.open = None,
        CtlLogEvent::Message(_) => {}
    }
    let Some(line) = encode_event(event) else {
        return;
    };
    if let Some(subscriber) = &link.subscriber {
        if !subscriber(line) {
            // The Dart stream is gone; stop encoding for it.
            link.subscriber = None;
        }
    }
}

impl CtlLog {
    pub fn new() -> Self {
        let link = Arc::new(Mutex::new(Link::default()));
        let sink_link = Arc::clone(&link);
        let hook = CtlLogHook::new(move |event| deliver(&sink_link, &event));
        Self { hook, link }
    }

    /// A clone of the hook for a session's `NetConfig`.
    pub fn hook(&self) -> CtlLogHook {
        self.hook.clone()
    }

    /// Queues one upload message. False when there is no connection, the queue refused it,
    /// or it is not an upload kind.
    pub fn send(&self, json: String) -> bool {
        self.hook.send(json)
    }

    pub fn state(&self) -> CtlLogSnapshot {
        let since = self.hook.since_last_input();
        CtlLogSnapshot {
            open: self.hook.is_connected(),
            buffered_bytes: i64::try_from(self.hook.buffered_bytes()).unwrap_or(i64::MAX),
            since_last_input_ms: if since == Duration::MAX {
                i64::MAX
            } else {
                i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
            },
        }
    }

    /// Makes `subscriber` the one receiver of events, replacing any earlier one. It returns
    /// false once its stream is closed. A currently open connection is replayed as `opened`.
    pub fn subscribe(&self, subscriber: impl Fn(String) -> bool + Send + 'static) {
        let mut link = lock(&self.link);
        if let Some((pc_id, peer_caps)) = link.open.clone() {
            let opened = CtlLogEvent::Opened { pc_id, peer_caps };
            if let Some(line) = encode_event(&opened) {
                if !subscriber(line) {
                    link.subscriber = None;
                    return;
                }
            }
        }
        link.subscriber = Some(Box::new(subscriber));
    }

    #[cfg(test)]
    fn emit(&self, event: CtlLogEvent) {
        deliver(&self.link, &event);
    }
}

impl Default for CtlLog {
    fn default() -> Self {
        Self::new()
    }
}

static GLOBAL: OnceLock<CtlLog> = OnceLock::new();

/// The process-wide instance.
pub fn global() -> &'static CtlLog {
    GLOBAL.get_or_init(CtlLog::new)
}

/// The hook the streaming session puts into its `NetConfig`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn hook() -> CtlLogHook {
    global().hook()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const FIXTURE: &str = include_str!("../../../fixtures/client-log-upload-contract.json");

    fn fixture(name: &str) -> (String, Value) {
        let all: Value = serde_json::from_str(FIXTURE).unwrap();
        let message = all["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .unwrap_or_else(|| panic!("no fixture {name}"));
        (message["json"].to_string(), message["json"].clone())
    }

    fn collect(log: &CtlLog) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel();
        log.subscribe(move |line| tx.send(line).is_ok());
        rx
    }

    fn parse(line: String) -> Value {
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn send_refuses_without_a_connection_and_state_is_closed() {
        let log = CtlLog::new();
        let (batch, _) = fixture("logs-batch");
        assert!(!log.send(batch));
        let state = log.state();
        assert!(!state.open);
        assert_eq!(state.buffered_bytes, 0);
        assert_eq!(state.since_last_input_ms, i64::MAX, "no input yet");
    }

    #[test]
    fn send_refuses_other_kinds_and_garbage() {
        let log = CtlLog::new();
        assert!(!log.send(r#"{"type":"input"}"#.into()));
        assert!(!log.send("nope".into()));
    }

    #[test]
    fn events_are_encoded_per_kind() {
        let opened = CtlLogEvent::Opened {
            pc_id: "PC-1".into(),
            peer_caps: vec!["control-aead-v1".into(), "client-logs-v1".into()],
        };
        assert_eq!(
            parse(encode_event(&opened).unwrap()),
            json!({"kind":"opened","pcId":"PC-1","peerCaps":["control-aead-v1","client-logs-v1"]})
        );
        assert_eq!(
            parse(encode_event(&CtlLogEvent::Closed).unwrap()),
            json!({"kind":"closed"})
        );
        let (text, value) = fixture("logs-state");
        assert_eq!(
            parse(encode_event(&CtlLogEvent::Message(text)).unwrap()),
            json!({"kind":"message","json":value})
        );
        assert_eq!(
            encode_event(&CtlLogEvent::Message("not json".into())),
            None,
            "a non-JSON message is dropped"
        );
    }

    #[test]
    fn contract_host_messages_reach_the_subscriber_in_order() {
        let log = CtlLog::new();
        let rx = collect(&log);
        log.emit(CtlLogEvent::Opened {
            pc_id: "PC-1".into(),
            peer_caps: vec!["client-logs-v1".into()],
        });
        let mut expected = Vec::new();
        for name in [
            "logs-state",
            "logs-ack-ok",
            "logs-ack-rate-limited",
            "logs-ack-budget",
            "logs-ack-rejected",
            "feedback-ack-ok",
        ] {
            let (text, value) = fixture(name);
            log.emit(CtlLogEvent::Message(text));
            expected.push(json!({"kind":"message","json":value}));
        }
        log.emit(CtlLogEvent::Closed);

        let got: Vec<Value> = rx.try_iter().map(parse).collect();
        assert_eq!(got.len(), 8);
        assert_eq!(got[0]["kind"], "opened");
        assert_eq!(got[1..7], expected[..]);
        assert_eq!(got[7], json!({"kind":"closed"}));
    }

    #[test]
    fn a_new_subscriber_replaces_the_old_and_gets_the_open_connection_replayed() {
        let log = CtlLog::new();
        let first = collect(&log);
        log.emit(CtlLogEvent::Opened {
            pc_id: "PC-1".into(),
            peer_caps: vec!["client-logs-v1".into()],
        });
        assert_eq!(parse(first.try_recv().unwrap())["kind"], "opened");

        let second = collect(&log);
        assert_eq!(
            parse(second.try_recv().unwrap()),
            json!({"kind":"opened","pcId":"PC-1","peerCaps":["client-logs-v1"]}),
            "replayed"
        );
        log.emit(CtlLogEvent::Closed);
        assert_eq!(parse(second.try_recv().unwrap())["kind"], "closed");
        assert!(
            first.try_recv().is_err(),
            "the old subscription is replaced"
        );

        // No replay once the connection is closed.
        let third = collect(&log);
        assert!(third.try_recv().is_err());
    }

    #[test]
    fn a_closed_stream_is_dropped_without_failing_the_hook() {
        let log = CtlLog::new();
        let rx = collect(&log);
        drop(rx);
        log.emit(CtlLogEvent::Closed);
        log.emit(CtlLogEvent::Closed);
        assert!(lock(&log.link).subscriber.is_none());
    }

    #[test]
    fn the_global_hook_is_one_shared_instance() {
        assert!(!global().state().open);
        assert!(!hook().is_connected());
    }
}
