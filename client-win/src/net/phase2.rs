//! Device keys phase 2 on `/ws`, viewer side (docs/architecture/device-keys.md "Handshake" and
//! "Phase 2 relay protocol"): the `auth-hello` / `auth-host` / `auth-finish` handshake, the
//! `ctl` envelope that carries every endpoint message after it, pairing with a code
//! (`pair-proof`, comparison number, `pair-done` / `pair-denied`) and the WebRTC `auth-bind`.
//!
//! This module holds the message shapes and the order of the steps only. Every cryptographic
//! step is a call into `bp_device_key::phase2` (transcript hash, signatures, X25519 + HKDF
//! control keys, the sequenced AES-GCM sealer and opener, pairing code, admit hash, pair MACs,
//! comparison number); client-win has no crypto of its own.

use std::fmt;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bp_device_key::phase2::{
    self as p2, CtlKeys, CtlOpener, CtlSealer, EphemeralSecret, PairingCode, Sas, Side, Transcript,
};
use bp_device_key::{DeviceId, DeviceIdentity};
use rand::RngCore as _;
use serde_json::{json, Map, Value};

/// Mandatory at cutover in both transport modes (decision 9).
pub const CAP_CONTROL_AEAD: &str = "control-aead-v1";
/// Mandatory in WebRTC mode (decision 9).
pub const CAP_DTLS_BIND: &str = "dtls-bind-v1";
/// Sent in `auth-hello` while pairing (decision 3).
pub const CAP_PAIR: &str = "pair-v1";

/// How long the viewer waits for `auth-host` after `auth-hello` (decision 16).
pub const AUTH_HOST_TIMEOUT: Duration = Duration::from_secs(10);
/// The message shown when no `auth-host` arrives in time (decision 16).
pub const NO_DEVICE_KEYS: &str = "This PC does not support device keys or is not reachable";

/// The kind used for the binary input frames inside `ctl` (decision 15).
pub const INPUT_KIND: &str = "input";
/// The kind of the PC's binary cursor frames inside `ctl` (decision 20).
pub const CURSOR_KIND: &str = "cursor";
/// The longest viewer label in the viewer's `caps` (decision 19).
pub const MAX_NAME_CHARS: usize = 64;

/// `ctl` kinds whose plaintext is a raw binary frame, not JSON (decision 20).
fn binary_kind(kind: &str) -> bool {
    kind == INPUT_KIND || kind == CURSOR_KIND
}

/// What the viewer trusts about the PC it reaches.
#[derive(Debug, Clone)]
pub enum PcTrust {
    /// Normal session: the PC's key must equal the key pinned in `pcs.json` for its ID.
    Pinned(Vec<(DeviceId, [u8; 32])>),
    /// Pairing: no key is pinned yet; the PC must be the one named by `--pc`.
    Pairing(DeviceId),
}

