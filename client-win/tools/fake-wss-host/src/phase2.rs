//! Device keys phase 2 on `/ws`, PC side, for the loopback fake: the fake plays the host.
//! This module holds the message shapes and the order of the steps for the
//! `auth-hello` / `auth-host` / `auth-finish` handshake, the `ctl` envelope that carries every
//! endpoint message after it, and the WebRTC `auth-bind`. The viewer side is
//! `client-win/src/net/phase2.rs`. Every cryptographic step is a call into
//! `bp_device_key::phase2`; this crate has no crypto of its own.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bp_device_key::phase2::{
    self as p2, CtlKeys, CtlOpener, CtlSealer, EphemeralSecret, Side, Transcript,
};
use bp_device_key::{DeviceId, DeviceIdentity};
use serde_json::{json, Map, Value};

/// The PC identity of the fake: the test-only key `handshake.pc` of
/// `fixtures/device-keys-contract.json`, ID 77VH-7M56-8GJK-Y12J. Never a real device key.
pub const PC_SEED_HEX: &str = "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb";

/// Capabilities: the viewer must offer `control-aead-v1`; the PC advertises both in its first
/// `ctl` message.
pub const CAP_CONTROL_AEAD: &str = "control-aead-v1";
pub const CAP_DTLS_BIND: &str = "dtls-bind-v1";
/// The `ctl` kind of binary input frames from the viewer.
pub const INPUT_KIND: &str = "input";
/// The `ctl` kind of the PC's cursor frames (binary raw bytes, or JSON).
pub const CURSOR_KIND: &str = "cursor";

/// The PC's identity, from [`PC_SEED_HEX`].
pub fn pc_identity() -> DeviceIdentity {
    DeviceIdentity::from_seed(&hex32(PC_SEED_HEX))
}

/// Whether a text message stays plaintext on `/ws` after the handshake: the relay's own types,
/// `ping` / `pong` and every `auth-*`. Everything else is an endpoint message and goes in `ctl`.
pub fn is_plain(text: &str) -> bool {
    let kind = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned));
    match kind.as_deref() {
        Some(
            "hello-ok" | "register-challenge" | "error" | "peer-left" | "peer-joined" | "ping"
            | "pong",
        ) => true,
        Some(kind) => kind.starts_with("auth-"),
        None => false,
    }
}

/// The value of the first `a=fingerprint:` line of an SDP, e.g. `sha-256 AB:CD:...`.
pub fn sdp_fingerprint(sdp: &str) -> Option<String> {
    sdp.lines()
        .find_map(|line| line.trim_end().strip_prefix("a=fingerprint:"))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The PC's side of one handshake, from `auth-host` until `auth-finish`.
pub struct Handshake {
    h: [u8; 32],
    viewer_public: [u8; 32],
    pc_signature: [u8; 64],
    keys: CtlKeys,
}

/// The `ctl` channel of an admitted viewer, from a verified `auth-finish` on.
pub struct Channel {
    h: [u8; 32],
    viewer_public: [u8; 32],
    sealer: CtlSealer,
    opener: CtlOpener,
}

/// An opened viewer `ctl` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// A JSON endpoint message whose `type` is its kind.
    Text { kind: String, text: String },
    /// A raw binary frame (kind `input`).
    Binary { kind: String, frame: Vec<u8> },
}

