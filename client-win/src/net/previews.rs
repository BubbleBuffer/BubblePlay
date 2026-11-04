//! Monitor previews (`monitor-previews-v1`, docs/architecture/monitor-previews.md): small JPEG
//! snapshots of the monitors the host is not streaming, sent over the sealed ctl channel.
//!
//! [`MonitorPreviews`] validates each `monitor-preview` message and holds the newest picture
//! per monitor selector, in memory only. The session clears it when a connection opens and
//! when it ends. A message that fails any check is ignored; nothing here blocks the session.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::Value;

use super::controls::{MAX_MONITORS, MAX_SELECTOR_CHARS};

/// Advertised in the viewer's ctl `caps`; without it the host sends no pictures.
pub const CAP_MONITOR_PREVIEWS: &str = "monitor-previews-v1";
/// The ctl message kind.
pub const PREVIEW_KIND: &str = "monitor-preview";

/// Longest accepted message JSON in bytes (`limits.maxMessageJsonBytes`).
pub const MAX_MESSAGE_BYTES: usize = 140_000;
/// Longest accepted decoded JPEG (`limits.maxJpegBytes`).
pub const MAX_JPEG_BYTES: usize = 96 * 1024;
/// Widest accepted picture.
pub const MAX_WIDTH: u64 = 960;
/// Tallest accepted picture.
pub const MAX_HEIGHT: u64 = 360;

/// One held picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorPreview {
    /// The `selector` of a `monitor-list` entry.
    pub monitor: String,
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    /// A baseline JPEG (starts with SOI, ends with EOI).
    pub jpeg: Arc<[u8]>,
}

/// Why a message was not held. Only for tests and counters; the wire has no reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ignored {
    NotJson,
    Version,
    Field,
    Size,
    Base64,
    NotJpeg,
    OlderSeq,
    TooManyMonitors,
}

/// The newest picture per monitor selector. Cloning shares the store.
#[derive(Clone, Debug, Default)]
pub struct MonitorPreviews {
    held: Arc<Mutex<HashMap<String, MonitorPreview>>>,
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn parse(text: &str) -> Result<MonitorPreview, Ignored> {
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(Ignored::Size);
    }
    let value: Value = serde_json::from_str(text).map_err(|_| Ignored::NotJson)?;
    if value["type"] != PREVIEW_KIND {
        return Err(Ignored::Field);
    }
    if value["v"].as_u64() != Some(1) {
        return Err(Ignored::Version);
    }
    let monitor = value["monitor"].as_str().ok_or(Ignored::Field)?;
    if monitor.is_empty() || utf16_len(monitor) > MAX_SELECTOR_CHARS {
        return Err(Ignored::Field);
    }
    let seq = value["seq"].as_u64().ok_or(Ignored::Field)?;
    let width = value["w"].as_u64().ok_or(Ignored::Field)?;
    let height = value["h"].as_u64().ok_or(Ignored::Field)?;
    if width == 0 || height == 0 || width > MAX_WIDTH || height > MAX_HEIGHT {
        return Err(Ignored::Size);
    }
    let encoded = value["jpeg"].as_str().ok_or(Ignored::Field)?;
    // Base64 is 4 characters per 3 bytes: refuse an oversized body before decoding it.
    if encoded.len() > MAX_JPEG_BYTES.div_ceil(3) * 4 {
        return Err(Ignored::Size);
    }
    let jpeg = STANDARD.decode(encoded).map_err(|_| Ignored::Base64)?;
    if jpeg.len() > MAX_JPEG_BYTES {
        return Err(Ignored::Size);
    }
    if jpeg.len() < 4 || jpeg[..2] != [0xFF, 0xD8] || jpeg[jpeg.len() - 2..] != [0xFF, 0xD9] {
        return Err(Ignored::NotJpeg);
    }
    Ok(MonitorPreview {
        monitor: monitor.to_owned(),
        seq,
        // Bounded by the checks above.
        width: width as u32,
        height: height as u32,
        jpeg: jpeg.into(),
    })
}

impl MonitorPreviews {
    pub fn new() -> Self {
        Self::default()
    }

    fn held(&self) -> MutexGuard<'_, HashMap<String, MonitorPreview>> {
        self.held.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Validates one `monitor-preview` message and holds it as the newest picture of its
    /// monitor. An older `seq` than the held picture of that monitor is ignored.
    pub fn accept(&self, text: &str) -> Result<(), Ignored> {
        let preview = parse(text)?;
        let mut held = self.held();
        match held.get(&preview.monitor) {
            Some(old) if preview.seq < old.seq => return Err(Ignored::OlderSeq),
            None if held.len() >= MAX_MONITORS => return Err(Ignored::TooManyMonitors),
            _ => {}
        }
        held.insert(preview.monitor.clone(), preview);
        Ok(())
    }