/// Why a handshake, a pairing or a phase 2 session ended. All of them end the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase2Error {
    /// A message without the exact phase 2 shape.
    Malformed(&'static str),
    /// The PC is not the one asked for with `--pc`.
    WrongPc { expected: DeviceId, got: DeviceId },
    /// The PC is not in `pcs.json`.
    NotPaired(DeviceId),
    /// The PC's key differs from the pinned key.
    KeyChanged(DeviceId),
    /// The PC's caps lack a mandatory capability.
    MissingCap(&'static str),
    /// The PC refused the pairing.
    PairDenied,
    /// The PC sent a `ctl` `error` (decision 21) and is closing.
    PcRefused { code: String, message: String },
    /// A signature, MAC, key agreement or `ctl` message failed in `bp_device_key::phase2`.
    Crypto(String),
}

impl Phase2Error {
    fn crypto(what: &str, error: bp_device_key::DeviceKeyError) -> Self {
        Phase2Error::Crypto(format!("{what}: {error:?}"))
    }
}

impl fmt::Display for Phase2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Phase2Error::Malformed(what) => write!(f, "malformed {what}"),
            Phase2Error::WrongPc { expected, got } => {
                write!(f, "reached PC {got}, not the requested PC {expected}")
            }
            Phase2Error::NotPaired(pc) => write!(
                f,
                "unknown PC {pc}: this viewer is not paired with it; pair with --pair <code> or the pairing link"
            ),
            Phase2Error::KeyChanged(pc) => write!(
                f,
                "PC key changed: PC {pc} does not hold the key this viewer paired with"
            ),
            Phase2Error::MissingCap(cap) => {
                write!(
                    f,
                    "this PC does not offer {cap}, which this connection requires"
                )
            }
            Phase2Error::PairDenied => write!(f, "pairing was denied on the PC"),
            Phase2Error::PcRefused { code, message } => {
                let what = match code.as_str() {
                    "dtls-bind-required" => {
                        "the PC requires the WebRTC connection to be bound to device keys"
                    }
                    "cap-missing" => "the PC requires a capability this viewer did not offer",
                    _ => "the PC ended the session",
                };
                if message.is_empty() {
                    write!(f, "{what} ({code})")
                } else {
                    write!(f, "{what} ({code}): {message}")
                }
            }
            Phase2Error::Crypto(message) => write!(f, "device-key check failed: {message}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Handshake

/// The viewer side of one handshake, from `auth-hello` until `auth-finish`.
pub struct ViewerHandshake {
    nonce: [u8; 32],
    ephemeral: EphemeralSecret,
    trust: PcTrust,
    expected_pc: Option<DeviceId>,
    relay_origin: String,
    epoch: String,
}

/// A completed handshake.
pub struct Established {
    pub pc_id: DeviceId,
    pub pc_public: [u8; 32],
    pub h: [u8; 32],
    pub ctl: Ctl,
}

impl ViewerHandshake {
    /// A fresh nonce and ephemeral key, then [`ViewerHandshake::start_with`].
    pub fn start(
        viewer: &DeviceIdentity,
        trust: PcTrust,
        expected_pc: Option<DeviceId>,
        relay_origin: &str,
        epoch: &str,
        caps: &[&str],
    ) -> Result<(Self, String), Phase2Error> {
        let mut nonce = [0u8; 32];
        rand::rng().fill_bytes(&mut nonce);
        let ephemeral =
            EphemeralSecret::generate().map_err(|e| Phase2Error::crypto("ephemeral key", e))?;
        Ok(Self::start_with(
            viewer,
            nonce,
            ephemeral,
            trust,
            expected_pc,
            relay_origin,
            epoch,
            caps,
        ))
    }

    /// Builds the handshake and its `auth-hello` (`{type, id, public, nonce, ephemeral, caps}`).
    /// `epoch` is the `hello-ok` `mediaEpoch`, used byte for byte in the transcript.
    #[allow(clippy::too_many_arguments)]
    pub fn start_with(
        viewer: &DeviceIdentity,
        nonce: [u8; 32],
        ephemeral: EphemeralSecret,
        trust: PcTrust,
        expected_pc: Option<DeviceId>,
        relay_origin: &str,
        epoch: &str,
        caps: &[&str],
    ) -> (Self, String) {
        let hello = json!({
            "type": "auth-hello",
            "id": viewer.id().to_string(),
            "public": viewer.public_b64(),
            "nonce": b64(&nonce),
            "ephemeral": b64(&ephemeral.public_key()),
            "caps": caps,
        })
        .to_string();
        let expected_pc = match &trust {
            PcTrust::Pairing(pc) => Some(*pc),
            PcTrust::Pinned(_) => expected_pc,
        };
        (
            Self {
                nonce,
                ephemeral,
                trust,
                expected_pc,
                relay_origin: relay_origin.to_owned(),
                epoch: epoch.to_owned(),
            },
            hello,
        )
    }

    /// Checks `auth-host` (exact keys, ID matching its key, the PC asked for, the pinned key,
    /// the signature) and returns the session plus the `auth-finish` to send.
    pub fn on_auth_host(
        self,
        viewer: &DeviceIdentity,
        text: &str,
    ) -> Result<(Established, String), Phase2Error> {
        let host = exact_object(
            text,
            "auth-host",
            &["type", "id", "public", "nonce", "ephemeral", "signature"],
        )
        .ok_or(Phase2Error::Malformed("auth-host"))?;
        let field = |name: &str| host.get(name).and_then(Value::as_str);
        let pc_id = field("id")
            .and_then(|id| DeviceId::parse(id).ok())
            .filter(|id| field("id") == Some(id.to_string().as_str()))
            .ok_or(Phase2Error::Malformed("auth-host id"))?;
        let pc_public: [u8; 32] =
            decode_fixed(field("public")).ok_or(Phase2Error::Malformed("auth-host public"))?;
        let pc_nonce: [u8; 32] =
            decode_fixed(field("nonce")).ok_or(Phase2Error::Malformed("auth-host nonce"))?;
        let pc_ephemeral: [u8; 32] = decode_fixed(field("ephemeral"))
            .ok_or(Phase2Error::Malformed("auth-host ephemeral"))?;
        let signature: [u8; 64] = decode_fixed(field("signature"))
            .ok_or(Phase2Error::Malformed("auth-host signature"))?;
        if DeviceId::from_public_key(&pc_public) != pc_id {
            return Err(Phase2Error::Malformed("auth-host id for its public key"));
        }
        if let Some(expected) = self.expected_pc {
            if expected != pc_id {
                return Err(Phase2Error::WrongPc {
                    expected,
                    got: pc_id,
                });
            }
        }
        if let PcTrust::Pinned(paired) = &self.trust {
            match paired.iter().find(|(id, _)| *id == pc_id) {
                None => return Err(Phase2Error::NotPaired(pc_id)),
                Some((_, pinned)) if *pinned != pc_public => {
                    return Err(Phase2Error::KeyChanged(pc_id))
                }
                Some(_) => {}
            }
        }
        let h = Transcript {
            relay_origin: &self.relay_origin,
            epoch: &self.epoch,
            viewer_id: viewer.id(),
            viewer_public: viewer.public_key(),
            pc_id,
            pc_public,
            viewer_nonce: self.nonce,
            pc_nonce,
            viewer_ephemeral: self.ephemeral.public_key(),
            pc_ephemeral,
        }
        .hash();
        p2::verify_auth_host(&pc_public, &h, &signature)
            .map_err(|e| Phase2Error::crypto("auth-host signature", e))?;
        let finish = p2::sign_auth_finish(viewer, &h, &signature);
        let shared = self
            .ephemeral
            .diffie_hellman(&pc_ephemeral)
            .map_err(|e| Phase2Error::crypto("key agreement", e))?;
        let (sealer, opener) = CtlKeys::derive(&shared, &h).channel(Side::Viewer);
        Ok((
            Established {
                pc_id,
                pc_public,
                h,
                ctl: Ctl::new(sealer, opener),
            },
            json!({"type": "auth-finish", "signature": b64(&finish)}).to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// Control channel

/// The `ctl` channel of one handshake: the crate's sequenced sealer and opener plus the JSON
/// envelope `{"type":"ctl","kind","seq","data"}`.
pub struct Ctl {
    sealer: CtlSealer,
    opener: CtlOpener,
}

/// An opened `ctl` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// A JSON endpoint message whose `type` equals its kind.
    Text { kind: String, text: String },
    /// A raw binary frame: kind `input` or `cursor` (decision 20), not JSON.
    Binary { kind: String, frame: Vec<u8> },
}

impl Ctl {
    fn new(sealer: CtlSealer, opener: CtlOpener) -> Self {
        Self { sealer, opener }
    }

    /// Seals a JSON endpoint message; its `type` is the kind.
    pub fn seal_text(&mut self, text: &str) -> Result<String, Phase2Error> {
        let kind = serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
            .ok_or(Phase2Error::Malformed("endpoint message without a type"))?;
        self.seal(&kind, text.as_bytes())
    }

    /// Seals a binary input frame as kind `input`.
    pub fn seal_binary(&mut self, frame: &[u8]) -> Result<String, Phase2Error> {
        self.seal(INPUT_KIND, frame)
    }

    pub(crate) fn seal(&mut self, kind: &str, plaintext: &[u8]) -> Result<String, Phase2Error> {
        let (seq, data) = self
            .sealer
            .seal(kind, plaintext)
            .map_err(|e| Phase2Error::crypto("ctl seal", e))?;
        Ok(json!({"type": "ctl", "kind": kind, "seq": seq, "data": b64(&data)}).to_string())
    }

    /// Opens a `ctl` envelope. Any failure ends the session: a wrong shape, a gap or repeat in
    /// `seq`, failed authentication, or a JSON body whose `type` is not the kind.
    pub fn open(&mut self, value: &Value) -> Result<Opened, Phase2Error> {
        let object = value
            .as_object()
            .filter(|o| o.len() == 4 && o.get("type").and_then(Value::as_str) == Some("ctl"))
            .ok_or(Phase2Error::Malformed("ctl envelope"))?;
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .ok_or(Phase2Error::Malformed("ctl kind"))?;
        let seq = object
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or(Phase2Error::Malformed("ctl seq"))?;
        let data = object
            .get("data")
            .and_then(Value::as_str)
            .and_then(|data| URL_SAFE_NO_PAD.decode(data).ok())
            .ok_or(Phase2Error::Malformed("ctl data"))?;
        let plaintext = self
            .opener
            .open(kind, seq, &data)
            .map_err(|e| Phase2Error::crypto("ctl open", e))?;
        if binary_kind(kind) {
            return Ok(Opened::Binary {
                kind: kind.to_owned(),
                frame: plaintext,
            });
        }
        let text =
            String::from_utf8(plaintext).map_err(|_| Phase2Error::Malformed("ctl body (UTF-8)"))?;
        let inner_type = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned));
        if inner_type.as_deref() != Some(kind) {
            return Err(Phase2Error::Malformed("ctl body type (not the kind)"));
        }
        Ok(Opened::Text {
            kind: kind.to_owned(),
            text,
        })
    }
}

/// How a text message on `/ws` is treated once the handshake is done (decision 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// A `ctl` envelope: open it.
    Ctl,
    /// A relay message, relay keepalive, or an auth-* / pair-* type: handled as plaintext.
    Plain,
    /// A plaintext endpoint message: dropped.
    Drop,
}

pub fn route(type_name: Option<&str>) -> Route {
    match type_name {
        Some("ctl") => Route::Ctl,
        Some(
            "hello-ok" | "register-challenge" | "error" | "peer-left" | "peer-joined"
            | "pair-closed" | "ping" | "pong" | "auth-hello" | "auth-host" | "auth-finish"
            | "auth-bind" | "pair-proof" | "pair-done" | "pair-denied",
        ) => Route::Plain,
        _ => Route::Drop,
    }
}

/// The viewer's first `ctl` message (decision 19): its caps and a label, at most
/// [`MAX_NAME_CHARS`] characters (the computer name, cut to fit).
pub fn viewer_caps(caps: &[&str], name: &str) -> String {
    let name: String = name.trim().chars().take(MAX_NAME_CHARS).collect();
    json!({"type": "caps", "caps": caps, "name": name}).to_string()
}

/// This computer's name for [`viewer_caps`].
pub fn computer_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "Windows viewer".to_owned())
}

