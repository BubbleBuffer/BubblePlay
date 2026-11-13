//! The signalling allowlist (`validSignalMessage`) and the per-type field checks.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bp_device_key::DeviceId;
use serde_json::{Map, Value};

use super::fields::*;
use super::{SignalKind, is_keyframe_reason};
use crate::limits::{
    ADMIT_MAX, ADMIT_VIEWERS_MAX, AUTH_MAX, CTL_DATA_MAX, CTL_SEQ_MAX, MEDIA_CONTROL_MAX, PAIR_MAX,
    SIGNAL_LIMIT,
};

/// The /ws allowlist (decision 15): Some(kind) when the relay accepts the message
/// from this side, None when it must be dropped. Every other type, including the
/// pre-cutover types, is None and gets the unknown-type handling.
pub(super) fn valid_signal_message(
    m: &Map<String, Value>,
    host: bool,
    viewer: bool,
    length: usize,
) -> Option<SignalKind> {
    if length > SIGNAL_LIMIT {
        return None;
    }
    match text(m, "type") {
        "bye" => (m.len() == 1).then_some(SignalKind::Bye),
        // Phase 2 and pairing. Exact keys, a direction and a size cap for each.
        "admit" => (host && valid_admit(m, length)).then_some(SignalKind::Admit),
        "pair-open" => (host && exact(m, &["type", "admit"]) && b64url_is(m, "admit", 32))
            .then_some(SignalKind::PairOpen),
        "pair-close" => (host && exact(m, &["type"])).then_some(SignalKind::PairClose),
        "auth-hello" => {
            (viewer && length <= AUTH_MAX && valid_auth_hello(m)).then_some(SignalKind::AuthHello)
        }
        "auth-host" => {
            (host && length <= AUTH_MAX && valid_auth_host(m)).then_some(SignalKind::AuthHost)
        }
        "auth-finish" => (viewer
            && length <= PAIR_MAX
            && exact(m, &["type", "signature"])
            && b64url_is(m, "signature", 64))
        .then_some(SignalKind::AuthFinish),
        "auth-bind" => ((host || viewer)
            && length <= PAIR_MAX
            && exact(m, &["type", "signature"])
            && b64url_is(m, "signature", 64))
        .then_some(SignalKind::AuthBind),
        "pair-proof" => ((host || viewer)
            && length <= PAIR_MAX
            && exact(m, &["type", "mac"])
            && b64url_is(m, "mac", 32))
        .then_some(SignalKind::PairProof),
        "pair-done" => {
            (host && length <= PAIR_MAX && exact(m, &["type"])).then_some(SignalKind::PairDone)
        }
        "pair-denied" => {
            (host && length <= PAIR_MAX && exact(m, &["type"])).then_some(SignalKind::PairDenied)
        }
        "ctl" => ((host || viewer) && valid_ctl(m)).then_some(SignalKind::Ctl),
        _ => None,
    }
}

/// True when `text` is a canonical device ID: it parses, and it prints back to
/// the same characters (hyphenated, upper case).
pub(super) fn canonical_id(text: &str) -> bool {
    DeviceId::parse(text).is_ok_and(|id| id.to_string() == text)
}

/// The string at `key`, or None when it is missing or not a string.
pub(super) fn str_of<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    m.get(key).and_then(Value::as_str)
}

/// True when `key` is a canonical unpadded base64url string of exactly `len`
/// bytes (the same rule the relay applies to every key and signature).
pub(super) fn b64url_is(m: &Map<String, Value>, key: &str, len: usize) -> bool {
    let Some(text) = str_of(m, key) else {
        return false;
    };
    match URL_SAFE_NO_PAD.decode(text) {
        Ok(bytes) => bytes.len() == len && URL_SAFE_NO_PAD.encode(&bytes) == text,
        Err(_) => false,
    }
}

/// Go-style admit: exact keys, a boolean remotePairing, and at most
/// ADMIT_VIEWERS_MAX distinct canonical viewer IDs in a message of at most
/// ADMIT_MAX bytes.
pub(super) fn valid_admit(m: &Map<String, Value>, length: usize) -> bool {
    if length > ADMIT_MAX || !exact(m, &["type", "viewers", "remotePairing"]) {
        return false;
    }
    if !is_boolean(m, "remotePairing") {
        return false;
    }
    let Some(viewers) = m.get("viewers").and_then(Value::as_array) else {
        return false;
    };
    if viewers.len() > ADMIT_VIEWERS_MAX {
        return false;
    }
    let mut seen = std::collections::BTreeSet::new();
    viewers.iter().all(|value| {
        value
            .as_str()
            .is_some_and(|id| canonical_id(id) && seen.insert(id))
    })
}