/// Answers an `auth-hello` from the viewer `viewer_id`, the one that passed `register-proof`.
/// `origin` is the relay origin and `epoch` the `mediaEpoch` of `hello-ok`; both go into the
/// transcript byte for byte. `nonce` and `ephemeral` are the PC's fresh values (`random32`,
/// `EphemeralSecret::generate`). Returns the `auth-host` text and the state to finish with.
pub fn answer(
    auth_hello: &str,
    viewer_id: &str,
    origin: &str,
    epoch: &str,
    nonce: [u8; 32],
    ephemeral: EphemeralSecret,
) -> Result<(String, Handshake), &'static str> {
    let hello = exact_object(
        auth_hello,
        "auth-hello",
        &["type", "id", "public", "nonce", "ephemeral", "caps"],
    )
    .ok_or("auth-hello has the wrong shape")?;
    let field = |name: &str| hello.get(name).and_then(Value::as_str);
    if field("id") != Some(viewer_id) {
        return Err("auth-hello names another viewer");
    }
    let caps = hello
        .get("caps")
        .and_then(Value::as_array)
        .ok_or("auth-hello caps are malformed")?;
    if !caps
        .iter()
        .any(|cap| cap.as_str() == Some(CAP_CONTROL_AEAD))
    {
        return Err("viewer does not offer control-aead-v1");
    }
    let viewer = DeviceId::parse(viewer_id).map_err(|_| "auth-hello id is malformed")?;
    let viewer_public: [u8; 32] =
        decode_fixed(field("public")).ok_or("auth-hello public key is malformed")?;
    let viewer_nonce: [u8; 32] =
        decode_fixed(field("nonce")).ok_or("auth-hello nonce is malformed")?;
    let viewer_ephemeral: [u8; 32] =
        decode_fixed(field("ephemeral")).ok_or("auth-hello ephemeral is malformed")?;
    if DeviceId::from_public_key(&viewer_public) != viewer {
        return Err("auth-hello id does not match its public key");
    }

    let pc = pc_identity();
    let pc_public = pc.public_key();
    let pc_ephemeral = ephemeral.public_key();
    let h = Transcript {
        relay_origin: origin,
        epoch,
        viewer_id: viewer,
        viewer_public,
        pc_id: pc.id(),
        pc_public,
        viewer_nonce,
        pc_nonce: nonce,
        viewer_ephemeral,
        pc_ephemeral,
    }
    .hash();
    let pc_signature = p2::sign_auth_host(&pc, &h);
    let shared = ephemeral
        .diffie_hellman(&viewer_ephemeral)
        .map_err(|_| "key agreement failed")?;
    let keys = CtlKeys::derive(&shared, &h);
    let host = json!({
        "type": "auth-host",
        "id": pc.id().to_string(),
        "public": b64(&pc_public),
        "nonce": b64(&nonce),
        "ephemeral": b64(&pc_ephemeral),
        "signature": b64(&pc_signature),
    })
    .to_string();
    Ok((
        host,
        Handshake {
            h,
            viewer_public,
            pc_signature,
            keys,
        },
    ))
}

impl Handshake {
    /// Checks the viewer's `auth-finish` and opens the channel. The caller closes the socket on
    /// an error.
    pub fn finish(self, text: &str) -> Result<Channel, &'static str> {
        let object = exact_object(text, "auth-finish", &["type", "signature"])
            .ok_or("auth-finish has the wrong shape")?;
        let signature: [u8; 64] = decode_fixed(object.get("signature").and_then(Value::as_str))
            .ok_or("auth-finish signature is malformed")?;
        p2::verify_auth_finish(&self.viewer_public, &self.h, &self.pc_signature, &signature)
            .map_err(|_| "auth-finish signature does not verify")?;
        let (sealer, opener) = self.keys.channel(Side::Pc);
        Ok(Channel {
            h: self.h,
            viewer_public: self.viewer_public,
            sealer,
            opener,
        })
    }

    /// The viewer's sealer and opener for these keys. Test use only: it plays the viewer side.
    #[cfg(test)]
    fn viewer_channel(&self) -> (CtlSealer, CtlOpener) {
        self.keys.channel(Side::Viewer)
    }
}