/// A `ctl` `error` from the PC (`{type:"error", code, message}`, decision 21) as the error that
/// ends the session; `None` for any other message.
pub fn pc_error(opened: &Opened) -> Option<Phase2Error> {
    let Opened::Text { kind, text } = opened else {
        return None;
    };
    if kind != "error" {
        return None;
    }
    let value: Value = serde_json::from_str(text).ok()?;
    let field = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Some(Phase2Error::PcRefused {
        code: field("code"),
        message: field("message"),
    })
}

/// The PC's first `ctl` message must be kind `caps` (`{"type":"caps","caps":[...]}`). Checks
/// the mandatory caps for the transport (`dtls-bind-v1` in WebRTC mode) and returns the list.
/// `control-aead-v1` is implied by a PC that answered `auth-host` (decision 16).
pub fn check_caps(opened: &Opened, webrtc: bool) -> Result<Vec<String>, Phase2Error> {
    if let Some(error) = pc_error(opened) {
        return Err(error);
    }
    let Opened::Text { kind, text } = opened else {
        return Err(Phase2Error::Malformed("first control message (not caps)"));
    };
    if kind != "caps" {
        return Err(Phase2Error::Malformed("first control message (not caps)"));
    }
    let caps: Vec<String> = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| {
            value.get("caps")?.as_array().map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
        })
        .ok_or(Phase2Error::Malformed("caps"))?;
    if webrtc && !caps.iter().any(|cap| cap == CAP_DTLS_BIND) {
        return Err(Phase2Error::MissingCap(CAP_DTLS_BIND));
    }
    Ok(caps)
}

// ---------------------------------------------------------------------------------------------
// Pairing

/// Parses a pairing code (normalised like device IDs, decision 1).
pub fn parse_code(input: &str) -> Result<PairingCode, String> {
    PairingCode::parse(input)
        .map_err(|_| "a pairing code has 10 characters from 0-9 and A-Z (XXXXX-XXXXX)".to_owned())
}

/// The `pair` field of the pairing `hello`: the relay admit hash for this PC and code.
pub fn admit_b64(code: &PairingCode, pc: &DeviceId) -> String {
    b64(&code.admit_hash(pc))
}

/// One step of the pairing exchange after `auth-finish`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairStep {
    /// Nothing to do (a relay keepalive or an unrelated message).
    Wait,
    /// Both MACs verified: show this number ("482 913") and "Confirm on the PC".
    ShowNumber(String),
    /// The PC confirmed: store it.
    Done,
}

/// The viewer side of pairing (decision 17): after `auth-finish` the viewer sends its
/// `pair-proof`, waits for the PC's, verifies it, only then shows the number, and waits for
/// `pair-done` or `pair-denied`.
pub struct Pairing {
    code: PairingCode,
    h: [u8; 32],
    pc_verified: bool,
}

impl Pairing {
    /// Returns the pairing state and the viewer's `pair-proof`.
    pub fn start(code: PairingCode, h: [u8; 32]) -> (Self, String) {
        let mac = code.pair_key().mac(Side::Viewer, &h);
        (
            Self {
                code,
                h,
                pc_verified: false,
            },
            json!({"type": "pair-proof", "mac": b64(&mac)}).to_string(),
        )
    }

