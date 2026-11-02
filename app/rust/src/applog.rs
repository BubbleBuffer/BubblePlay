//! Minimal JSONL error log for the Rust core of the Windows app (spec: lane/logging).
//!
//! Lines go to `<logs>/misc/app-win-rust-YYYY-MM-DD.jsonl`, or
//! `<logs>/pc/<pc-id>/app-win-rust-YYYY-MM-DD.jsonl` when the line relates to a PC, where
//! `<logs>` is `%LOCALAPPDATA%\BubblePlay\viewer\logs` (the Dart log store's root, which owns
//! the size and age caps for this tree; this module only appends to day files).
//!
//! Never panics and never waits on anything but one small file append. Secrets are redacted
//! before writing (spec rules 1-6); callers should still never pass them in.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

const SRC: &str = "app-win";
const FILE_PREFIX: &str = "app-win-rust";
const MAX_LINE: usize = 8 * 1024;
const REDACTED: &str = "[redacted]";
const BLOB: &str = "[redacted-blob]";
const TRUNCATED: &str = "...[truncated]";
/// Most lines written per minute; the rest are dropped.
const PER_MINUTE: u32 = 60;

static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static LIMIT: Mutex<Option<(Instant, u32)>> = Mutex::new(None);

/// Registers a literal secret that must never reach the file (spec rule 5).
pub fn register_secret(secret: &str) {
    if secret.is_empty() {
        return;
    }
    if let Ok(mut list) = SECRETS.lock() {
        if !list.iter().any(|known| known == secret) {
            list.push(secret.to_owned());
        }
    }
}

/// Logs an error line. `pc` is the PC id the line relates to, if any.
pub fn error(pc: Option<&str>, target: &str, msg: &str, error: &dyn std::fmt::Display) {
    log(
        Level::Error,
        pc,
        target,
        msg,
        json!({ "error": error.to_string() }),
    );
}

#[derive(Clone, Copy)]
pub enum Level {
    Warn,
    Error,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// Writes one line under the default log root. Silent when the root is unknown or on any failure.
pub fn log(level: Level, pc: Option<&str>, target: &str, msg: &str, fields: Value) {
    let Some(root) = default_root() else { return };
    let _ = std::panic::catch_unwind(|| {
        if allow() {
            write_line(&root, level, pc, target, msg, fields, SystemTime::now());
        }
    });
}

fn default_root() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(local)
            .join("BubblePlay")
            .join("viewer")
            .join("logs"),
    )
}

/// Rate limit: `PER_MINUTE` lines a minute. Never blocks: a contended lock drops the line.
fn allow() -> bool {
    let Ok(mut guard) = LIMIT.try_lock() else {
        return false;
    };
    let now = Instant::now();
    let (start, count) = guard.get_or_insert((now, 0));
    if now.duration_since(*start).as_secs() >= 60 {
        *start = now;
        *count = 0;
    }
    if *count >= PER_MINUTE {
        return false;
    }
    *count += 1;
    true
}

fn write_line(
    root: &Path,
    level: Level,
    pc: Option<&str>,
    target: &str,
    msg: &str,
    fields: Value,
    when: SystemTime,
) {
    let pc = pc.and_then(sanitize_id);
    let line = build_line(level, pc.as_deref(), target, msg, fields, when);
    let day = &rfc3339(when)[..10];
    let dir = match &pc {
        Some(pc) => root.join("pc").join(pc),
        None => root.join("misc"),
    };
    let _ = append(&dir.join(format!("{FILE_PREFIX}-{day}.jsonl")), &line);
}