    /// The held picture of `selector`, if any; it stays held.
    pub fn get(&self, selector: &str) -> Option<MonitorPreview> {
        self.held().get(selector).cloned()
    }

    /// Removes and returns the held picture of `selector` (the monitor that went live).
    pub fn take(&self, selector: &str) -> Option<MonitorPreview> {
        self.held().remove(selector)
    }

    /// Drops every picture (a connection opened or ended).
    pub fn clear(&self) {
        self.held().clear();
    }

    pub fn len(&self) -> usize {
        self.held().len()
    }

    pub fn is_empty(&self) -> bool {
        self.held().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FIXTURE: &str = include_str!("../../../fixtures/monitor-previews-contract.json");
    const DISPLAY2: &str = "\\\\.\\DISPLAY2";

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).unwrap()
    }

    fn vector<'a>(fixture: &'a Value, list: &str, name: &str) -> &'a Value {
        fixture[list]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap_or_else(|| panic!("fixture vector {name}"))
    }

    #[test]
    fn the_capability_and_limits_match_the_fixture() {
        let f = fixture();
        assert_eq!(f["capability"], CAP_MONITOR_PREVIEWS);
        assert_eq!(f["messages"][0]["name"], PREVIEW_KIND);
        assert_eq!(f["limits"]["maxJpegBytes"], MAX_JPEG_BYTES);
        assert_eq!(f["limits"]["maxMessageJsonBytes"], MAX_MESSAGE_BYTES);
        assert_eq!(f["limits"]["maxDimension"], MAX_WIDTH);
    }

    #[test]
    fn every_accept_vector_is_held() {
        let f = fixture();
        for entry in f["accept"].as_array().unwrap() {
            let store = MonitorPreviews::new();
            let text = entry["json"].to_string();
            assert_eq!(store.accept(&text), Ok(()), "{}", entry["name"]);
            let monitor = entry["json"]["monitor"].as_str().unwrap();
            let held = store.get(monitor).unwrap();
            assert_eq!(held.seq, entry["json"]["seq"].as_u64().unwrap());
            assert_eq!(&held.jpeg[..2], &[0xFF, 0xD8]);
        }
    }

    #[test]
    fn every_ignore_vector_is_ignored() {
        let f = fixture();
        for entry in f["ignore"].as_array().unwrap() {
            let store = MonitorPreviews::new();
            if let Some(after) = entry["after"].as_str() {
                let first = vector(&f, "accept", after)["json"].to_string();
                store.accept(&first).unwrap();
            }
            let before = store.len();
            let held_before = store.get(DISPLAY2);
            assert!(
                store.accept(&entry["json"].to_string()).is_err(),
                "{}",
                entry["name"]
            );
            assert_eq!(store.len(), before, "{}", entry["name"]);
            assert_eq!(store.get(DISPLAY2), held_before, "{}", entry["name"]);
        }
    }

    #[test]
    fn the_ignore_vectors_fail_for_their_reason() {
        let f = fixture();
        let reason = |name: &str| {
            MonitorPreviews::new()
                .accept(&vector(&f, "ignore", name)["json"].to_string())
                .unwrap_err()
        };
        assert_eq!(reason("unknown-version"), Ignored::Version);
        assert_eq!(reason("missing-monitor"), Ignored::Field);
        assert_eq!(reason("bad-base64"), Ignored::Base64);
        assert_eq!(reason("not-a-jpeg"), Ignored::NotJpeg);
        assert_eq!(reason("zero-size"), Ignored::Size);
    }

    fn with(f: &Value, patch: impl FnOnce(&mut Value)) -> String {
        let mut json = vector(f, "accept", "valid")["json"].clone();
        patch(&mut json);
        json.to_string()
    }

    #[test]
    fn an_older_seq_loses_and_a_newer_one_replaces() {
        let f = fixture();
        let store = MonitorPreviews::new();
        store.accept(&with(&f, |_| {})).unwrap();
        let older = with(&f, |j| j["seq"] = json!(3));
        assert_eq!(store.accept(&older), Err(Ignored::OlderSeq));
        let newer = with(&f, |j| {
            j["seq"] = json!(9);
            j["w"] = json!(32);
        });
        store.accept(&newer).unwrap();
        let held = store.get(DISPLAY2).unwrap();
        assert_eq!((held.seq, held.width), (9, 32));
    }

    #[test]
    fn seq_is_kept_per_monitor() {
        let f = fixture();
        let store = MonitorPreviews::new();
        store.accept(&with(&f, |_| {})).unwrap();
        let other = with(&f, |j| {
            j["monitor"] = json!("\\\\.\\DISPLAY3");
            j["seq"] = json!(1);
        });
        store.accept(&other).unwrap();
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn size_and_shape_limits() {
        let f = fixture();
        let store = MonitorPreviews::new();
        for (name, patch) in [
            ("too wide", json!({"w": 961})),
            ("too tall", json!({"h": 361})),
            ("zero width", json!({"w": 0})),
            ("negative seq", json!({"seq": -1})),
            ("string seq", json!({"seq": "7"})),
            ("empty monitor", json!({"monitor": ""})),
            ("long monitor", json!({"monitor": "x".repeat(129)})),
            ("wrong type", json!({"type": "other"})),
        ] {
            let text = with(&f, |j| {
                for (key, value) in patch.as_object().unwrap() {
                    j[key] = value.clone();
                }
            });
            assert!(store.accept(&text).is_err(), "{name}");
        }
        assert!(store.is_empty());
    }

    fn jpeg_message(body: &[u8]) -> String {
        json!({"type": "monitor-preview", "v": 1, "monitor": DISPLAY2, "seq": 1,
            "w": 16, "h": 9, "jpeg": STANDARD.encode(body)})
        .to_string()
    }

    #[test]
    fn the_jpeg_must_have_both_markers_and_fit() {
        let ok = MonitorPreviews::new();
        assert!(ok
            .accept(&jpeg_message(&[0xFF, 0xD8, 0x00, 0xFF, 0xD9]))
            .is_ok());
        let none = MonitorPreviews::new();
        for body in [
            &[0xFF, 0xD8, 0x00, 0x00][..],
            &[0x00, 0x00, 0xFF, 0xD9][..],
            &[0xFF, 0xD8, 0xFF][..],
        ] {
            assert_eq!(none.accept(&jpeg_message(body)), Err(Ignored::NotJpeg));
        }
        let framed = |len: usize| {
            let mut body = vec![0u8; len];
            body[0] = 0xFF;
            body[1] = 0xD8;
            body[len - 2] = 0xFF;
            body[len - 1] = 0xD9;
            body
        };
        assert_eq!(
            none.accept(&jpeg_message(&framed(MAX_JPEG_BYTES + 1))),
            Err(Ignored::Size)
        );
        assert!(none.accept(&jpeg_message(&framed(MAX_JPEG_BYTES))).is_ok());
    }

    #[test]
    fn an_oversized_message_is_refused() {
        let store = MonitorPreviews::new();
        let text = format!(
            "{}{}",
            jpeg_message(&[0xFF, 0xD8, 0xFF, 0xD9]),
            " ".repeat(MAX_MESSAGE_BYTES)
        );
        assert_eq!(store.accept(&text), Err(Ignored::Size));
        assert_eq!(store.accept("not json"), Err(Ignored::NotJson));
    }

    #[test]
    fn at_most_sixteen_monitors_are_held() {
        let store = MonitorPreviews::new();
        let base = jpeg_message(&[0xFF, 0xD8, 0xFF, 0xD9]);
        for n in 0..MAX_MONITORS {
            store
                .accept(&base.replace("DISPLAY2", &format!("DISPLAY{n}")))
                .unwrap();
        }
        let extra = base.replace("DISPLAY2", "DISPLAY99");
        assert_eq!(store.accept(&extra), Err(Ignored::TooManyMonitors));
        // A newer picture of a held monitor still replaces.
        let newer = base
            .replace("DISPLAY2", "DISPLAY1")
            .replace("\"seq\":1", "\"seq\":2");
        assert!(store.accept(&newer).is_ok());
    }

    #[test]
    fn take_and_clear_drop_pictures() {
        let f = fixture();
        let store = MonitorPreviews::new();
        let shared = store.clone();
        store.accept(&with(&f, |_| {})).unwrap();
        assert!(shared.take(DISPLAY2).is_some());
        assert!(shared.take(DISPLAY2).is_none());
        store.accept(&with(&f, |_| {})).unwrap();
        shared.clear();
        assert!(store.is_empty());
    }
}
