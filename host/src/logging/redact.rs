//! Redaction for log lines (spec rules 1-7). Pure functions, no I/O.
//!
//! Secrets are never meant to reach a log call in the first place; this is the net.
//! Replacement text is `[redacted]` (`[redacted-blob]` for long opaque runs).
//!
//! * Rule 1: object keys that name a secret (`password`, `token`, `key`, ...) lose their whole value.
//! * Rule 2: URL query and fragment parameters named like a secret lose their value.
//! * Rule 3: `Bearer <x>` and `Authorization: ...` header lines.
//! * Rule 4: `key=value`, `key: value`, `"key":"value"` in free text where the key names a secret.
//! * Rule 5: literal secrets registered at runtime with [`register_secret`], longest first.
//! * Rule 6: runs of 40 or more `[A-Za-z0-9+/_=-]` characters become `[redacted-blob]`,
//!   except in the dedicated `device`, `pc` and `session` fields.
//! * Rule 7: redaction never panics; [`redact_line`] falls back to a `[redaction failed]` line.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

use serde_json::{Map, Value};

pub const REDACTED: &str = "[redacted]";
pub const REDACTED_BLOB: &str = "[redacted-blob]";
/// Runs of at least this many blob characters are replaced (rule 6).
pub const BLOB_MIN_LEN: usize = 40;
/// Registered secrets shorter than this are ignored: replacing them would mangle ordinary text.
pub const MIN_SECRET_LEN: usize = 4;

static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Register a literal secret (token, passphrase, ...) that must never appear in a log file.
/// Safe to call from any thread, repeatedly, with the same value.
pub fn register_secret(secret: &str) {
    if secret.len() < MIN_SECRET_LEN {
        return;
    }
    let mut secrets = match SECRETS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if secrets.iter().any(|known| known == secret) {
        return;
    }
    secrets.push(secret.to_string());
    secrets.sort_by_key(|known| std::cmp::Reverse(known.len()));
}

/// Forget every registered secret (tests).
pub fn clear_secrets() {
    match SECRETS.lock() {
        Ok(mut guard) => guard.clear(),
        Err(poisoned) => poisoned.into_inner().clear(),
    }
}

fn registered() -> Vec<String> {
    match SECRETS.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Words that mark a key as secret-bearing (rule 1), matched per key token.
const KEY_WORDS_PREFIX: &[&str] = &[
    "password",
    "passphrase",
    "passwd",
    "secret",
    "token",
    "proof",
    "ticket",
    "pairing",
    "signature",
    "cookie",
    "credential",
    "authorization",
    "authenticat",
];
/// Short words that must match a whole token (or its plural).
const KEY_WORDS_EXACT: &[&str] = &["key", "code", "auth", "psk", "sig", "pwd"];
/// Compact-key substrings that are never secrets (rule 1 allow list).
const KEY_ALLOW: &[&str] = &["keyboard", "keyframe", "keycode", "keyid"];
/// URL parameter names (rule 2) beyond the rule 1 key test.
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

/// Split a key on `_ - .` and camelCase boundaries into lowercase tokens.
fn key_tokens(key: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for c in key.chars() {
        if matches!(c, '_' | '-' | '.' | ' ' | '/' | ':') {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            continue;
        }
        if c.is_uppercase() && previous_lower && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        previous_lower = c.is_lowercase() || c.is_ascii_digit();
        current.extend(c.to_lowercase());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Rule 1: does this key name a secret?
pub fn is_secret_key(key: &str) -> bool {
    let tokens = key_tokens(key);
    if tokens.is_empty() {
        return false;
    }
    let compact: String = tokens.concat();
    if compact.contains("apikey") || compact.contains("privatekey") {
        return true;
    }
    let mut i = 0;
    while i < tokens.len() {
        // Allow-listed pairs ("key_id", "key_code", "key_frame") are not secrets.
        if tokens[i] == "key" {
            if let Some(next) = tokens.get(i + 1) {
                let pair = format!("key{next}");
                if KEY_ALLOW.contains(&pair.as_str()) {
                    i += 2;
                    continue;
                }
            }
        }
        if token_is_secret_word(&tokens[i]) {
            return true;
        }
        i += 1;
    }
    false
}

fn token_is_secret_word(token: &str) -> bool {
    if KEY_ALLOW.contains(&token) {
        return false;
    }
    KEY_WORDS_PREFIX.iter().any(|word| token.starts_with(word))
        || KEY_WORDS_EXACT.iter().any(|word| {
            token == *word
                || (token.len() == word.len() + 1
                    && token.starts_with(word)
                    && token.ends_with('s'))
        })
        || (token.starts_with("auth") && token != "author" && token != "authors")
}

fn is_url_param(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    URL_PARAMS.contains(&lower.as_str()) || is_secret_key(name)
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

fn is_blob_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '_' | '=' | '-')
}

/// Rule 5: replace registered literal secrets, longest first.
fn apply_secrets(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if secret.len() >= MIN_SECRET_LEN && out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), REDACTED);
        }
    }
    out
}