fn append(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

/// Only `[A-Za-z0-9_-]`, at most 64 characters; anything else is rejected.
fn sanitize_id(id: &str) -> Option<String> {
    let id = id.trim();
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then(|| id.to_owned())
}

/// One redacted, size-capped JSON line ending in a newline.
fn build_line(
    level: Level,
    pc: Option<&str>,
    target: &str,
    msg: &str,
    fields: Value,
    when: SystemTime,
) -> String {
    let result = std::panic::catch_unwind(|| {
        let msg = redact_text(msg);
        let fields = redact_value(&fields, 0);
        compose(level, pc, target, msg, fields, when)
    });
    result.unwrap_or_else(|_| {
        compose(
            level,
            pc,
            target,
            "[redaction failed]".to_owned(),
            json!({}),
            when,
        )
    })
}

fn compose(
    level: Level,
    pc: Option<&str>,
    target: &str,
    mut msg: String,
    mut fields: Value,
    when: SystemTime,
) -> String {
    let target = format!("rust::{}", redact_text(target));
    let mut fields_dropped = false;
    loop {
        let mut line = json!({
            "ts": rfc3339(when),
            "level": level.name(),
            "src": SRC,
            "device": Value::Null,
            "pc": pc,
            "session": Value::Null,
            "target": target,
            "msg": msg,
            "fields": fields,
        })
        .to_string();
        line.push('\n');
        if line.len() <= MAX_LINE {
            return line;
        }
        // Too long: drop the fields first, then shorten the message.
        if !fields_dropped {
            fields_dropped = true;
            fields = json!({ "truncated": true });
            continue;
        }
        let excess = line.len() - MAX_LINE + TRUNCATED.len() + 64;
        let mut cut = msg.len().saturating_sub(excess).min(msg.len() / 2);
        while !msg.is_char_boundary(cut) {
            cut -= 1;
        }
        msg.truncate(cut);
        msg.push_str(TRUNCATED);
    }
}

// ---- redaction ----

const BAD_KEY_PARTS: &[&str] = &[
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
const ALLOWED_KEY_PARTS: &[&str] = &["keyboard", "keyframe", "keycode", "keyid"];
const URL_PARAMS: &[&str] = &[
    "token",
    "key",
    "secret",
    "password",
    "passphrase",
    "code",
    "ticket",
    "proof",
    "pairing",
    "sig",
];

/// Rule 1: ignore `_ - .` separators and case (so camelCase words join too), drop allow-listed
/// words, then look for a secret-bearing part.
fn key_is_secret(key: &str) -> bool {
    let mut words: String = key
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | '.'))
        .map(|c| c.to_ascii_lowercase())
        .collect();
    for allowed in ALLOWED_KEY_PARTS {
        words = words.replace(allowed, "");
    }
    BAD_KEY_PARTS.iter().any(|part| words.contains(part))
}

fn redact_value(value: &Value, depth: usize) -> Value {
    if depth > 8 {
        return json!(REDACTED);
    }
    match value {
        Value::String(text) => Value::String(redact_text(text)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| redact_value(v, depth + 1)).collect())
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                let item = if key_is_secret(key) {
                    json!(REDACTED)
                } else {
                    redact_value(item, depth + 1)
                };
                out.insert(key.clone(), item);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Rules 5, 3, 4 and 2, then 6, in that order.
fn redact_text(text: &str) -> String {
    let mut text = text.to_owned();
    let mut secrets = SECRETS.lock().map(|list| list.clone()).unwrap_or_default();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for secret in &secrets {
        text = text.replace(secret.as_str(), REDACTED);
    }
    text = redact_authorization(&text);
    text = redact_bearer(&text);
    text = redact_pairs(&text);
    redact_blobs(&text)
}

fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let lower = haystack.to_ascii_lowercase();
    lower.get(from..)?.find(needle).map(|i| i + from)
}

/// Rule 3, header form: everything after `Authorization:` to the end of the line.
fn redact_authorization(text: &str) -> String {
    const HEADER: &str = "authorization:";
    let mut out = String::new();
    let mut rest = 0;
    while let Some(at) = find_ci(text, HEADER, rest) {
        let value_start = at + HEADER.len();
        let end = text[value_start..]
            .find('\n')
            .map_or(text.len(), |i| value_start + i);
        out.push_str(&text[rest..value_start]);
        out.push(' ');
        out.push_str(REDACTED);
        rest = end;
    }
    out.push_str(&text[rest..]);
    out
}

