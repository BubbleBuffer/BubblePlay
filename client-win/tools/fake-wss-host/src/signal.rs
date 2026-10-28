//! Text messages on the signalling socket (`/ws`) and the validation the
//! relay applies to them for protocol 4 (`relay/protocol.go`
//! `validSignalMessage`). Media-socket control lives in `protocol.rs`.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::Value;

use crate::protocol::{is_valid_epoch, V4_MAX_CONTROL_BYTES};

/// Signalling frames are larger than media control (an SDP can be long), so
/// they get their own cap. Oversize frames are refused, not truncated.
pub const SIGNAL_MAX_BYTES: usize = 16 * 1024;

/// The keys a viewer's `/ws` hello may carry. Any other key closes the socket.
const VIEWER_HELLO_KEYS: [&str; 5] = ["type", "role", "id", "public", "pair"];

/// A viewer's `/ws` hello after its keys and types were checked. The proof
/// itself is checked later, against the challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerHello {
    /// The device ID as sent (the proof check requires the canonical form).
    pub id: String,
    /// The raw Ed25519 public key as sent, unpadded base64url.
    pub public: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// A `hello`. `role` is read here; `body` is the whole message, so the
    /// caller can check the keys its socket allows.
    Hello {
        role: String,
        body: Value,
    },
    Ping,
    MediaControl(MediaControl),
    FirstFrame {
        phase: String,
        request_id: String,
    },
    /// The browser's WebRTC offer (`{type:"offer", sdp}`), answered by the
    /// RTC answerer when `--webrtc` is on and refused otherwise.
    Offer {
        sdp: String,
    },
    /// A browser ICE candidate (`{type:"ice", candidate}`), the `candidate:`
    /// string with no `a=` prefix.
    Ice {
        candidate: String,
    },
    /// Any other type. The fake does not act on it; it is only counted.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaControl {
    pub action: String,
    pub protocol: Option<u64>,
    pub epoch: Option<String>,
    pub codec: Option<String>,
    pub reason: Option<String>,
    pub request_id: Option<String>,
    /// The integer `audio` field, if present and an integer. Only `1` opts in.
    pub audio: Option<u64>,
    /// `action: "monitor"`: the selector to switch to, as sent (not trimmed).
    pub monitor: Option<String>,
    /// `action: "mode"`: `work` or `game`, as sent. Other values are the caller's to refuse.
    pub stream_mode: Option<String>,
}

fn token_ok(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

fn optional_str(object: &Value, key: &str) -> Result<Option<String>, &'static str> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err("field is not a string"),
    }
}

pub fn parse_signal(text: &str) -> Result<Signal, &'static str> {
    if text.len() > SIGNAL_MAX_BYTES {
        return Err("signal too large");
    }
    let value: Value = serde_json::from_str(text).map_err(|_| "signal is not JSON")?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or("signal has no type")?;
    match kind {
        "hello" => {
            let role = value
                .get("role")
                .and_then(Value::as_str)
                .ok_or("hello has no role")?;
            Ok(Signal::Hello {
                role: role.to_owned(),
                body: value.clone(),
            })
        }
        "ping" => Ok(Signal::Ping),
        "media-control" => {
            let action = value
                .get("action")
                .and_then(Value::as_str)
                .ok_or("media-control has no action")?;
            if !token_ok(action) {
                return Err("media-control action is not a token");
            }
            let protocol = match value.get("protocol") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_u64().ok_or("protocol is not an integer")?),
            };
            let epoch = optional_str(&value, "epoch")?;
            if let Some(epoch) = &epoch {
                if !is_valid_epoch(epoch) {
                    return Err("epoch is not 32 lowercase hex");
                }
            }
            let reason = optional_str(&value, "reason")?;
            if let Some(reason) = &reason {
                if !token_ok(reason) {
                    return Err("reason is not a token");
                }
            }
            let request_id = optional_str(&value, "requestId")?;
            if let Some(request_id) = &request_id {
                if !token_ok(request_id) {
                    return Err("requestId is not a token");
                }
            }
            Ok(Signal::MediaControl(MediaControl {
                action: action.to_owned(),
                protocol,
                epoch,
                codec: optional_str(&value, "codec")?,
                reason,
                request_id,
                audio: value.get("audio").and_then(Value::as_u64),
                monitor: optional_str(&value, "monitor").ok().flatten(),
                stream_mode: optional_str(&value, "streamMode").ok().flatten(),
            }))
        }
        "first-frame" => {
            let phase = value
                .get("phase")
                .and_then(Value::as_str)
                .ok_or("first-frame has no phase")?;
            let request_id = value
                .get("requestId")
                .and_then(Value::as_str)
                .ok_or("first-frame has no requestId")?;
            if !token_ok(request_id) {
                return Err("requestId is not a token");
            }
            Ok(Signal::FirstFrame {
                phase: phase.to_owned(),
                request_id: request_id.to_owned(),
            })
        }
        "offer" => match value.get("sdp").and_then(Value::as_str) {
            Some(sdp) => Ok(Signal::Offer {
                sdp: sdp.to_owned(),
            }),
            None => Ok(Signal::Other("offer".into())),
        },
        "ice" => match value.get("candidate").and_then(Value::as_str) {
            Some(candidate) => Ok(Signal::Ice {
                candidate: candidate.to_owned(),
            }),
            None => Ok(Signal::Other("ice".into())),
        },
        other if token_ok(other) => Ok(Signal::Other(other.to_owned())),
        _ => Err("type is not a token"),
    }
}