impl Channel {
    /// Seals a JSON endpoint message as a `ctl` envelope whose kind is its `type`.
    pub fn seal_text(&mut self, text: &str) -> Result<String, &'static str> {
        let kind = serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
            .ok_or("endpoint message has no type")?;
        self.seal(&kind, text.as_bytes())
    }

    /// Seals a raw binary frame under `kind` (the PC's binary cursor frames use `cursor`).
    pub fn seal_frame(&mut self, kind: &str, frame: &[u8]) -> Result<String, &'static str> {
        self.seal(kind, frame)
    }

    fn seal(&mut self, kind: &str, plaintext: &[u8]) -> Result<String, &'static str> {
        let (seq, data) = self
            .sealer
            .seal(kind, plaintext)
            .map_err(|_| "ctl seal failed")?;
        Ok(json!({"type": "ctl", "kind": kind, "seq": seq, "data": b64(&data)}).to_string())
    }

    /// Opens one viewer `ctl` envelope. A body that is a JSON object with `type` equal to its
    /// kind is a text message. Any other body is a binary frame, which only the `input` and
    /// `cursor` kinds may carry. Any failure is an error, and the caller ends the channel.
    pub fn open(&mut self, value: &Value) -> Result<Opened, &'static str> {
        let object = value
            .as_object()
            .filter(|o| o.len() == 4 && o.get("type").and_then(Value::as_str) == Some("ctl"))
            .ok_or("ctl envelope has the wrong shape")?;
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .ok_or("ctl kind is missing")?;
        let seq = object
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or("ctl seq is missing")?;
        let data = object
            .get("data")
            .and_then(Value::as_str)
            .and_then(|data| URL_SAFE_NO_PAD.decode(data).ok())
            .ok_or("ctl data is malformed")?;
        let plaintext = self
            .opener
            .open(kind, seq, &data)
            .map_err(|_| "ctl message does not open")?;
        if let Some(text) = json_of_kind(&plaintext, kind) {
            return Ok(Opened::Text {
                kind: kind.to_owned(),
                text,
            });
        }
        if kind == INPUT_KIND || kind == CURSOR_KIND {
            return Ok(Opened::Binary {
                kind: kind.to_owned(),
                frame: plaintext,
            });
        }
        Err("ctl body type is not its kind")
    }

    /// The PC's `auth-bind`. `local` is the PC's own fingerprint (the answer's `a=fingerprint`)
    /// and `remote` the viewer's (the offer's).
    pub fn bind(&self, local: &str, remote: &str) -> Result<String, &'static str> {
        let signature = p2::sign_auth_bind(&pc_identity(), &self.h, local, remote)
            .map_err(|_| "auth-bind fingerprints are malformed")?;
        Ok(json!({"type": "auth-bind", "signature": b64(&signature)}).to_string())
    }

    /// Verifies the viewer's `auth-bind`. The viewer signed with `local` = its own (the offer's)
    /// fingerprint and `remote` = the PC's (the answer's).
    pub fn verify_viewer_bind(
        &self,
        text: &str,
        local: &str,
        remote: &str,
    ) -> Result<(), &'static str> {
        let object = exact_object(text, "auth-bind", &["type", "signature"])
            .ok_or("auth-bind has the wrong shape")?;
        let signature: [u8; 64] = decode_fixed(object.get("signature").and_then(Value::as_str))
            .ok_or("auth-bind signature is malformed")?;
        p2::verify_auth_bind(&self.viewer_public, &self.h, local, remote, &signature)
            .map_err(|_| "auth-bind does not verify")
    }
}

/// The text of `plaintext` when it is a JSON object whose `type` is `kind`.
fn json_of_kind(plaintext: &[u8], kind: &str) -> Option<String> {
    let text = std::str::from_utf8(plaintext).ok()?;
    let value: Value = serde_json::from_str(text).ok()?;
    (value.get("type").and_then(Value::as_str) == Some(kind)).then(|| text.to_owned())
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn decode_fixed<const N: usize>(text: Option<&str>) -> Option<[u8; N]> {
    URL_SAFE_NO_PAD.decode(text?).ok()?.try_into().ok()
}

fn hex32(text: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * index..2 * index + 2], 16).expect("hex digits");
    }
    out
}