/// Rule 3: `Bearer <x>` becomes `Bearer [redacted]`.
fn redact_bearer(text: &str) -> String {
    const WORD: &str = "bearer ";
    let mut out = String::new();
    let mut rest = 0;
    while let Some(at) = find_ci(text, WORD, rest) {
        let value_start = at + WORD.len();
        let end = text[value_start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |i| value_start + i);
        out.push_str(&text[rest..at]);
        out.push_str("Bearer ");
        out.push_str(REDACTED);
        rest = end;
    }
    out.push_str(&text[rest..]);
    out
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '.'
}

/// Rules 2 and 4: `key=value`, `key: value`, `"key":"value"`, and URL query/fragment params.
fn redact_pairs(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut i = 0;
    while i < n {
        if !is_word(chars[i]) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut j = i;
        while j < n && is_word(chars[j]) {
            j += 1;
        }
        let word: String = chars[i..j].iter().collect();
        out.push_str(&word);
        let mut k = j;
        if k < n && (chars[k] == '"' || chars[k] == '\'') {
            k += 1;
        }
        let after_url_mark = i > 0 && matches!(chars[i - 1], '?' | '&' | '#');
        let matches_key = key_is_secret(&word)
            || (after_url_mark && URL_PARAMS.contains(&word.to_ascii_lowercase().as_str()));
        if matches_key && k < n && (chars[k] == '=' || chars[k] == ':') {
            let mut v = k + 1;
            while v < n && (chars[v] == ' ' || chars[v] == '\t') {
                v += 1;
            }
            let quote = if v < n && (chars[v] == '"' || chars[v] == '\'') {
                Some(chars[v])
            } else {
                None
            };
            let (start, end) = if let Some(q) = quote {
                let s = v + 1;
                let e = chars[s..].iter().position(|c| *c == q).map_or(n, |p| s + p);
                (s, e)
            } else {
                let e = chars[v..]
                    .iter()
                    .position(|c| {
                        c.is_whitespace()
                            || matches!(c, ',' | ';' | '&' | '#' | '"' | '\'' | ')' | '}')
                    })
                    .map_or(n, |p| v + p);
                (v, e)
            };
            out.extend(&chars[j..start]);
            out.push_str(REDACTED);
            i = end;
        } else {
            i = j;
        }
    }
    out
}