/// Checks a viewer's `/ws` hello body: only the allowed keys, `id` and `public`
/// as strings, and `pair`, if present, as 32 bytes of base64url.
pub fn parse_viewer_hello(body: &Value) -> Result<ViewerHello, &'static str> {
    let object = body.as_object().ok_or("hello is not an object")?;
    if object
        .keys()
        .any(|key| !VIEWER_HELLO_KEYS.contains(&key.as_str()))
    {
        return Err("hello has an unknown key");
    }
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or("hello has no id")?;
    let public = body
        .get("public")
        .and_then(Value::as_str)
        .ok_or("hello has no public key")?;
    if let Some(pair) = object.get("pair") {
        let bytes = pair
            .as_str()
            .and_then(|text| URL_SAFE_NO_PAD.decode(text).ok())
            .ok_or("pair is not base64url")?;
        if bytes.len() != 32 {
            return Err("pair is not 32 bytes");
        }
    }
    Ok(ViewerHello {
        id: id.to_owned(),
        public: public.to_owned(),
    })
}

/// The media socket's text control has a tighter cap (`V4_MAX_CONTROL_BYTES`).
pub fn media_text_fits(text: &str) -> bool {
    text.len() <= V4_MAX_CONTROL_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn hello_roles_are_read_for_both_sockets() {
        let viewer = r#"{"type":"hello","role":"viewer","id":"x","public":"y"}"#;
        assert_eq!(
            parse_signal(viewer),
            Ok(Signal::Hello {
                role: "viewer".into(),
                body: serde_json::from_str(viewer).unwrap(),
            })
        );
        let media = r#"{"type":"hello","role":"media-viewer","ticket":"t"}"#;
        match parse_signal(media) {
            Ok(Signal::Hello { role, body }) => {
                assert_eq!(role, "media-viewer");
                assert_eq!(body["ticket"], "t");
            }
            other => panic!("expected a hello, got {other:?}"),
        }
        assert!(parse_signal(r#"{"type":"hello"}"#).is_err());
    }

    #[test]
    fn viewer_hello_takes_only_its_keys() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use serde_json::json;

        let plain = json!({"type": "hello", "role": "viewer", "id": "a", "public": "b"});
        assert_eq!(
            parse_viewer_hello(&plain),
            Ok(ViewerHello {
                id: "a".into(),
                public: "b".into()
            })
        );
        let pair = URL_SAFE_NO_PAD.encode([7u8; 32]);
        let with_pair =
            json!({"type": "hello", "role": "viewer", "id": "a", "public": "b", "pair": pair});
        assert!(parse_viewer_hello(&with_pair).is_ok());
        let short_pair =
            json!({"type": "hello", "role": "viewer", "id": "a", "public": "b", "pair": "AAAA"});
        assert!(parse_viewer_hello(&short_pair).is_err());
        let extra =
            json!({"type": "hello", "role": "viewer", "id": "a", "public": "b", "turn": true});
        assert!(parse_viewer_hello(&extra).is_err());
        let no_public = json!({"type": "hello", "role": "viewer", "id": "a"});
        assert!(parse_viewer_hello(&no_public).is_err());
    }

    #[test]
    fn media_control_start_carries_the_browser_fields() {
        let text = format!(
            r#"{{"type":"media-control","action":"start","protocol":4,"epoch":"{EPOCH}","codec":"hevc","streamMode":"desktop","reason":"client-start","quality":"high"}}"#
        );
        match parse_signal(&text).unwrap() {
            Signal::MediaControl(mc) => {
                assert_eq!(mc.action, "start");
                assert_eq!(mc.protocol, Some(4));
                assert_eq!(mc.epoch.as_deref(), Some(EPOCH));
                assert_eq!(mc.codec.as_deref(), Some("hevc"));
                assert_eq!(mc.reason.as_deref(), Some("client-start"));
            }
            other => panic!("expected media-control, got {other:?}"),
        }
    }

    #[test]
    fn malformed_epochs_and_reasons_are_refused() {
        let upper = EPOCH.to_uppercase();
        let short = &EPOCH[1..];
        for bad in [
            format!(r#"{{"type":"media-control","action":"start","epoch":"{upper}"}}"#),
            format!(r#"{{"type":"media-control","action":"start","epoch":"{short}"}}"#),
            r#"{"type":"media-control","action":"keyframe","reason":"has space"}"#.to_owned(),
            format!(
                r#"{{"type":"media-control","action":"keyframe","reason":"{}"}}"#,
                "x".repeat(65)
            ),
            r#"{"type":"media-control","action":"start","epoch":7}"#.to_owned(),
        ] {
            assert!(parse_signal(&bad).is_err(), "should refuse {bad}");
        }
    }

    #[test]
    fn audio_is_read_as_an_integer_and_only_the_opt_in_value_opts_in() {
        let read = |field: &str| {
            let text =
                format!(r#"{{"type":"media-control","action":"start","protocol":4{field}}}"#);
            match parse_signal(&text) {
                Ok(Signal::MediaControl(mc)) => mc.audio,
                other => panic!("expected media-control, got {other:?}"),
            }
        };
        assert_eq!(read(r#","audio":1"#), Some(1));
        assert_eq!(read(r#","audio":2"#), Some(2));
        assert_eq!(read(r#","audio":"1""#), None, "a string is not an opt-in");
        assert_eq!(read(""), None);
    }

    #[test]
    fn first_frame_and_unknown_types() {
        assert_eq!(
            parse_signal(
                r#"{"type":"first-frame","protocol":1,"phase":"request","requestId":"ff-1"}"#
            ),
            Ok(Signal::FirstFrame {
                phase: "request".into(),
                request_id: "ff-1".into()
            })
        );
        assert_eq!(
            parse_signal(r#"{"type":"monitor-list"}"#),
            Ok(Signal::Other("monitor-list".into()))
        );
        assert!(parse_signal(r#"{"type":"bad type"}"#).is_err());
        assert!(parse_signal("not json").is_err());
        assert!(parse_signal(&format!(
            r#"{{"type":"ping","pad":"{}"}}"#,
            "x".repeat(SIGNAL_MAX_BYTES)
        ))
        .is_err());
    }

    #[test]
    fn offer_and_ice_carry_their_strings() {
        assert_eq!(
            parse_signal(r#"{"type":"offer","sdp":"v=0\r\n"}"#),
            Ok(Signal::Offer {
                sdp: "v=0\r\n".into()
            })
        );
        assert_eq!(
            parse_signal(
                r#"{"type":"ice","candidate":"candidate:1 1 udp 2 127.0.0.1 9 typ host"}"#
            ),
            Ok(Signal::Ice {
                candidate: "candidate:1 1 udp 2 127.0.0.1 9 typ host".into()
            })
        );
        // A missing string is counted as an unknown type, not acted on.
        assert_eq!(
            parse_signal(r#"{"type":"offer"}"#),
            Ok(Signal::Other("offer".into()))
        );
        assert_eq!(
            parse_signal(r#"{"type":"ice","candidate":3}"#),
            Ok(Signal::Other("ice".into()))
        );
    }

    #[test]
    fn media_text_cap_is_the_v4_control_cap() {
        assert!(media_text_fits(&"x".repeat(V4_MAX_CONTROL_BYTES)));
        assert!(!media_text_fits(&"x".repeat(V4_MAX_CONTROL_BYTES + 1)));
    }
}
