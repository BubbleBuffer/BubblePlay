//! A minimal structured-log facade for the relay (logging spec sections 1 to 3).
//!
//! The core has no clock, no console and no file. A line is built here as one JSON
//! object (`ts`, `level`, `src` "relay", `device`, `pc`, `session`, `target`, `msg`,
//! `fields`) and handed to a sink the adapter installs once at start-up: the native
//! server writes it to stderr, the Worker to the console. With no sink installed a
//! line is dropped, so library users and tests stay quiet.
//!
//! Redaction runs on `msg` and on every string in `fields` before the line leaves
//! this module. Callers still never pass secrets; redaction is the net. Device ids
//! reach `emit` already truncated to their first 8 characters (see [`short_id`]).

use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value, json};

/// Line severity, lowest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl Level {
    /// The lowercase name written into the line.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// Receives each finished line (no trailing newline).
pub type Sink = fn(Level, &str);

/// A line is at most this many bytes.
pub const MAX_LINE_BYTES: usize = 8 * 1024;

const REDACTED: &str = "[redacted]";
const BLOB: &str = "[redacted-blob]";
const TRUNCATED: &str = "...[truncated]";

static SINK: OnceLock<Sink> = OnceLock::new();
static MIN_LEVEL: AtomicU8 = AtomicU8::new(Level::Debug as u8);
static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

thread_local! {
    static CAPTURE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// Installs the sink. Only the first call takes effect; returns whether it did.
pub fn set_sink(sink: Sink) -> bool {
    SINK.set(sink).is_ok()
}

/// Lines below this level are dropped. The default keeps everything.
pub fn set_min_level(level: Level) {
    MIN_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Registers a literal secret (a configured token, say). Any occurrence in a line is
/// replaced, longest first. Empty and very short values are ignored.
pub fn register_secret(secret: &str) {
    if secret.len() < 4 {
        return;
    }
    if let Ok(mut list) = SECRETS.lock()
        && !list.iter().any(|known| known == secret)
    {
        list.push(secret.to_owned());
        list.sort_by_key(|known| std::cmp::Reverse(known.len()));
    }
}

/// Runs `f` and returns what it logged on this thread, in order, instead of sending
/// it to the sink. For tests.
pub fn capture<R>(f: impl FnOnce() -> R) -> (R, Vec<String>) {
    CAPTURE.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
    let result = f();
    let lines = CAPTURE.with(|slot| slot.borrow_mut().take().unwrap_or_default());
    (result, lines)
}

/// The first 8 characters of a device id, which is all a relay log may carry.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Writes one line. `ts_ms` is unix milliseconds, `pc` an already truncated PC id,
/// `fields` a JSON object (any other value is ignored). Never panics.
pub fn emit(level: Level, ts_ms: u64, target: &str, msg: &str, pc: Option<&str>, fields: Value) {
    if (level as u8) < MIN_LEVEL.load(Ordering::Relaxed) {
        return;
    }
    let capturing = CAPTURE.with(|slot| slot.borrow().is_some());
    if !capturing && SINK.get().is_none() {
        return;
    }
    let line = build_line(level, ts_ms, target, msg, pc, fields);
    if capturing {
        CAPTURE.with(|slot| {
            if let Some(lines) = slot.borrow_mut().as_mut() {
                lines.push(line);
            }
        });
    } else if let Some(sink) = SINK.get() {
        sink(level, &line);
    }
}

fn build_line(
    level: Level,
    ts_ms: u64,
    target: &str,
    msg: &str,
    pc: Option<&str>,
    fields: Value,
) -> String {
    let fields = match fields {
        Value::Object(map) => redact_object(map),
        _ => Map::new(),
    };
    let mut msg = redact_text(msg);
    let mut fields = Value::Object(fields);
    let render = |msg: &str, fields: &Value| {
        json!({
            "ts": rfc3339_ms(ts_ms),
            "level": level.as_str(),
            "src": "relay",
            "device": Value::Null,
            "pc": pc.map(|id| redact_text(&short_id(id))),
            "session": Value::Null,
            "target": target,
            "msg": msg,
            "fields": fields,
        })
        .to_string()
    };
    let mut line = render(&msg, &fields);
    if line.len() > MAX_LINE_BYTES {
        let keep: String = msg.chars().take(2048).collect();
        msg = format!("{keep}{TRUNCATED}");
        line = render(&msg, &fields);
        if line.len() > MAX_LINE_BYTES {
            fields = json!({});
            line = render(&msg, &fields);
        }
    }
    line
}

// ---------------------------------------------------------------------------
// Redaction (spec section 2, rules 1 to 6)
// ---------------------------------------------------------------------------

const SENSITIVE_WORDS: [&str; 15] = [
    "password",
    "passphrase",
    "passwd",
    "secret",
    "token",
    "key",
    "proof",
    "ticket",
    "code",
    "pairing",
    "psk",
    "signature",
    "auth",
    "cookie",
    "credential",
];
const ALLOWED_KEY_WORDS: [&str; 4] = ["keyboard", "keyframe", "keycode", "keyid"];

/// Rule 1: a key whose normalised form contains a sensitive word, after the allowed
/// words are removed. `deviceKeyId`, `keyframe` and `keyboard` pass.
pub fn key_is_sensitive(key: &str) -> bool {
    let mut normal: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    for allowed in ALLOWED_KEY_WORDS {
        normal = normal.replace(allowed, "");
    }
    SENSITIVE_WORDS.iter().any(|word| normal.contains(word)) || normal == "sig"
}

fn redact_object(map: Map<String, Value>) -> Map<String, Value> {
    map.into_iter()
        .map(|(key, value)| {
            let value = if key_is_sensitive(&key) {
                Value::String(REDACTED.to_owned())
            } else {
                redact_value(value)
            };
            (key, value)
        })
        .collect()
}

fn redact_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_text(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_value).collect()),
        Value::Object(map) => Value::Object(redact_object(map)),
        other => other,
    }
}