/// Rule 6: runs of 40 or more `[A-Za-z0-9+/_=-]` characters.
fn redact_blobs(text: &str) -> String {
    let blob_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '_' | '=' | '-');
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().count() >= 40 {
            out.push_str(BLOB);
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if blob_char(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

// ---- time ----

/// UTC RFC3339 with exactly three fractional digits and `Z`.
fn rfc3339(when: SystemTime) -> String {
    let since = when.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // 2026-11-01T13:25:12.345Z
    const T0_MS: u64 = 1_793_539_512_345;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("bp-applog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn all_text(root: &Path) -> String {
        let mut out = String::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push_str(&std::fs::read_to_string(path).unwrap());
                }
            }
        }
        out
    }

    #[test]
    fn timestamp_format() {
        let t = UNIX_EPOCH + Duration::from_millis(T0_MS);
        assert_eq!(rfc3339(t), "2026-11-01T13:25:12.345Z");
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn rule1_secret_keys_and_allow_list() {
        for key in [
            "password",
            "apiToken",
            "pairing_code",
            "Auth",
            "secret-key",
            "device.key",
            "Cookie",
            "psk",
        ] {
            assert!(key_is_secret(key), "{key}");
        }
        for key in [
            "keyboard", "keyframe", "keycode", "key_id", "error", "stack", "pc",
        ] {
            assert!(!key_is_secret(key), "{key}");
        }
        let v = redact_value(
            &json!({"token": "abc", "nested": {"passphrase": 5}, "error": "boom"}),
            0,
        );
        assert_eq!(v["token"], REDACTED);
        assert_eq!(v["nested"]["passphrase"], REDACTED);
        assert_eq!(v["error"], "boom");
    }

    #[test]
    fn rule2_url_params() {
        let out = redact_text("open https://x.test/p?sig=abc123&name=ok#proof=zzz end");
        assert!(!out.contains("abc123") && !out.contains("zzz"), "{out}");
        assert!(out.contains("name=ok"), "{out}");
    }

    #[test]
    fn rule3_bearer_and_authorization() {
        assert_eq!(
            redact_text("hdr Bearer sekrit1 done"),
            "hdr Bearer [redacted] done"
        );
        let out = redact_text("a\nAuthorization: Basic dXNlcjpwdw==\nb");
        assert!(!out.contains("dXNlcjpwdw"), "{out}");
        assert!(out.contains("\nb"));
    }

    #[test]
    fn rule4_free_text_pairs() {
        let out =
            redact_text(r#"failed password=hunter2 then {"ticket":"t-9 9"} and code: 123456 ok"#);
        for leaked in ["hunter2", "t-9 9", "123456"] {
            assert!(!out.contains(leaked), "{out}");
        }
        assert!(out.contains("failed") && out.contains("ok"));
    }

    #[test]
    fn rule5_registered_literal() {
        register_secret("planted-literal-77");
        let out = redact_text("oops planted-literal-77 leaked");
        assert!(!out.contains("planted-literal-77"), "{out}");
    }

    #[test]
    fn rule6_blobs() {
        let blob = "A".repeat(39);
        assert_eq!(redact_text(&blob), blob);
        let out = redact_text(&format!("x {blob}B end"));
        assert_eq!(out, format!("x {BLOB} end"));
    }

    #[test]
    fn written_file_never_contains_secrets() {
        let root = temp_root("secrets");
        register_secret("file-literal-secret-42");
        write_line(
            &root,
            Level::Error,
            Some("pc_1"),
            "backend::forget",
            "bad file-literal-secret-42 password=hunter2 Bearer abcdef",
            json!({"error": "x file-literal-secret-42", "token": "tok-123", "apiKey": "k"}),
            UNIX_EPOCH + Duration::from_millis(T0_MS),
        );
        let text = all_text(&root);
        for leaked in ["file-literal-secret-42", "hunter2", "abcdef", "tok-123"] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
        let path = root
            .join("pc")
            .join("pc_1")
            .join("app-win-rust-2026-11-01.jsonl");
        let line = std::fs::read_to_string(path).unwrap();
        let v: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(v["src"], "app-win");
        assert_eq!(v["level"], "error");
        assert_eq!(v["pc"], "pc_1");
        assert_eq!(v["target"], "rust::backend::forget");
        assert!(line.ends_with('\n'));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn misc_file_without_pc_and_bad_pc_id() {
        let root = temp_root("misc");
        let when = UNIX_EPOCH + Duration::from_millis(T0_MS);
        write_line(&root, Level::Warn, None, "t", "m", json!({}), when);
        write_line(
            &root,
            Level::Warn,
            Some("../evil"),
            "t",
            "m",
            json!({}),
            when,
        );
        let file = root.join("misc").join("app-win-rust-2026-11-01.jsonl");
        assert_eq!(std::fs::read_to_string(file).unwrap().lines().count(), 2);
        assert!(!root.join("pc").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn line_cap_truncates() {
        let when = UNIX_EPOCH + Duration::from_millis(T0_MS);
        let long = "word ".repeat(5000);
        let line = build_line(Level::Error, None, "t", &long, json!({"error": long}), when);
        assert!(line.len() <= MAX_LINE, "{}", line.len());
        let v: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert!(v["msg"].as_str().unwrap().ends_with(TRUNCATED));
    }
}