/// auth-hello: the viewer's canonical ID, its public key, a nonce and an
/// ephemeral key (32 bytes each), and caps: at most 32 names of 1 to 64 characters
/// from [a-z0-9-].
pub(super) fn valid_auth_hello(m: &Map<String, Value>) -> bool {
    if !exact(m, &["type", "id", "public", "nonce", "ephemeral", "caps"]) {
        return false;
    }
    if !str_of(m, "id").is_some_and(canonical_id) {
        return false;
    }
    if !(b64url_is(m, "public", 32) && b64url_is(m, "nonce", 32) && b64url_is(m, "ephemeral", 32)) {
        return false;
    }
    let Some(caps) = m.get("caps").and_then(Value::as_array) else {
        return false;
    };
    caps.len() <= 32
        && caps.iter().all(|cap| {
            cap.as_str()
                .is_some_and(|name| (1..=64).contains(&name.len()) && is_cap_name(name))
        })
}

/// auth-host: the same fields as auth-hello with a 64-byte signature in place of caps.
pub(super) fn valid_auth_host(m: &Map<String, Value>) -> bool {
    exact(
        m,
        &["type", "id", "public", "nonce", "ephemeral", "signature"],
    ) && str_of(m, "id").is_some_and(canonical_id)
        && b64url_is(m, "public", 32)
        && b64url_is(m, "nonce", 32)
        && b64url_is(m, "ephemeral", 32)
        && b64url_is(m, "signature", 64)
}

/// A capability name: lower-case letters, digits and hyphens.
pub(super) fn is_cap_name(name: &str) -> bool {
    name.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// ctl: exact keys; kind is 1 to 32 characters from [a-z0-9-]; seq is an integer
/// from 0 to 2^53 - 1; data is unpadded base64url of at most CTL_DATA_MAX
/// characters. The relay does not track seq.
pub(super) fn valid_ctl(m: &Map<String, Value>) -> bool {
    if !exact(m, &["type", "kind", "seq", "data"]) {
        return false;
    }
    let kind_ok =
        str_of(m, "kind").is_some_and(|kind| (1..=32).contains(&kind.len()) && is_cap_name(kind));
    if !kind_ok || !integer(m, "seq", 0.0, CTL_SEQ_MAX) {
        return false;
    }
    match str_of(m, "data") {
        Some(data) => data.len() <= CTL_DATA_MAX && URL_SAFE_NO_PAD.decode(data).is_ok(),
        None => false,
    }
}

/// Go's validMediaControl: media-control text on a media socket.
pub(super) fn valid_media_control(m: &Map<String, Value>, length: usize) -> bool {
    if length > MEDIA_CONTROL_MAX {
        return false;
    }
    match text(m, "type") {
        "ack" => {
            only(m, &["type", "generation", "seq", "recvTsMs", "decodedSeq"])
                && uint32_value(m, "generation")
                && uint32_value(m, "seq")
                && number(m, "recvTsMs").is_some_and(|stamp| stamp >= 0.0)
                && (!has(m, "decodedSeq") || uint32_value(m, "decodedSeq"))
        }
        "keyframe" => exact(m, &["type", "reason"]) && is_keyframe_reason(text(m, "reason")),
        _ => false,
    }
}

/// Go's validDrop: the pointer a drag was released at.
pub(super) fn valid_drop(m: &Map<String, Value>) -> bool {
    exact(m, &["x", "y", "generation", "viewportRevision"])
        && integer(m, "x", 0.0, 65535.0)
        && integer(m, "y", 0.0, 65535.0)
        && integer(m, "generation", 1.0, 4_294_967_295.0)
        && uint32_value(m, "viewportRevision")
}

/// Go's validFileMeta: the metadata payload of an OFFER packet.
pub(super) fn valid_file_meta(m: &Map<String, Value>) -> bool {
    if !bounded_string(m, "name", 1, 1024)
        || !integer(m, "size", 0.0, 2.0 * 1024.0 * 1024.0 * 1024.0)
        || !bounded_string(m, "mime", 0, 255)
        || !integer(m, "mtimeMs", 0.0, 9_007_199_254_740_991.0)
        || !uint32_value(m, "batchId")
        || !integer(m, "batchCount", 1.0, 64.0)
    {
        return false;
    }
    let count = number(m, "batchCount").unwrap_or_default();
    if !integer(m, "batchIndex", 0.0, count - 1.0) {
        return false;
    }
    match text(m, "purpose") {
        "save" | "paste" => !has(m, "drop"),
        "drop" => object(m, "drop").is_some_and(valid_drop),
        _ => false,
    }
}