/// Rules 3 to 6 on free text, in an order that keeps later rules from re-reading
/// earlier replacements.
pub fn redact_text(text: &str) -> String {
    let mut out = text.to_owned();
    if let Ok(list) = SECRETS.lock() {
        for secret in list.iter() {
            out = out.replace(secret.as_str(), REDACTED);
        }
    }
    out = redact_authorization(&out);
    out = redact_pairs(&out);
    redact_blobs(&out)
}

/// Rule 3: `Bearer <x>` and whole `Authorization: ...` lines.
fn redact_authorization(text: &str) -> String {
    let mut lines = Vec::new();
    for line in text.split('\n') {
        let lower = line.to_ascii_lowercase();
        if let Some(at) = lower.find("authorization:") {
            lines.push(format!("{}Authorization: {REDACTED}", &line[..at]));
            continue;
        }
        lines.push(redact_bearer(line));
    }
    lines.join("\n")
}

fn redact_bearer(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let mut out = String::new();
    let mut from = 0;
    while let Some(found) = lower[from..].find("bearer ") {
        let start = from + found;
        let value_start = start + "bearer ".len();
        let value_end = line[value_start..]
            .find(|c: char| c.is_whitespace() || c == '"' || c == ',')
            .map_or(line.len(), |n| value_start + n);
        out.push_str(&line[from..value_start]);
        out.push_str(REDACTED);
        from = value_end;
    }
    out.push_str(&line[from..]);
    out
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// Rules 2 and 4: `key=value`, `key: value` and `"key":"value"` where the key is
/// sensitive. The value runs to the next whitespace, comma, ampersand, quote or end.
fn redact_pairs(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '=' && c != ':' {
            out.push(c);
            i += 1;
            continue;
        }
        // The key is the run of key characters just before the separator, with an
        // optional closing quote in between.
        let mut end = out.chars().count();
        let before: Vec<char> = out.chars().collect();
        if end > 0 && before[end - 1] == '"' {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && is_key_char(before[start - 1]) {
            start -= 1;
        }
        let key: String = before[start..end].iter().collect();
        out.push(c);
        i += 1;
        if key.is_empty() || !key_is_sensitive(&key) {
            continue;
        }
        while i < chars.len() && (chars[i] == ' ' || chars[i] == '"') {
            out.push(chars[i]);
            i += 1;
        }
        let value_start = i;
        while i < chars.len() && !matches!(chars[i], ' ' | '\t' | ',' | '&' | '"' | '\n' | '}') {
            i += 1;
        }
        if i > value_start {
            out.push_str(REDACTED);
        }
    }
    out
}

/// Rule 6: a run of 40 or more characters from `[A-Za-z0-9+/_=-]`.
fn redact_blobs(text: &str) -> String {
    let is_blob = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '_' | '=' | '-');
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for c in text.chars() {
        if is_blob(c) {
            run.push(c);
            continue;
        }
        flush_run(&mut out, &mut run);
        out.push(c);
    }
    flush_run(&mut out, &mut run);
    out
}