    pub fn on_message(&mut self, text: &str) -> Result<PairStep, Phase2Error> {
        let value: Value =
            serde_json::from_str(text).map_err(|_| Phase2Error::Malformed("pairing message"))?;
        match value.get("type").and_then(Value::as_str) {
            Some("pair-proof") if !self.pc_verified => {
                let object = exact_object(text, "pair-proof", &["type", "mac"])
                    .ok_or(Phase2Error::Malformed("pair-proof"))?;
                let mac = object
                    .get("mac")
                    .and_then(Value::as_str)
                    .and_then(|mac| URL_SAFE_NO_PAD.decode(mac).ok())
                    .ok_or(Phase2Error::Malformed("pair-proof mac"))?;
                self.code
                    .pair_key()
                    .verify_mac(Side::Pc, &self.h, &mac)
                    .map_err(|e| Phase2Error::crypto("the PC's pair-proof", e))?;
                self.pc_verified = true;
                Ok(PairStep::ShowNumber(
                    Sas::from_transcript_hash(&self.h).display(),
                ))
            }
            Some("pair-proof") => Err(Phase2Error::Malformed("a second pair-proof")),
            Some("pair-done") if self.pc_verified => Ok(PairStep::Done),
            Some("pair-done") => Err(Phase2Error::Malformed("pair-done before the PC's proof")),
            Some("pair-denied") => Err(Phase2Error::PairDenied),
            _ => Ok(PairStep::Wait),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// DTLS bind (WebRTC only)

/// The `auth-bind` exchange of one WebRTC peer. Media and input wait for [`Binder::verified`].
pub struct Binder {
    h: [u8; 32],
    pc_public: [u8; 32],
    local: Option<String>,
    remote: Option<String>,
    pc_signature: Option<[u8; 64]>,
    sent: bool,
    verified: bool,
}

impl Binder {
    pub fn new(h: [u8; 32], pc_public: [u8; 32]) -> Self {
        Self {
            h,
            pc_public,
            local: None,
            remote: None,
            pc_signature: None,
            sent: false,
            verified: false,
        }
    }

    pub fn verified(&self) -> bool {
        self.verified
    }

    /// Our offer's fingerprint.
    pub fn local_sdp(&mut self, sdp: &str) -> Result<(), Phase2Error> {
        self.local = Some(sdp_fingerprint(sdp).ok_or(Phase2Error::Malformed("offer fingerprint"))?);
        Ok(())
    }

    /// The answer's fingerprint. Once both are known, returns our `auth-bind` to send.
    pub fn remote_sdp(
        &mut self,
        viewer: &DeviceIdentity,
        sdp: &str,
    ) -> Result<Option<String>, Phase2Error> {
        if self.remote.is_some() {
            return Ok(None);
        }
        self.remote =
            Some(sdp_fingerprint(sdp).ok_or(Phase2Error::Malformed("answer fingerprint"))?);
        let (Some(local), Some(remote)) = (&self.local, &self.remote) else {
            return Ok(None);
        };
        let signature = p2::sign_auth_bind(viewer, &self.h, local, remote)
            .map_err(|e| Phase2Error::crypto("auth-bind", e))?;
        self.sent = true;
        self.check()?;
        Ok(Some(
            json!({"type": "auth-bind", "signature": b64(&signature)}).to_string(),
        ))
    }

    /// The PC's `auth-bind`. Verified as soon as both fingerprints are known.
    pub fn on_pc_bind(&mut self, text: &str) -> Result<(), Phase2Error> {
        if self.pc_signature.is_some() {
            return Err(Phase2Error::Malformed("a second auth-bind"));
        }
        let object = exact_object(text, "auth-bind", &["type", "signature"])
            .ok_or(Phase2Error::Malformed("auth-bind"))?;
        self.pc_signature = Some(
            decode_fixed(object.get("signature").and_then(Value::as_str))
                .ok_or(Phase2Error::Malformed("auth-bind signature"))?,
        );
        self.check()
    }

    fn check(&mut self) -> Result<(), Phase2Error> {
        if let (true, Some(local), Some(remote), Some(signature)) =
            (self.sent, &self.local, &self.remote, &self.pc_signature)
        {
            // The PC signs in its own order: its fingerprint (our remote) first.
            p2::verify_auth_bind(&self.pc_public, &self.h, remote, local, signature)
                .map_err(|e| Phase2Error::crypto("the PC's auth-bind", e))?;
            self.verified = true;
        }
        Ok(())
    }
}

/// The value of the first `a=fingerprint:` line, e.g. `sha-256 AB:CD:...`, as written in the SDP.
pub fn sdp_fingerprint(sdp: &str) -> Option<String> {
    sdp.lines()
        .find_map(|line| line.trim_end().strip_prefix("a=fingerprint:"))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

// ---------------------------------------------------------------------------------------------
// Helpers

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn decode_fixed<const N: usize>(text: Option<&str>) -> Option<[u8; N]> {
    URL_SAFE_NO_PAD.decode(text?).ok()?.try_into().ok()
}

/// The object of `text` when its `type` is `kind` and it has exactly `keys`.
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

// ---------------------------------------------------------------------------------------------
// The PC side, for tests

/// The PC end of the handshake with the fixture PC key (`fixtures/device-keys-contract.json`
/// `handshake.pc`, a public, test-only key), for the unit tests and the fake relays. It uses
/// the real `bp_device_key::phase2` primitives.
#[cfg(test)]
pub(crate) mod test_pc {
    use super::*;

    pub const FIXTURE: &str = include_str!("../../../fixtures/device-keys-contract.json");

    pub fn fixture() -> Value {
        serde_json::from_str(FIXTURE).expect("fixture JSON")
    }

    fn hex32(text: &str) -> [u8; 32] {
        let bytes: Vec<u8> = (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
            .collect();
        bytes.try_into().expect("32 bytes")
    }

    pub fn fixture_hex32(path: &[&str]) -> [u8; 32] {
        let mut value = fixture();
        for key in path {
            value = value[*key].take();
        }
        hex32(value.as_str().expect("hex string"))
    }

    pub fn fixture_b64_32(path: &[&str]) -> [u8; 32] {
        let mut value = fixture();
        for key in path {
            value = value[*key].take();
        }
        decode_fixed(value.as_str()).expect("32 bytes of base64url")
    }

    /// The fixture PC: its ID and pinned public key.
    pub fn fixture_pc() -> (DeviceId, [u8; 32]) {
        let identity = pc_identity();
        (identity.id(), identity.public_key())
    }

    fn pc_identity() -> DeviceIdentity {
        DeviceIdentity::from_seed(&fixture_hex32(&["handshake", "pc", "seedHex"]))
    }

    /// The fixture viewer's handshake inputs (nonce and ephemeral secret).
    pub fn viewer_inputs() -> ([u8; 32], EphemeralSecret) {
        (
            fixture_b64_32(&["handshake", "viewer", "nonce"]),
            EphemeralSecret::from_bytes(fixture_hex32(&[
                "handshake",
                "viewer",
                "ephemeralPrivateHex",
            ])),
        )
    }

    /// What the PC learned from a valid `auth-hello`.
    pub struct PcSession {
        pub h: [u8; 32],
        pub viewer_public: [u8; 32],
        pub pc_signature: [u8; 64],
        pub ctl: Ctl,
    }

    /// Answers an `auth-hello` with `auth-host` (fixture PC nonce and ephemeral); `None` when the
    /// hello is malformed.
    pub fn answer(
        auth_hello: &str,
        relay_origin: &str,
        epoch: &str,
    ) -> Option<(String, PcSession)> {
        let hello = exact_object(
            auth_hello,
            "auth-hello",
            &["type", "id", "public", "nonce", "ephemeral", "caps"],
        )?;
        let field = |name: &str| hello.get(name).and_then(Value::as_str);
        let viewer_id = DeviceId::parse(field("id")?).ok()?;
        let viewer_public: [u8; 32] = decode_fixed(field("public"))?;
        let viewer_nonce: [u8; 32] = decode_fixed(field("nonce"))?;
        let viewer_ephemeral: [u8; 32] = decode_fixed(field("ephemeral"))?;
        let pc = pc_identity();
        let pc_nonce = fixture_b64_32(&["handshake", "pc", "nonce"]);
        let ephemeral =
            EphemeralSecret::from_bytes(fixture_hex32(&["handshake", "pc", "ephemeralPrivateHex"]));
        let pc_ephemeral = ephemeral.public_key();
        let h = Transcript {
            relay_origin,
            epoch,
            viewer_id,
            viewer_public,
            pc_id: pc.id(),
            pc_public: pc.public_key(),
            viewer_nonce,
            pc_nonce,
            viewer_ephemeral,
            pc_ephemeral,
        }
        .hash();
        let pc_signature = p2::sign_auth_host(&pc, &h);
        let (sealer, opener) =
            CtlKeys::derive(&ephemeral.diffie_hellman(&viewer_ephemeral).ok()?, &h)
                .channel(Side::Pc);
        let host = json!({
            "type": "auth-host",
            "id": pc.id().to_string(),
            "public": pc.public_b64(),
            "nonce": b64(&pc_nonce),
            "ephemeral": b64(&pc_ephemeral),
            "signature": b64(&pc_signature),
        })
        .to_string();
        Some((
            host,
            PcSession {
                h,
                viewer_public,
                pc_signature,
                ctl: Ctl::new(sealer, opener),
            },
        ))
    }

    impl PcSession {
        /// Checks the viewer's `auth-finish`.
        pub fn finish_ok(&self, text: &str) -> bool {
            exact_object(text, "auth-finish", &["type", "signature"])
                .and_then(|o| decode_fixed::<64>(o.get("signature").and_then(Value::as_str)))
                .is_some_and(|signature| {
                    p2::verify_auth_finish(
                        &self.viewer_public,
                        &self.h,
                        &self.pc_signature,
                        &signature,
                    )
                    .is_ok()
                })
        }

        /// The PC's `pair-proof` for `code`.
        pub fn pair_proof(&self, code: &PairingCode) -> String {
            let mac = code.pair_key().mac(Side::Pc, &self.h);
            json!({"type": "pair-proof", "mac": b64(&mac)}).to_string()
        }

        /// The PC's `auth-bind` over its own fingerprint order (its own first).
        pub fn bind(&self, local: &str, remote: &str) -> String {
            let signature = p2::sign_auth_bind(&pc_identity(), &self.h, local, remote)
                .expect("valid fingerprints");
            json!({"type": "auth-bind", "signature": b64(&signature)}).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_pc::*;
    use super::*;
    use crate::net::config::ViewerAuth;

    const ORIGIN: &str = "https://example.test";
    const EPOCH: &str = "9007199254740993";

    fn viewer() -> ViewerAuth {
        ViewerAuth::fixture()
    }

    fn pinned() -> PcTrust {
        PcTrust::Pinned(vec![fixture_pc()])
    }

    fn fixed_start(trust: PcTrust, caps: &[&str]) -> (ViewerHandshake, String) {
        let viewer = viewer();
        let (nonce, ephemeral) = viewer_inputs();
        ViewerHandshake::start_with(
            viewer.identity(),
            nonce,
            ephemeral,
            trust,
            viewer.pc(),
            ORIGIN,
            EPOCH,
            caps,
        )
    }

    fn message(path: &[&str]) -> Value {
        let mut value = fixture();
        for key in path {
            value = value[*key].take();
        }
        value
    }

    /// auth-hello / auth-host / auth-finish against the test PC.
    fn handshake(trust: PcTrust, caps: &[&str]) -> Result<(Established, PcSession), Phase2Error> {
        let viewer = viewer();
        let (shake, hello) =
            ViewerHandshake::start(viewer.identity(), trust, viewer.pc(), ORIGIN, EPOCH, caps)?;
        let (host, pc) = answer(&hello, ORIGIN, EPOCH).expect("valid auth-hello");
        let (established, finish) = shake.on_auth_host(viewer.identity(), &host)?;
        assert!(pc.finish_ok(&finish), "auth-finish verifies at the PC");
        Ok((established, pc))
    }

    #[test]
    fn the_handshake_reproduces_the_fixture_messages_and_keys() {
        let (shake, hello) = fixed_start(pinned(), &[CAP_CONTROL_AEAD, CAP_DTLS_BIND]);
        assert_eq!(
            serde_json::from_str::<Value>(&hello).unwrap(),
            message(&["handshake", "messages", "authHello"])
        );
        let host = message(&["handshake", "messages", "authHost"]).to_string();
        let (mut established, finish) = shake.on_auth_host(viewer().identity(), &host).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&finish).unwrap(),
            message(&["handshake", "messages", "authFinish"])
        );
        assert_eq!(
            established.h,
            fixture_hex32(&["handshake", "transcriptHashHex"])
        );
        // ctl vectors: the viewer seals viewer-to-pc and opens pc-to-viewer.
        let vectors = message(&["phase2", "ctl", "messages"]);
        for vector in vectors.as_array().unwrap() {
            let envelope = vector["message"].clone();
            if vector["direction"] == "viewer-to-pc" {
                let sealed = if vector["encoding"] == "binary" {
                    let hex = vector["plaintextHex"].as_str().unwrap();
                    let bytes: Vec<u8> = (0..hex.len())
                        .step_by(2)
                        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                        .collect();
                    established.ctl.seal_binary(&bytes).unwrap()
                } else {
                    established
                        .ctl
                        .seal_text(vector["plaintextUtf8"].as_str().unwrap())
                        .unwrap()
                };
                assert_eq!(serde_json::from_str::<Value>(&sealed).unwrap(), envelope);
            } else {
                let kind = vector["kind"].as_str().unwrap();
                let plaintext = vector["plaintextUtf8"].as_str().unwrap();
                // Decision 20: cursor is a binary kind; its plaintext comes back untouched.
                let expected = if binary_kind(kind) {
                    Opened::Binary {
                        kind: kind.into(),
                        frame: plaintext.as_bytes().to_vec(),
                    }
                } else {
                    Opened::Text {
                        kind: kind.into(),
                        text: plaintext.into(),
                    }
                };
                assert_eq!(established.ctl.open(&envelope).unwrap(), expected);
            }
        }
    }

    #[test]
    fn the_fixture_ctl_failures_end_the_channel() {
        let (shake, _) = fixed_start(pinned(), &[CAP_CONTROL_AEAD]);
        let host = message(&["handshake", "messages", "authHost"]).to_string();
        let viewer = viewer();
        // The viewer opens only pc-to-viewer; a viewer-to-pc message under the wrong key fails.
        let (mut established, _) = shake.on_auth_host(viewer.identity(), &host).unwrap();
        let failures = message(&["phase2", "ctl", "failures"]);
        let wrong_direction = failures
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == "wrong-direction-key")
            .unwrap();
        assert!(established.ctl.open(&wrong_direction["message"]).is_err());
        // Dead after the failure, even for a valid message.
        let valid = &message(&["phase2", "ctl", "messages"])[2]["message"];
        assert!(established.ctl.open(valid).is_err());
    }

    #[test]
    fn happy_path_round_trips_text_and_binary_input() {
        let (mut viewer_side, mut pc) = handshake(pinned(), &[CAP_CONTROL_AEAD]).unwrap();
        assert_eq!(viewer_side.pc_id, fixture_pc().0);
        assert_eq!(viewer_side.h, pc.h);
        let sealed = viewer_side
            .ctl
            .seal_text(r#"{"type":"offer","sdp":"x"}"#)
            .unwrap();
        assert!(!sealed.contains("sdp"), "the body is not in the clear");
        assert_eq!(
            pc.ctl
                .open(&serde_json::from_str(&sealed).unwrap())
                .unwrap(),
            Opened::Text {
                kind: "offer".into(),
                text: r#"{"type":"offer","sdp":"x"}"#.into()
            }
        );
        let input = viewer_side.ctl.seal_binary(&[0x0d, 0x02, 0x4b]).unwrap();
        assert!(input.contains(r#""seq":1"#));
        assert_eq!(
            pc.ctl.open(&serde_json::from_str(&input).unwrap()).unwrap(),
            Opened::Binary {
                kind: "input".into(),
                frame: vec![0x0d, 0x02, 0x4b]
            }
        );
        let caps = pc
            .ctl
            .seal_text(r#"{"type":"caps","caps":["control-aead-v1","dtls-bind-v1"]}"#)
            .unwrap();
        let opened = viewer_side
            .ctl
            .open(&serde_json::from_str(&caps).unwrap())
            .unwrap();
        assert_eq!(check_caps(&opened, true).unwrap().len(), 2);
    }

    #[test]
    fn a_pc_key_that_differs_from_the_pinned_key_is_refused() {
        let (id, _) = fixture_pc();
        let trust = PcTrust::Pinned(vec![(id, [7u8; 32])]);
        let error = handshake(trust, &[CAP_CONTROL_AEAD]).err().unwrap();
        assert_eq!(error, Phase2Error::KeyChanged(id));
        assert!(error.to_string().contains("PC key changed"));
    }

    #[test]
    fn an_unpaired_pc_is_refused_with_a_pointer_to_pair() {
        let error = handshake(PcTrust::Pinned(Vec::new()), &[CAP_CONTROL_AEAD])
            .err()
            .unwrap();
        assert_eq!(error, Phase2Error::NotPaired(fixture_pc().0));
        assert!(error.to_string().contains("--pair"));
    }

    #[test]
    fn an_auth_host_signed_for_another_epoch_is_refused() {
        let viewer = viewer();
        let (shake, hello) = fixed_start(pinned(), &[CAP_CONTROL_AEAD]);
        let (host, _) = answer(&hello, ORIGIN, "1").unwrap();
        assert!(matches!(
            shake.on_auth_host(viewer.identity(), &host),
            Err(Phase2Error::Crypto(_))
        ));
    }

    #[test]
    fn auth_host_with_extra_keys_is_malformed() {
        let viewer = viewer();
        let (shake, _) = fixed_start(pinned(), &[CAP_CONTROL_AEAD]);
        let mut host = message(&["handshake", "messages", "authHost"]);
        host["caps"] = json!(["control-aead-v1"]);
        assert!(matches!(
            shake.on_auth_host(viewer.identity(), &host.to_string()),
            Err(Phase2Error::Malformed(_))
        ));
    }

    #[test]
    fn a_seq_gap_or_repeat_ends_the_channel() {
        let (mut viewer_side, mut pc) = handshake(pinned(), &[CAP_CONTROL_AEAD]).unwrap();
        let first = pc.ctl.seal_text(r#"{"type":"caps","caps":[]}"#).unwrap();
        let second = pc
            .ctl
            .seal_text(r#"{"type":"desktop-state","state":"active"}"#)
            .unwrap();
        assert!(viewer_side
            .ctl
            .open(&serde_json::from_str(&second).unwrap())
            .is_err());
        assert!(
            viewer_side
                .ctl
                .open(&serde_json::from_str(&first).unwrap())
                .is_err(),
            "dead after a gap"
        );

        let (mut viewer_side, mut pc) = handshake(pinned(), &[CAP_CONTROL_AEAD]).unwrap();
        let first = pc.ctl.seal_text(r#"{"type":"caps","caps":[]}"#).unwrap();
        let first: Value = serde_json::from_str(&first).unwrap();
        assert!(viewer_side.ctl.open(&first).is_ok());
        assert!(viewer_side.ctl.open(&first).is_err(), "repeat");
    }

    #[test]
    fn a_body_whose_type_is_not_the_kind_is_refused() {
        let (mut viewer_side, mut pc) = handshake(pinned(), &[CAP_CONTROL_AEAD]).unwrap();
        // Sealed under kind "answer" (so the AAD matches) but the body says caps.
        let sealed = pc
            .ctl
            .seal("answer", br#"{"type":"caps","caps":[]}"#)
            .unwrap();
        assert!(viewer_side
            .ctl
            .open(&serde_json::from_str(&sealed).unwrap())
            .is_err());
    }

    #[test]
    fn plaintext_endpoint_messages_are_dropped_after_the_handshake() {
        for kind in [
            "answer",
            "ice",
            "desktop-state",
            "monitor-list",
            "input-capabilities",
            "caps",
            "first-frame",
            "clipboard",
        ] {
            assert_eq!(route(Some(kind)), Route::Drop, "{kind}");
        }
        assert_eq!(route(None), Route::Drop);
        assert_eq!(route(Some("ctl")), Route::Ctl);
        for kind in [
            "error",
            "peer-left",
            "pong",
            "pair-closed",
            "auth-bind",
            "pair-done",
        ] {
            assert_eq!(route(Some(kind)), Route::Plain, "{kind}");
        }
    }

    #[test]
    fn webrtc_needs_dtls_bind_in_the_first_caps() {
        let caps = |list: Value| Opened::Text {
            kind: "caps".into(),
            text: json!({"type": "caps", "caps": list}).to_string(),
        };
        assert_eq!(
            check_caps(&caps(json!(["control-aead-v1"])), true),
            Err(Phase2Error::MissingCap(CAP_DTLS_BIND))
        );
        assert!(check_caps(&caps(json!(["control-aead-v1"])), false).is_ok());
        assert!(check_caps(&caps(json!(["control-aead-v1", "dtls-bind-v1"])), true).is_ok());
        let not_caps = Opened::Text {
            kind: "desktop-state".into(),
            text: r#"{"type":"desktop-state"}"#.into(),
        };
        assert!(check_caps(&not_caps, false).is_err());
    }

    #[test]
    fn pairing_reproduces_the_fixture_admit_proofs_and_number() {
        let code = parse_code("7oq-ik9 mxrl").unwrap();
        assert_eq!(code.as_str(), "70Q1K9MXR1");
        let (pc_id, _) = fixture_pc();
        assert_eq!(
            admit_b64(&code, &pc_id),
            message(&["phase2", "pairing", "admit"]).as_str().unwrap()
        );
        let caps = [CAP_CONTROL_AEAD, CAP_DTLS_BIND, CAP_PAIR];
        let (shake, hello) = fixed_start(PcTrust::Pairing(pc_id), &caps);
        assert_eq!(
            serde_json::from_str::<Value>(&hello).unwrap(),
            message(&["phase2", "pairing", "messages", "authHello"])
        );
        let host = message(&["handshake", "messages", "authHost"]).to_string();
        let (established, _) = shake.on_auth_host(viewer().identity(), &host).unwrap();
        let (mut pairing, proof) = Pairing::start(code, established.h);
        assert_eq!(
            serde_json::from_str::<Value>(&proof).unwrap(),
            message(&["phase2", "pairing", "messages", "pairProofViewer"])
        );
        // Nothing is shown before the PC's proof; a pong changes nothing.
        assert_eq!(pairing.on_message(r#"{"type":"pong"}"#), Ok(PairStep::Wait));
        let pc_proof = message(&["phase2", "pairing", "messages", "pairProofPc"]).to_string();
        assert_eq!(
            pairing.on_message(&pc_proof),
            Ok(PairStep::ShowNumber("830 687".into()))
        );
        assert_eq!(
            pairing.on_message(r#"{"type":"pair-done"}"#),
            Ok(PairStep::Done)
        );
    }

    #[test]
    fn pair_done_before_the_pc_proof_shows_no_number_and_fails() {
        let (established, _) = handshake(
            PcTrust::Pairing(fixture_pc().0),
            &[CAP_CONTROL_AEAD, CAP_PAIR],
        )
        .unwrap();
        let (mut pairing, _) = Pairing::start(parse_code("70Q1K9MXR1").unwrap(), established.h);
        assert!(pairing.on_message(r#"{"type":"pair-done"}"#).is_err());
    }

    #[test]
    fn a_wrong_pc_mac_shows_no_number() {
        let (established, pc) = handshake(
            PcTrust::Pairing(fixture_pc().0),
            &[CAP_CONTROL_AEAD, CAP_PAIR],
        )
        .unwrap();
        let (mut pairing, _) = Pairing::start(parse_code("70Q1K9MXR1").unwrap(), established.h);
        let wrong = parse_code("70Q1K9MXR2").unwrap();
        assert!(matches!(
            pairing.on_message(&pc.pair_proof(&wrong)),
            Err(Phase2Error::Crypto(_))
        ));
    }

    #[test]
    fn pair_denied_ends_pairing() {
        let (established, pc) = handshake(
            PcTrust::Pairing(fixture_pc().0),
            &[CAP_CONTROL_AEAD, CAP_PAIR],
        )
        .unwrap();
        let code = parse_code("70Q1K9MXR1").unwrap();
        let proof = pc.pair_proof(&code);
        let (mut pairing, _) = Pairing::start(code, established.h);
        assert!(matches!(
            pairing.on_message(&proof),
            Ok(PairStep::ShowNumber(_))
        ));
        assert_eq!(
            pairing.on_message(r#"{"type":"pair-denied"}"#),
            Err(Phase2Error::PairDenied)
        );
    }

    #[test]
    fn pairing_refuses_a_pc_other_than_the_one_named() {
        let other = DeviceId::from_public_key(&[9u8; 32]);
        let viewer = viewer();
        let (shake, _) = fixed_start(PcTrust::Pairing(other), &[CAP_CONTROL_AEAD, CAP_PAIR]);
        let host = message(&["handshake", "messages", "authHost"]).to_string();
        assert!(matches!(
            shake.on_auth_host(viewer.identity(), &host),
            Err(Phase2Error::WrongPc { .. })
        ));
    }

    #[test]
    fn bind_reproduces_the_fixture_and_verifies_the_pc_only_with_both_fingerprints() {
        let viewer = viewer();
        let (shake, _) = fixed_start(pinned(), &[CAP_CONTROL_AEAD, CAP_DTLS_BIND]);
        let host = message(&["handshake", "messages", "authHost"]).to_string();
        let (established, _) = shake.on_auth_host(viewer.identity(), &host).unwrap();
        let local = message(&["handshake", "dtls", "viewer", "localFingerprint"]);
        let remote = message(&["handshake", "dtls", "viewer", "remoteFingerprint"]);
        let (local, remote) = (local.as_str().unwrap(), remote.as_str().unwrap());
        let mut binder = Binder::new(established.h, established.pc_public);
        binder
            .local_sdp(&format!("v=0\r\na=fingerprint:{local}\r\n"))
            .unwrap();
        // The PC's bind may arrive before the answer is processed.
        binder
            .on_pc_bind(&message(&["handshake", "messages", "authBindPc"]).to_string())
            .unwrap();
        assert!(!binder.verified());
        let ours = binder
            .remote_sdp(
                viewer.identity(),
                &format!("v=0\r\na=fingerprint:{remote}\r\n"),
            )
            .unwrap()
            .expect("our auth-bind");
        assert_eq!(
            serde_json::from_str::<Value>(&ours).unwrap(),
            message(&["handshake", "messages", "authBind"])
        );
        assert!(binder.verified());

        // A PC bind over another fingerprint fails and media stays gated.
        let (established, pc) = handshake(pinned(), &[CAP_CONTROL_AEAD, CAP_DTLS_BIND]).unwrap();
        let mut binder = Binder::new(established.h, established.pc_public);
        binder.local_sdp(&format!("a=fingerprint:{local}")).unwrap();
        binder
            .remote_sdp(viewer.identity(), &format!("a=fingerprint:{remote}"))
            .unwrap();
        assert!(binder.on_pc_bind(&pc.bind(remote, remote)).is_err());
        assert!(!binder.verified());
    }

    #[test]
    fn the_viewer_caps_carry_a_label_cut_to_64_characters() {
        let long = "Ä".repeat(70);
        let caps: Value =
            serde_json::from_str(&viewer_caps(&[CAP_CONTROL_AEAD, CAP_DTLS_BIND], &long)).unwrap();
        assert_eq!(caps["type"], "caps");
        assert_eq!(caps["caps"], json!(["control-aead-v1", "dtls-bind-v1"]));
        assert_eq!(
            caps["name"].as_str().unwrap().chars().count(),
            MAX_NAME_CHARS
        );
        let short: Value = serde_json::from_str(&viewer_caps(&[], " DESK-01 ")).unwrap();
        assert_eq!(short["name"], "DESK-01");
        assert!(!computer_name().is_empty());
    }

    #[test]
    fn binary_cursor_frames_skip_the_type_check() {
        let (mut viewer_side, mut pc) = handshake(pinned(), &[CAP_CONTROL_AEAD]).unwrap();
        // A raw old-style cursor frame: not JSON, no `type`.
        let frame = vec![0x07, 0x01, 0x00, 0xff];
        let sealed = pc.ctl.seal(CURSOR_KIND, &frame).unwrap();
        assert_eq!(
            viewer_side
                .ctl
                .open(&serde_json::from_str(&sealed).unwrap())
                .unwrap(),
            Opened::Binary {
                kind: "cursor".into(),
                frame
            }
        );
        // A non-binary kind still needs JSON whose type is the kind.
        let sealed = pc.ctl.seal("desktop-state", &[0x07, 0x01]).unwrap();
        assert!(viewer_side
            .ctl
            .open(&serde_json::from_str(&sealed).unwrap())
            .is_err());
    }

    #[test]
    fn a_pc_ctl_error_ends_the_session_with_a_clear_message() {
        let (mut viewer_side, mut pc) = handshake(pinned(), &[CAP_CONTROL_AEAD]).unwrap();
        let sealed = pc
            .ctl
            .seal_text(
                r#"{"type":"error","code":"dtls-bind-required","message":"bind the DTLS session"}"#,
            )
            .unwrap();
        let opened = viewer_side
            .ctl
            .open(&serde_json::from_str(&sealed).unwrap())
            .unwrap();
        let error = pc_error(&opened).expect("a PC error");
        assert_eq!(
            error,
            Phase2Error::PcRefused {
                code: "dtls-bind-required".into(),
                message: "bind the DTLS session".into()
            }
        );
        assert!(error.to_string().contains("bound to device keys"));
        // As the first message it fails the caps check with the same error.
        assert_eq!(check_caps(&opened, false), Err(error));
        let missing = Opened::Text {
            kind: "error".into(),
            text: r#"{"type":"error","code":"cap-missing","message":""}"#.into(),
        };
        assert!(pc_error(&missing)
            .unwrap()
            .to_string()
            .contains("capability"));
        assert!(pc_error(&Opened::Text {
            kind: "caps".into(),
            text: r#"{"type":"caps","caps":[]}"#.into()
        })
        .is_none());
    }

    #[test]
    fn the_pairing_hello_matches_the_fixture() {
        let code = parse_code("70Q1K-9MXR1").unwrap();
        let hello = crate::net::protocol::viewer_hello_pairing(
            viewer().identity(),
            &admit_b64(&code, &fixture_pc().0),
        );
        assert_eq!(
            serde_json::from_str::<Value>(&hello).unwrap(),
            message(&["phase2", "pairing", "messages", "viewerHello"])
        );
    }
}