/// The object of `text` when its `type` is `kind` and it has exactly the keys `keys`.
fn exact_object(text: &str, kind: &str, keys: &[&str]) -> Option<Map<String, Value>> {
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    if object.get("type")?.as_str()? != kind
        || object.len() != keys.len()
        || !keys.iter().all(|key| object.contains_key(*key))
    {
        return None;
    }
    Some(object.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../../fixtures/device-keys-contract.json");
    /// `handshake.sessionEpoch`, the transcript's epoch string.
    const EPOCH: &str = "9007199254740993";

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).expect("fixture JSON")
    }

    fn at<'a>(value: &'a Value, path: &[&str]) -> &'a Value {
        path.iter().fold(value, |node, key| &node[*key])
    }

    fn text_at(value: &Value, path: &[&str]) -> String {
        at(value, path).as_str().expect("string").to_owned()
    }

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn hex_bytes(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// The PC nonce and ephemeral key of the fixture.
    fn pc_inputs(f: &Value) -> ([u8; 32], EphemeralSecret) {
        let nonce: [u8; 32] =
            decode_fixed(Some(&text_at(f, &["handshake", "pc", "nonce"]))).expect("pc nonce");
        let ephemeral = EphemeralSecret::from_bytes(hex32(&text_at(
            f,
            &["handshake", "pc", "ephemeralPrivateHex"],
        )));
        (nonce, ephemeral)
    }

    /// The fixture's `auth-hello`, answered as the PC.
    fn answer_fixture(f: &Value) -> Result<(String, Handshake), &'static str> {
        let hello = at(f, &["handshake", "messages", "authHello"]).to_string();
        let viewer_id = text_at(f, &["handshake", "messages", "authHello", "id"]);
        let (nonce, ephemeral) = pc_inputs(f);
        answer(
            &hello,
            &viewer_id,
            &text_at(f, &["handshake", "relayOrigin"]),
            EPOCH,
            nonce,
            ephemeral,
        )
    }

    /// The fixture handshake finished: the PC channel and the viewer's side of the same keys.
    fn fixture_channels(f: &Value) -> (Channel, (CtlSealer, CtlOpener)) {
        let (_, handshake) = answer_fixture(f).expect("the fixture auth-hello is answered");
        let viewer_side = handshake.viewer_channel();
        let channel = handshake
            .finish(&at(f, &["handshake", "messages", "authFinish"]).to_string())
            .expect("the fixture auth-finish verifies");
        (channel, viewer_side)
    }

    #[test]
    fn the_handshake_reproduces_the_fixture_auth_host_and_keys() {
        let f = fixture();
        let (host, handshake) = answer_fixture(&f).expect("answered");
        let host: Value = serde_json::from_str(&host).expect("JSON");
        assert_eq!(host, at(&f, &["handshake", "messages", "authHost"]).clone());
        assert_eq!(
            to_hex(&handshake.h),
            text_at(&f, &["handshake", "transcriptHashHex"])
        );
        assert_eq!(
            to_hex(handshake.keys.viewer_to_pc()),
            text_at(&f, &["handshake", "controlKeys", "viewerToPcHex"])
        );
        assert_eq!(
            to_hex(handshake.keys.pc_to_viewer()),
            text_at(&f, &["handshake", "controlKeys", "pcToViewerHex"])
        );
    }

    #[test]
    fn finish_accepts_the_fixture_and_refuses_a_bad_signature() {
        let f = fixture();
        let good = at(&f, &["handshake", "messages", "authFinish"]).clone();
        let mut bad = good.clone();
        let original = good["signature"].as_str().unwrap().to_owned();
        let first = if original.starts_with('A') { "B" } else { "A" };
        bad["signature"] = format!("{first}{}", &original[1..]).into();
        let (_, handshake) = answer_fixture(&f).expect("answered");
        assert!(handshake.finish(&bad.to_string()).is_err());
        let (_, handshake) = answer_fixture(&f).expect("answered");
        assert!(handshake.finish(&good.to_string()).is_ok());
    }

    #[test]
    fn the_fixture_ctl_messages_seal_and_open_exactly() {
        let f = fixture();
        let (mut pc, (mut viewer_sealer, mut viewer_opener)) = fixture_channels(&f);
        for entry in at(&f, &["phase2", "ctl", "messages"])
            .as_array()
            .expect("list")
        {
            let kind = entry["kind"].as_str().expect("kind");
            let seq = entry["seq"].as_u64().expect("seq");
            let plaintext = hex_bytes(entry["plaintextHex"].as_str().expect("plaintextHex"));
            if entry["direction"] == "pc-to-viewer" {
                let envelope = match entry["encoding"].as_str() {
                    Some("binary") => pc.seal_frame(kind, &plaintext),
                    _ => pc.seal_text(entry["plaintextUtf8"].as_str().expect("JSON text")),
                }
                .expect("sealed");
                let envelope: Value = serde_json::from_str(&envelope).expect("envelope");
                assert_eq!(envelope, entry["message"], "pc-to-viewer seq {seq}");
                let data = URL_SAFE_NO_PAD
                    .decode(entry["message"]["data"].as_str().unwrap())
                    .unwrap();
                assert_eq!(
                    viewer_opener.open(kind, seq, &data).expect("viewer opens"),
                    plaintext,
                    "viewer opens pc seq {seq}"
                );
            } else {
                let (sealed_seq, data) = viewer_sealer.seal(kind, &plaintext).expect("sealed");
                assert_eq!(sealed_seq, seq);
                let envelope = json!({
                    "type": "ctl",
                    "kind": kind,
                    "seq": sealed_seq,
                    "data": b64(&data),
                });
                assert_eq!(envelope, entry["message"], "viewer seq {seq} seals");
                let opened = pc
                    .open(&entry["message"])
                    .expect("pc opens the viewer message");
                let expected = match entry["encoding"].as_str() {
                    Some("binary") => Opened::Binary {
                        kind: kind.to_owned(),
                        frame: plaintext.clone(),
                    },
                    _ => Opened::Text {
                        kind: kind.to_owned(),
                        text: entry["plaintextUtf8"].as_str().unwrap().to_owned(),
                    },
                };
                assert_eq!(opened, expected, "viewer-to-pc seq {seq}");
            }
        }
    }

    #[test]
    fn the_fixture_ctl_failures_do_not_open() {
        let f = fixture();
        let messages = at(&f, &["phase2", "ctl", "messages"])
            .as_array()
            .expect("list");
        for failure in at(&f, &["phase2", "ctl", "failures"])
            .as_array()
            .expect("list")
        {
            let name = failure["name"].as_str().expect("name");
            let (mut pc, _) = fixture_channels(&f);
            if name == "replayed-seq" {
                // The first message opens, so the replay is the one that must fail.
                pc.open(&messages[0]["message"]).expect("seq 0 opens first");
            }
            assert!(
                pc.open(&failure["message"]).is_err(),
                "{name} must not open"
            );
        }
    }

    #[test]
    fn the_pc_bind_and_the_viewer_bind_match_the_fixture() {
        let f = fixture();
        let (channel, _) = fixture_channels(&f);
        let pc_local = text_at(&f, &["handshake", "dtls", "pc", "localFingerprint"]);
        let pc_remote = text_at(&f, &["handshake", "dtls", "pc", "remoteFingerprint"]);
        let bound: Value =
            serde_json::from_str(&channel.bind(&pc_local, &pc_remote).expect("bind")).unwrap();
        assert_eq!(
            bound,
            at(&f, &["handshake", "messages", "authBindPc"]).clone()
        );

        let viewer_local = text_at(&f, &["handshake", "dtls", "viewer", "localFingerprint"]);
        let viewer_remote = text_at(&f, &["handshake", "dtls", "viewer", "remoteFingerprint"]);
        let viewer_bind = at(&f, &["handshake", "messages", "authBind"]).to_string();
        channel
            .verify_viewer_bind(&viewer_bind, &viewer_local, &viewer_remote)
            .expect("the fixture viewer bind verifies");
        assert!(channel
            .verify_viewer_bind(&viewer_bind, &viewer_remote, &viewer_local)
            .is_err());
    }

    #[test]
    fn a_viewer_caps_message_opens_and_a_cursor_frame_opens_on_the_viewer() {
        let f = fixture();
        let (mut pc, (mut viewer_sealer, mut viewer_opener)) = fixture_channels(&f);
        let (seq, data) = viewer_sealer
            .seal("caps", br#"{"type":"caps","caps":["control-aead-v1"]}"#)
            .expect("sealed");
        let envelope = json!({"type": "ctl", "kind": "caps", "seq": seq, "data": b64(&data)});
        assert_eq!(
            pc.open(&envelope).expect("opens"),
            Opened::Text {
                kind: "caps".into(),
                text: r#"{"type":"caps","caps":["control-aead-v1"]}"#.into(),
            }
        );

        let frame = [0x01u8, 0x02, 0x03, 0xff];
        let sealed: Value =
            serde_json::from_str(&pc.seal_frame(CURSOR_KIND, &frame).expect("sealed")).unwrap();
        assert_eq!(sealed["kind"], "cursor");
        assert_eq!(sealed["seq"], 0);
        let data = URL_SAFE_NO_PAD
            .decode(sealed["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            viewer_opener
                .open("cursor", 0, &data)
                .expect("viewer opens"),
            frame.to_vec()
        );
    }

    #[test]
    fn an_auth_hello_for_another_viewer_or_with_extra_keys_or_no_control_cap_is_refused() {
        let f = fixture();
        let hello = at(&f, &["handshake", "messages", "authHello"]).clone();
        let origin = text_at(&f, &["handshake", "relayOrigin"]);
        let viewer_id = text_at(&f, &["handshake", "messages", "authHello", "id"]);
        let refused = |text: &str, id: &str| {
            let (nonce, ephemeral) = pc_inputs(&f);
            answer(text, id, &origin, EPOCH, nonce, ephemeral).is_err()
        };
        assert!(refused(&hello.to_string(), "77VH-7M56-8GJK-Y12J"));
        let mut extra = hello.clone();
        extra["extra"] = json!(1);
        assert!(refused(&extra.to_string(), &viewer_id));
        let mut no_cap = hello.clone();
        no_cap["caps"] = json!(["dtls-bind-v1"]);
        assert!(refused(&no_cap.to_string(), &viewer_id));
    }

    #[test]
    fn plaintext_types_are_the_relay_ones_and_the_auth_ones() {
        assert!(is_plain(r#"{"type":"hello-ok"}"#));
        assert!(is_plain(r#"{"type":"register-challenge","nonce":"x"}"#));
        assert!(is_plain(r#"{"type":"ping"}"#));
        assert!(is_plain(r#"{"type":"pong"}"#));
        assert!(is_plain(r#"{"type":"auth-bind","signature":"x"}"#));
        assert!(!is_plain(r#"{"type":"media-peer","present":true}"#));
        assert!(!is_plain(r#"{"type":"answer","sdp":"x"}"#));
        assert!(!is_plain("not json"));
    }

    #[test]
    fn the_pc_identity_is_the_fixture_pc() {
        let f = fixture();
        let pc = pc_identity();
        assert_eq!(pc.id().to_string(), text_at(&f, &["handshake", "pc", "id"]));
        assert_eq!(pc.public_b64(), text_at(&f, &["handshake", "pc", "public"]));
    }

    #[test]
    fn the_sdp_fingerprint_is_the_first_a_fingerprint_line() {
        let sdp = "v=0\r\na=setup:passive\r\na=fingerprint:sha-256 AB:CD\r\na=fingerprint:x\r\n";
        assert_eq!(sdp_fingerprint(sdp).as_deref(), Some("sha-256 AB:CD"));
        assert_eq!(sdp_fingerprint("v=0\r\n"), None);
    }
}