fn flush_run(out: &mut String, run: &mut String) {
    if run.chars().count() >= 40 {
        out.push_str(BLOB);
    } else {
        out.push_str(run);
    }
    run.clear();
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

/// Unix milliseconds as UTC RFC 3339 with exactly three fractional digits.
pub fn rfc3339_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(msg: &str, fields: Value) -> Value {
        let (_, lines) =
            capture(|| emit(Level::Info, 0, "test", msg, Some("ABCD-EFGH-JKLM"), fields));
        assert_eq!(lines.len(), 1);
        serde_json::from_str(&lines[0]).expect("a log line is JSON")
    }

    #[test]
    fn timestamp_is_rfc3339_with_three_digits() {
        assert_eq!(rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_ms(1_762_090_512_345), "2025-11-02T13:35:12.345Z");
        assert_eq!(rfc3339_ms(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn line_has_the_spec_shape_and_a_truncated_pc() {
        let line = one("hello", json!({"class": "x"}));
        assert_eq!(line["src"], "relay");
        assert_eq!(line["level"], "info");
        assert_eq!(line["pc"], "ABCD-EFG");
        assert!(line["device"].is_null());
        assert!(line["session"].is_null());
        assert_eq!(line["fields"]["class"], "x");
    }

    #[test]
    fn sensitive_keys_lose_their_value_and_allowed_ones_keep_it() {
        let line = one(
            "m",
            json!({"token": "abc", "pairingCode": "123456", "Pair_Proof": "p", "keyboard": "us",
                   "keyId": "k1", "status": 1008}),
        );
        for key in ["token", "pairingCode", "Pair_Proof"] {
            assert_eq!(line["fields"][key], REDACTED, "{key}");
        }
        assert_eq!(line["fields"]["keyboard"], "us");
        assert_eq!(line["fields"]["keyId"], "k1");
        assert_eq!(line["fields"]["status"], 1008);
    }

    #[test]
    fn free_text_secrets_are_replaced() {
        let line = one(
            "Authorization: Basic abc\nsent Bearer s3cr3t-value then token=abc123 and \"code\":\"987654\" and ?ticket=zzz&x=1",
            json!({}),
        );
        let msg = line["msg"].as_str().expect("msg");
        for leaked in ["Basic abc", "s3cr3t-value", "abc123", "987654", "zzz"] {
            assert!(!msg.contains(leaked), "{leaked} leaked in {msg}");
        }
        assert!(msg.contains("x=1"));
    }

    #[test]
    fn long_blobs_are_replaced_in_msg_and_fields() {
        let blob = "A".repeat(43);
        let line = one(&format!("value {blob} end"), json!({"note": blob.clone()}));
        assert_eq!(line["msg"], format!("value {BLOB} end"));
        assert_eq!(line["fields"]["note"], BLOB);
    }

    #[test]
    fn a_registered_literal_never_appears() {
        register_secret("hunter2-literal");
        let line = one(
            "the value hunter2-literal here",
            json!({"note": "x hunter2-literal"}),
        );
        let text = line.to_string();
        assert!(!text.contains("hunter2-literal"), "{text}");
    }

    #[test]
    fn oversized_lines_are_cut_to_the_cap() {
        let line = {
            let (_, lines) = capture(|| {
                emit(
                    Level::Info,
                    0,
                    "t",
                    &"word ".repeat(5000),
                    None,
                    json!({"a": "b"}),
                );
            });
            lines.into_iter().next().expect("one line")
        };
        assert!(line.len() <= MAX_LINE_BYTES);
        assert!(line.contains(TRUNCATED));
    }
}