/// Rule 6: runs of >= 40 blob characters.
fn apply_blobs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().count() >= BLOB_MIN_LEN {
            out.push_str(REDACTED_BLOB);
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if is_blob_char(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Rule 3: `Bearer <x>` and `Authorization: ...` header lines.
fn apply_headers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if lower[i..].starts_with("authorization:") && word_start(&lower, i) {
            let end = text[i..]
                .find(['\r', '\n'])
                .map(|n| i + n)
                .unwrap_or(text.len());
            out.push_str(&text[i..i + "authorization:".len()]);
            out.push(' ');
            out.push_str(REDACTED);
            i = end;
            continue;
        }
        if lower[i..].starts_with("bearer") && word_start(&lower, i) {
            let after = i + "bearer".len();
            let rest = &text[after..];
            let spaces = rest.len() - rest.trim_start_matches([' ', '\t']).len();
            if spaces > 0 {
                let value_start = after + spaces;
                let value_len = text[value_start..]
                    .find(|c: char| {
                        c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';' | ')' | '}' | ']')
                    })
                    .unwrap_or(text.len() - value_start);
                if value_len > 0 {
                    out.push_str(&text[i..after]);
                    out.push(' ');
                    out.push_str(REDACTED);
                    i = value_start + value_len;
                    continue;
                }
            }
        }
        let ch = text[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn word_start(text: &str, at: usize) -> bool {
    at == 0
        || !text[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric())
}

/// Rules 2 and 4: `key=value`, `key: value`, `"key":"value"` and URL params with a secret key.
fn apply_pairs(text: &str) -> String {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let (start, c) = chars[i];
        if !is_ident_char(c) {
            out.push(c);
            i += 1;
            continue;
        }
        // Identifier run.
        let mut j = i;
        while j < chars.len() && is_ident_char(chars[j].1) {
            j += 1;
        }
        let end = chars.get(j).map(|(b, _)| *b).unwrap_or(text.len());
        let ident = &text[start..end];
        out.push_str(ident);
        // Optional closing quote, spaces, then = or :
        let mut k = j;
        if k < chars.len() && matches!(chars[k].1, '"' | '\'') {
            k += 1;
        }
        let quote_len = k - j;
        let mut after_ident = k;
        while after_ident < chars.len() && matches!(chars[after_ident].1, ' ' | '\t') {
            after_ident += 1;
        }
        let sep = chars.get(after_ident).map(|(_, c)| *c);
        let is_sep = matches!(sep, Some('=') | Some(':'));
        let url_context =
            start > 0 && matches!(text.as_bytes()[start - 1], b'?' | b'&' | b'#' | b';');
        let secret = if url_context && sep == Some('=') {
            is_url_param(ident)
        } else {
            is_secret_key(ident)
        };
        if !(is_sep && secret) {
            i = j;
            continue;
        }
        // `key://` is a scheme, not a pair.
        let mut v = after_ident + 1;
        if sep == Some(':') && v + 1 < chars.len() && chars[v].1 == '/' && chars[v + 1].1 == '/' {
            i = j;
            continue;
        }
        // Emit the separator part verbatim, then the value is replaced.
        let sep_end = chars.get(v).map(|(b, _)| *b).unwrap_or(text.len());
        let sep_start = chars.get(j).map(|(b, _)| *b).unwrap_or(text.len());
        let _ = quote_len;
        out.push_str(&text[sep_start..sep_end]);
        while v < chars.len() && matches!(chars[v].1, ' ' | '\t') {
            out.push(chars[v].1);
            v += 1;
        }
        let value_quote = chars
            .get(v)
            .map(|(_, c)| *c)
            .filter(|c| matches!(c, '"' | '\''));
        let (value_begin, mut value_end);
        if let Some(q) = value_quote {
            out.push(q);
            value_begin = v + 1;
            value_end = value_begin;
            while value_end < chars.len() {
                let (_, ch) = chars[value_end];
                if ch == '\\' {
                    value_end += 2;
                    continue;
                }
                if ch == q {
                    break;
                }
                value_end += 1;
            }
            value_end = value_end.min(chars.len());
        } else {
            value_begin = v;
            value_end = v;
            while value_end < chars.len()
                && !chars[value_end].1.is_whitespace()
                && !matches!(
                    chars[value_end].1,
                    '&' | '#' | ',' | ';' | '"' | '\'' | ')' | '}' | ']'
                )
            {
                value_end += 1;
            }
        }
        if value_end > value_begin {
            out.push_str(REDACTED);
            i = value_end;
        } else {
            i = value_begin;
        }
    }
    out
}

/// Redact free text with the registered secrets (rules 2-6).
pub fn redact_text(text: &str) -> String {
    redact_text_with(text, &registered())
}

/// Like [`redact_text`] with an explicit secrets list (tests, ingestion with per-device secrets).
pub fn redact_text_with(text: &str, secrets: &[String]) -> String {
    let mut sorted: Vec<String> = secrets.to_vec();
    sorted.sort_by_key(|known| std::cmp::Reverse(known.len()));
    let step = apply_secrets(text, &sorted);
    let step = apply_headers(&step);
    let step = apply_pairs(&step);
    apply_blobs(&step)
}

/// Rules 2-5 only (no blob rule): for dedicated id fields.
fn redact_text_no_blob(text: &str, secrets: &[String]) -> String {
    let step = apply_secrets(text, secrets);
    let step = apply_headers(&step);
    apply_pairs(&step)
}

fn is_dedicated_key(key: &str) -> bool {
    matches!(
        key,
        "device" | "pc" | "session" | "device_id" | "pc_id" | "session_id"
    )
}

fn redact_inner(value: &mut Value, secrets: &[String], skip_blob: bool) {
    match value {
        Value::String(text) => {
            *text = if skip_blob {
                redact_text_no_blob(text, secrets)
            } else {
                redact_text_with(text, secrets)
            };
        }
        Value::Array(items) => {
            for item in items {
                redact_inner(item, secrets, skip_blob);
            }
        }
        Value::Object(map) => redact_object(map, secrets, skip_blob),
        _ => {}
    }
}

fn redact_object(map: &mut Map<String, Value>, secrets: &[String], skip_blob: bool) {
    for (key, child) in map.iter_mut() {
        if is_secret_key(key) {
            if !child.is_null() {
                *child = Value::String(REDACTED.to_string());
            }
            continue;
        }
        let skip = skip_blob || is_dedicated_key(key);
        redact_inner(child, secrets, skip);
    }
}

/// Redact every string in `value` in place: rule 1 on keys, rules 2-6 on strings.
/// The dedicated `device`, `pc` and `session` keys keep long ids (no blob rule).
pub fn redact_value(value: &mut Value) {
    redact_value_with(value, &registered());
}

pub fn redact_value_with(value: &mut Value, secrets: &[String]) {
    let mut sorted: Vec<String> = secrets.to_vec();
    sorted.sort_by_key(|known| std::cmp::Reverse(known.len()));
    if let Value::Object(map) = value {
        redact_object(map, &sorted, false);
    } else {
        redact_inner(value, &sorted, false);
    }
}

/// The replacement for a line whose redaction failed (rule 7).
pub fn failed_line(ts: &str) -> String {
    let line = serde_json::json!({
        "ts": ts,
        "level": "error",
        "msg": "[redaction failed]",
    });
    line.to_string()
}

/// Redact one JSONL line (or plain text, which is treated as free text) and return it
/// re-serialized. Never panics; on failure returns a `[redaction failed]` line without fields.
pub fn redact_line(line: &str) -> String {
    redact_line_with(line, &registered())
}

pub fn redact_line_with(line: &str, secrets: &[String]) -> String {
    let attempt = catch_unwind(AssertUnwindSafe(|| {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        match serde_json::from_str::<Value>(trimmed) {
            Ok(mut value @ Value::Object(_)) => {
                redact_value_with(&mut value, secrets);
                super::render_line(&value)
            }
            _ => redact_text_with(trimmed, secrets),
        }
    }));
    match attempt {
        Ok(text) => text,
        Err(_) => failed_line(&super::now_rfc3339()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(input: &str) -> String {
        redact_text_with(input, &[])
    }

    #[test]
    fn rule1_secret_keys_lose_the_whole_value() {
        let mut value = json!({
            "password": "hunter2",
            "Passphrase": {"nested": "thing"},
            "api_key": "abc",
            "apiKey": "abc",
            "authToken": "t",
            "pairing_code": "123456",
            "ticket": "x",
            "proof": 5,
            "cookie": "a=b",
            "Credentials": "c",
            "psk": "p",
            "signature": "s",
            "nonce_secret": "n",
            "auth": "a",
            "pairing.code": "1",
            "ok": "fine",
            "keyboard": "us",
            "keyframe": 3,
            "key_code": 65,
            "keycode": 65,
            "device_key_id": "dev-1",
            "keyId": "k1",
            "monitor": "primary",
            "author": "me",
        });
        redact_value_with(&mut value, &[]);
        for key in [
            "password",
            "Passphrase",
            "api_key",
            "apiKey",
            "authToken",
            "pairing_code",
            "ticket",
            "proof",
            "cookie",
            "Credentials",
            "psk",
            "signature",
            "nonce_secret",
            "auth",
            "pairing.code",
        ] {
            assert_eq!(value[key], json!("[redacted]"), "{key}");
        }
        assert_eq!(value["ok"], "fine");
        assert_eq!(value["keyboard"], "us");
        assert_eq!(value["keyframe"], 3);
        assert_eq!(value["keycode"], 65);
        assert_eq!(value["device_key_id"], "dev-1");
        assert_eq!(value["keyId"], "k1");
        assert_eq!(value["monitor"], "primary");
        assert_eq!(value["author"], "me");
    }

    #[test]
    fn rule1_recurses_into_nested_objects_and_arrays() {
        let mut value =
            json!({"fields": {"headers": [{"token": "abc"}], "inner": {"secret": "s"}}});
        redact_value_with(&mut value, &[]);
        assert_eq!(value["fields"]["headers"][0]["token"], "[redacted]");
        assert_eq!(value["fields"]["inner"]["secret"], "[redacted]");
    }

    #[test]
    fn rule2_url_params_are_replaced() {
        let out =
            text("GET wss://relay.example/ws?room=abc&token=SECRETVALUE&sig=zzz&x=1#key=frag");
        assert!(!out.contains("SECRETVALUE"), "{out}");
        assert!(!out.contains("zzz"), "{out}");
        assert!(!out.contains("frag"), "{out}");
        assert!(out.contains("room=abc"), "{out}");
        assert!(out.contains("x=1"), "{out}");
        let out = text("https://h/p?code=654321&ticket=abcd&proof=p1&pairing=9&passphrase=pw&secret=s&password=pp");
        for leaked in ["654321", "abcd", "p1", "pw", "pp"] {
            assert!(!out.contains(&format!("={leaked}")), "{out}");
        }
    }

    #[test]
    fn rule3_bearer_and_authorization_headers() {
        assert_eq!(
            text("sent Bearer abc.def.ghi ok"),
            "sent Bearer [redacted] ok"
        );
        assert_eq!(text("bearer   tok123"), "bearer [redacted]");
        let out = text("headers\nAuthorization: Basic dXNlcjpwYXNz\nHost: x");
        assert!(!out.contains("dXNlcjpwYXNz"), "{out}");
        assert!(out.contains("Authorization: [redacted]"), "{out}");
        assert!(out.contains("Host: x"), "{out}");
        // Known cost of the rule: prose after the word "bearer" is redacted too.
        assert_eq!(text("the bearer of news"), "the bearer [redacted] news");
    }

    #[test]
    fn rule4_pairs_in_free_text() {
        let out = text("login password=hunter2 user=bob");
        assert_eq!(out, "login password=[redacted] user=bob");
        assert_eq!(
            text("pairing code: 123456 for host"),
            "pairing code: [redacted] for host"
        );
        assert_eq!(
            text(r#"{"token":"abc123","n":1}"#),
            r#"{"token":"[redacted]","n":1}"#
        );
        assert_eq!(
            text(r#"{"passphrase" : "two words","n":1}"#),
            r#"{"passphrase" : "[redacted]","n":1}"#
        );
        assert_eq!(text("api_key = abcdef end"), "api_key = [redacted] end");
        assert_eq!(text("keyboard=us key=zzz"), "keyboard=us key=[redacted]");
        assert_eq!(
            text("ws read: connection reset"),
            "ws read: connection reset"
        );
        assert_eq!(text("at 12:34:56 done"), "at 12:34:56 done");
        assert_eq!(text("token://scheme"), "token://scheme");
    }

    #[test]
    fn rule5_registered_literals_are_replaced_longest_first() {
        let secrets = vec!["abcd".to_string(), "abcdefgh".to_string()];
        assert_eq!(
            redact_text_with("x abcdefgh y abcd z", &secrets),
            "x [redacted] y [redacted] z"
        );
        // Too-short secrets are never registered.
        register_secret("ab");
        assert!(!registered().iter().any(|s| s == "ab"));
    }

    #[test]
    fn rule6_long_opaque_runs_become_blobs() {
        let blob = "A".repeat(40);
        assert_eq!(text(&format!("k {blob} k")), "k [redacted-blob] k");
        let short = "A".repeat(39);
        assert_eq!(text(&format!("k {short} k")), format!("k {short} k"));
        let b64 = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5+/=";
        assert_eq!(text(&format!("v={b64}")), "[redacted-blob]");
        // Dedicated fields keep long ids; other fields do not.
        let id = "I".repeat(43);
        let mut value = json!({"device": id.clone(), "pc": id.clone(), "session": id.clone(),
                               "msg": id.clone(), "fields": {"x": id.clone()}});
        redact_value_with(&mut value, &[]);
        assert_eq!(value["device"], id.as_str());
        assert_eq!(value["pc"], id.as_str());
        assert_eq!(value["session"], id.as_str());
        assert_eq!(value["msg"], REDACTED_BLOB);
        assert_eq!(value["fields"]["x"], REDACTED_BLOB);
    }

    #[test]
    fn redact_line_handles_json_and_plain_text() {
        let line = r#"{"ts":"2026-11-02T13:45:12.345Z","level":"info","src":"host","msg":"hello token=abc","fields":{"password":"p"}}"#;
        let out = redact_line_with(line, &[]);
        assert!(!out.contains("abc"), "{out}");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["fields"]["password"], "[redacted]");
        assert_eq!(parsed["msg"], "hello token=[redacted]");
        assert!(out.starts_with(r#"{"ts":"#), "{out}");
        assert_eq!(
            redact_line_with("plain Bearer abc", &[]),
            "plain Bearer [redacted]"
        );
    }

    #[test]
    fn redaction_never_panics_on_odd_input() {
        for input in [
            "",
            "=",
            ":",
            "\"",
            "token=",
            "token:\"",
            "é=é",
            "key:   ",
            "\u{1F600} key=\u{1F600}",
            "a\\",
            "\"key\":\"\\",
        ] {
            let _ = text(input);
            let _ = redact_line_with(input, &[]);
        }
    }
}
