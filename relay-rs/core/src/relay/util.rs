//! Small helpers: encodings, constant-time compare, Go-style JSON accessors and TURN.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use bp_device_key::DeviceId;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::RelayRng;

/// Unpadded base64url, the encoding of every key, nonce, signature and ticket.
pub(crate) fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes canonical unpadded base64url of exactly `len` bytes (Go's decodeKeyField).
pub(crate) fn decode_b64url(text: &str, len: usize) -> Option<Vec<u8>> {
    let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
    (bytes.len() == len && b64url(&bytes) == text).then_some(bytes)
}

/// Lower-case hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compare fixed-size SHA-256 digests, without a branch on secret length.
/// Hashing work depends on the input lengths; comparison work is always 32 bytes.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let a = Sha256::digest(a);
    let b = Sha256::digest(b);
    bool::from(a.ct_eq(&b))
}

/// `len` random bytes from the adapter's source.
pub(crate) fn random<const N: usize>(rng: &mut dyn RelayRng) -> [u8; N] {
    let mut buf = [0u8; N];
    rng.fill(&mut buf);
    buf
}

/// Canonical device id for a raw `pc` value, or None when it is not an id.
pub(crate) fn canonical_pc(raw: &str) -> Option<String> {
    DeviceId::parse(raw).ok().map(|id| id.to_string())
}

/// Go's str(): the value when it is a JSON string, else "".
pub(crate) fn text_of<'a>(m: &'a Map<String, Value>, key: &str) -> &'a str {
    m.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Go's boolean(): true only for a JSON true.
pub(crate) fn bool_of(m: &Map<String, Value>, key: &str) -> bool {
    m.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Parses a text frame as a JSON object. Anything else (including null) is None.
pub(crate) fn parse_object(text: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// Standard base64 with padding (TURN credentials).
pub(crate) fn b64_std(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Go's turnAuthority: `host` or `host:port`, port defaulting to 3478, IPv6
/// hosts bracketed. None for anything Go would refuse.
pub(crate) fn turn_authority(value: &str) -> Option<String> {
    if value.is_empty() || value.contains(['/', '?', '#', '@', ' ', '\t', '\r', '\n']) {
        return None;
    }
    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let tail = &rest[end + 1..];
        if tail.is_empty() {
            return None;
        }
        (host, tail.strip_prefix(':')?)
    } else if value.contains(':') {
        let (host, port) = value.rsplit_once(':')?;
        if host.contains(':') {
            return None;
        }
        (host, port)
    } else {
        (value, "3478")
    };
    let number: u32 = port.parse().ok()?;
    if host.is_empty() || !(1..=65535).contains(&number) {
        return None;
    }
    Some(if host.contains(':') {
        format!("[{host}]:{number}")
    } else {
        format!("{host}:{number}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_defaults_and_brackets() {
        assert_eq!(
            turn_authority("turn.example").as_deref(),
            Some("turn.example:3478")
        );
        assert_eq!(
            turn_authority("turn.example:5349").as_deref(),
            Some("turn.example:5349")
        );
        assert_eq!(
            turn_authority("[2001:db8::1]:3478").as_deref(),
            Some("[2001:db8::1]:3478")
        );
        assert_eq!(turn_authority("2001:db8::1"), None);
        assert_eq!(turn_authority("turn.example:0"), None);
        assert_eq!(turn_authority("turn.example/x"), None);
    }

    #[test]
    fn strict_base64url_length_and_form() {
        assert!(decode_b64url(&b64url(&[7u8; 32]), 32).is_some());
        assert!(decode_b64url(&b64url(&[7u8; 31]), 32).is_none());
        assert!(decode_b64url("AAAA=", 3).is_none());
    }
}
