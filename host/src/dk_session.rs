//! Phase 2 per-viewer session: the platform-independent state machine for one viewer's handshake,
//! pairing, ctl channel and WebRTC DTLS bind. No Win32, no clock, no storage, no sockets. All
//! crypto comes from `bp_device_key::phase2`.
//!
//! API for Signaling (one `Session` per PC; relay-generated messages never reach it):
//!
//! - `Session::new(SessionConfig, Box<dyn Randomness>)` at startup; `OsRandomness` in production.
//! - `parse_peer_joined(text)` on the relay's plaintext `peer-joined`. `mediaEpoch` is opaque ASCII
//!   (decision 22: the viewer's hello-ok epoch) and is kept byte for byte.
//! - `peer_joined(peer, trusted, pairing_code)` on every peer event; it resets the session.
//!   `trusted` is a snapshot of the trusted list; `pairing_code` is `Some` only while the pairing
//!   window is open. `auth-hello` before any peer event is an error that ends the attempt.
//! - `on_text(text)` for every plaintext text frame from the viewer on /ws (binary frames are
//!   dropped by the caller).
//! - `allow()`, `deny()`, `pair_saved(bool)` for the Devices decision; `stop(End)` when the
//!   pairing window closes or expires.
//! - `webrtc_offer(sdp)` when the viewer's offer arrives in ctl, `webrtc_answer(sdp)` with our
//!   answer, `dtls_connected()` once DTLS is up. `sdp_fingerprint(sdp)` extracts `a=fingerprint`.
//! - `seal_text(kind, json)` and `seal_binary("cursor", bytes)` return the ctl envelope text to
//!   send. `seal_text` requires the JSON `type` to equal `kind`.
//! - Gates and state: `is_established()`, `media_ready()` (input and media wait for both DTLS
//!   binds), `viewer()` (id, public key, caps), `is_ended()`.
//!
//! Results: `Ok(events)` carries `Event::Send` (text to write on /ws), state events, `Ignored`
//! (dropped, no state change; log at debug) and `Ended` (drop the viewer). A viewer that breaks the
//! protocol gets `Ended`, never `Err`. `Err(SessionError)` means API misuse or bad caller input
//! (nothing changed), or a failed primitive (the session is then ended).

use std::fmt;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bp_device_key::phase2::{
    check_fingerprint, sign_auth_bind, sign_auth_host, verify_auth_bind, verify_auth_finish,
    CtlKeys, CtlOpener, CtlSealer, EphemeralSecret, PairKey, PairingCode, Sas, Side, Transcript,
};
use bp_device_key::{DeviceId, DeviceIdentity, DeviceKeyError};
use rand_core::{OsRng, RngCore};
use serde_json::{json, Map, Value};

const CAP_CTL: &str = "control-aead-v1";
const CAP_DTLS: &str = "dtls-bind-v1";
const CAP_PAIR: &str = "pair-v1";
/// Longest accepted `mediaEpoch`. The live relay sends 32 lowercase hex characters.
const MAX_EPOCH_LEN: usize = 64;

/// The PC's per-handshake randomness: a fresh nonce and a fresh X25519 secret for every viewer
/// hello. Production uses [`OsRandomness`]; tests inject fixture values.
pub trait Randomness {
    fn nonce(&mut self) -> Result<[u8; 32], SessionError>;
    fn ephemeral(&mut self) -> Result<[u8; 32], SessionError>;
}

/// Operating-system randomness.
pub struct OsRandomness;

impl Randomness for OsRandomness {
    fn nonce(&mut self) -> Result<[u8; 32], SessionError> {
        os_bytes()
    }

    fn ephemeral(&mut self) -> Result<[u8; 32], SessionError> {
        os_bytes()
    }
}

fn os_bytes() -> Result<[u8; 32], SessionError> {
    let mut rng = OsRng;
    let mut out = [0u8; 32];
    rng.try_fill_bytes(&mut out)
        .map_err(|_| SessionError::Crypto(DeviceKeyError::Entropy))?;
    Ok(out)
}

/// Failures of the calls themselves. Viewer protocol failures are not errors; see [`End`].
#[derive(Debug)]
pub enum SessionError {
    /// The call is not allowed in the current state. Nothing changed.
    Misuse(&'static str),
    /// Caller-supplied data is malformed, for example an SDP without a fingerprint. Nothing changed.
    Input(&'static str),
    /// A primitive failed (entropy, sealing). The session is ended.
    Crypto(DeviceKeyError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Misuse(what) => write!(f, "device session misuse: {what}"),
            Self::Input(what) => write!(f, "device session input rejected: {what}"),
            Self::Crypto(error) => write!(f, "device session crypto failure: {error}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// A relay `peer-joined` event: the epoch exactly as the relay sent it, and whether a pairing
/// window applies to this viewer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerJoined {
    pub media_epoch: String,
    pub pairing: bool,
}

/// One trusted-list entry as the session needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trusted {
    pub id: DeviceId,
    pub public: [u8; 32],
}

/// The authenticated viewer: canonical ID, raw Ed25519 public key and the caps from its auth-hello.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Viewer {
    pub id: String,
    pub public: [u8; 32],
    pub caps: Vec<String>,
}

/// What the Devices window shows while pairing: the viewer and the six-digit comparison number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairRequest {
    pub viewer_id: String,
    pub sas_display: String,
}

/// Why a session ended. The caller drops the viewer's connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// The viewer sent a message the state does not allow, or malformed JSON.
    Protocol(&'static str),
    /// A signature or handshake value did not verify.
    Auth(&'static str),
    /// A ctl message failed: sequence, tag, kind or type mismatch.
    Ctl(&'static str),
    /// A WebRTC offer came from a viewer whose auth-hello lacked dtls-bind-v1 (ctl error sent first).
    DtlsBindRequired,
    /// The viewer's pair MAC did not match. No number was shown.
    PairCodeMismatch,
    /// The PC user denied the pairing.
    PairDenied,
    /// The PC user allowed the pairing but the trusted list could not be saved.
    PairSaveFailed,
    /// Pairing finished with pair-done.
    PairFinished,
    /// The caller stopped the session (window closed or expired, relay pair-closed).
    Stopped(&'static str),
    /// A new peer event replaced this session.
    PeerReset,
    /// An auth-hello was refused (mode, caps, trust or shape). Nothing was sent.
    Refused(&'static str),
}

/// What a call produced. `Send` text goes out on this viewer's /ws.
pub enum Event {
    /// Plaintext for auth-*, pair-* and auth-bind, or a ctl envelope.
    Send(String),
    /// The normal handshake finished: the ctl channel is open and `caps` was sent first.
    Established(Viewer),
    /// Pairing: show the viewer and the number; wait for `allow()` or `deny()`.
    PairRequest(PairRequest),
    /// Pairing allowed: add the trusted entry, then call `pair_saved`.
    PairAllowed(Viewer),
    /// An authenticated JSON endpoint message. `json` is the plaintext; its `type` equals `kind`.
    Message { kind: String, json: String },
    /// The exact plaintext bytes of a ctl `input` message (binary input frames; never parsed).
    Input(Vec<u8>),
    /// Both DTLS binds verified: media and input may flow.
    Bound,
    /// Dropped with no state change. The reason is static and safe to log.
    Ignored(&'static str),
    /// The session is over: drop this viewer.
    Ended(End),
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Send(text) => write!(f, "Send({} bytes)", text.len()),
            Self::Established(viewer) => write!(f, "Established({})", viewer.id),
            Self::PairRequest(request) => write!(f, "PairRequest({})", request.viewer_id),
            Self::PairAllowed(viewer) => write!(f, "PairAllowed({})", viewer.id),
            Self::Message { kind, json } => write!(f, "Message({kind}, {} bytes)", json.len()),
            Self::Input(bytes) => write!(f, "Input({} bytes)", bytes.len()),
            Self::Bound => f.write_str("Bound"),
            Self::Ignored(reason) => write!(f, "Ignored({reason})"),
            Self::Ended(end) => write!(f, "Ended({end:?})"),
        }
    }
}

/// The PC side of the session: its identity, the relay origin the transcript binds and the caps
/// it sends in its first ctl.
pub struct SessionConfig {
    pub identity: DeviceIdentity,
    /// `bp_device_key::relay_origin` of the worker URL.
    pub relay_origin: String,
    /// Sent as the first ctl (`caps`). Must contain `control-aead-v1`.
    pub pc_caps: Vec<String>,
}

/// One viewer's session. Signaling owns one per PC and feeds it every peer event and viewer frame.
pub struct Session {
    config: SessionConfig,
    random: Box<dyn Randomness>,
    state: State,
}

enum State {
    /// No peer event has arrived yet.
    Idle,
    /// A peer joined; waiting for the viewer's auth-hello.
    AwaitHello(Peer),
    /// auth-host sent; waiting for auth-finish.
    AwaitFinish(Box<Hand>),
    /// Pairing: auth-finish verified; waiting for the viewer's pair-proof.
    AwaitViewerProof(Box<Hand>),
    /// Pairing: both MACs verified and sent; waiting for the PC user.
    AwaitDecision(Viewer),
    /// Pairing: allowed; waiting for the caller to save the trusted entry.
    AwaitSave(Viewer),
    /// Normal handshake done: the ctl channel is open.
    Established(Box<Channel>),
    Ended,
}

struct Peer {
    epoch: String,
    pairing: bool,
    trusted: Vec<Trusted>,
    pair: Option<PairKey>,
}

struct Hand {
    viewer: Viewer,
    viewer_ephemeral: [u8; 32],
    pc_ephemeral: Option<EphemeralSecret>,
    h: [u8; 32],
    pc_signature: [u8; 64],
    pair: Option<PairKey>,
}

struct Channel {
    viewer: Viewer,
    h: [u8; 32],
    sealer: CtlSealer,
    opener: CtlOpener,
    offer_fp: Option<String>,
    answer_fp: Option<String>,
    /// Our auth-bind signature, once signed. Repeated `dtls_connected` calls re-send it.
    bind_signature: Option<[u8; 64]>,
    viewer_bound: bool,
}

impl Session {
    pub fn new(config: SessionConfig, random: Box<dyn Randomness>) -> Result<Self, SessionError> {
        if !config.pc_caps.iter().any(|cap| cap == CAP_CTL) {
            return Err(SessionError::Input("pc caps must contain control-aead-v1"));
        }
        Ok(Self {
            config,
            random,
            state: State::Idle,
        })
    }

    /// Starts a session for a peer event. Replaces any earlier session.
    pub fn peer_joined(
        &mut self,
        peer: PeerJoined,
        trusted: Vec<Trusted>,
        pairing_code: Option<PairingCode>,
    ) -> Result<Vec<Event>, SessionError> {
        check_epoch(&peer.media_epoch)?;
        let mut events = Vec::new();
        if !matches!(self.state, State::Idle | State::Ended) {
            events.push(Event::Ended(End::PeerReset));
        }
        let pair = match (peer.pairing, pairing_code) {
            (true, Some(code)) => Some(code.pair_key()),
            _ => None,
        };
        self.state = State::AwaitHello(Peer {
            epoch: peer.media_epoch,
            pairing: peer.pairing,
            trusted,
            pair,
        });
        Ok(events)
    }

    /// Handles one plaintext text frame from the viewer. Never pass relay messages here.
    pub fn on_text(&mut self, text: &str) -> Result<Vec<Event>, SessionError> {
        let state = std::mem::replace(&mut self.state, State::Ended);
        let (next, events) = self.step(state, text)?;
        self.state = next;
        Ok(events)
    }

    fn step(&mut self, state: State, text: &str) -> Result<(State, Vec<Event>), SessionError> {
        match state {
            State::Idle => match message_type(text).as_deref() {
                Some("auth-hello") => fail(End::Protocol("auth-hello before peer-joined")),
                _ => Ok((State::Idle, vec![Event::Ignored("no peer joined")])),
            },
            State::AwaitHello(peer) => self.hello(peer, text),
            State::AwaitFinish(hand) => self.finish(hand, text),
            State::AwaitViewerProof(hand) => self.viewer_proof(hand, text),
            State::AwaitDecision(viewer) => Ok((
                State::AwaitDecision(viewer),
                vec![Event::Ignored("waiting for the PC decision")],
            )),
            State::AwaitSave(viewer) => Ok((
                State::AwaitSave(viewer),
                vec![Event::Ignored("waiting for the pairing save")],
            )),
            State::Established(channel) => established(channel, text),
            State::Ended => Ok((State::Ended, vec![Event::Ignored("session ended")])),
        }
    }

    fn hello(&mut self, peer: Peer, text: &str) -> Result<(State, Vec<Event>), SessionError> {
        if message_type(text).as_deref() != Some("auth-hello") {
            return Ok((
                State::AwaitHello(peer),
                vec![Event::Ignored("not an auth-hello")],
            ));
        }
        // A refused auth-hello ends the attempt. Nothing goes on the wire, and the viewer gets no
        // retry until a new peer-joined.
        let hello = match parse_hello(text, &peer) {
            Ok(hello) => hello,
            Err(reason) => return fail(End::Refused(reason)),
        };
        let pc_nonce = self.random.nonce()?;
        let pc_secret = EphemeralSecret::from_bytes(self.random.ephemeral()?);
        let pc_ephemeral = pc_secret.public_key();
        let pc_id = self.config.identity.id();
        let pc_public = self.config.identity.public_key();
        let h = Transcript {
            relay_origin: &self.config.relay_origin,
            epoch: &peer.epoch,
            viewer_id: hello.id,
            viewer_public: hello.viewer.public,
            pc_id,
            pc_public,
            viewer_nonce: hello.nonce,
            pc_nonce,
            viewer_ephemeral: hello.ephemeral,
            pc_ephemeral,
        }
        .hash();
        let signature = sign_auth_host(&self.config.identity, &h);
        let auth_host = json!({
            "type": "auth-host",
            "id": pc_id.to_string(),
            "public": b64(&pc_public),
            "nonce": b64(&pc_nonce),
            "ephemeral": b64(&pc_ephemeral),
            "signature": b64(&signature),
        })
        .to_string();
        let hand = Hand {
            viewer: hello.viewer,
            viewer_ephemeral: hello.ephemeral,
            pc_ephemeral: Some(pc_secret),
            h,
            pc_signature: signature,
            pair: peer.pair,
        };
        Ok((
            State::AwaitFinish(Box::new(hand)),
            vec![Event::Send(auth_host)],
        ))
    }

    fn finish(
        &mut self,
        mut hand: Box<Hand>,
        text: &str,
    ) -> Result<(State, Vec<Event>), SessionError> {
        // Only a message typed auth-finish is verified. Any other message is an endpoint message
        // that arrived early: dropped, and the handshake keeps waiting for the real auth-finish.
        let object = match parse_object(text) {
            Ok(object) if object.get("type").and_then(Value::as_str) == Some("auth-finish") => {
                object
            }
            _ => {
                return Ok((
                    State::AwaitFinish(hand),
                    vec![Event::Ignored("not auth-finish")],
                ))
            }
        };
        let Ok(signature) = fixed_field::<64>(&object, "signature") else {
            return fail(End::Auth("malformed auth-finish"));
        };
        if verify_auth_finish(&hand.viewer.public, &hand.h, &hand.pc_signature, &signature).is_err()
        {
            return fail(End::Auth("auth-finish signature"));
        }
        if hand.pair.is_some() {
            return Ok((State::AwaitViewerProof(hand), Vec::new()));
        }
        let Some(secret) = hand.pc_ephemeral.take() else {
            return fail(End::Protocol("handshake ephemeral missing"));
        };
        let Ok(shared) = secret.diffie_hellman(&hand.viewer_ephemeral) else {
            return fail(End::Auth("weak ephemeral key"));
        };
        let (sealer, opener) = CtlKeys::derive(&shared, &hand.h).channel(Side::Pc);
        let mut channel = Channel {
            viewer: hand.viewer,
            h: hand.h,
            sealer,
            opener,
            offer_fp: None,
            answer_fp: None,
            bind_signature: None,
            viewer_bound: false,
        };
        let caps = json!({"type": "caps", "caps": self.config.pc_caps.clone()});
        let envelope = channel.seal("caps", caps.to_string().as_bytes())?;
        let viewer = channel.viewer.clone();
        Ok((
            State::Established(Box::new(channel)),
            vec![Event::Send(envelope), Event::Established(viewer)],
        ))
    }

    fn viewer_proof(
        &mut self,
        hand: Box<Hand>,
        text: &str,
    ) -> Result<(State, Vec<Event>), SessionError> {
        // As for auth-finish: only a message typed pair-proof is verified; others are dropped.
        let object = match parse_object(text) {
            Ok(object) if object.get("type").and_then(Value::as_str) == Some("pair-proof") => {
                object
            }
            _ => {
                return Ok((
                    State::AwaitViewerProof(hand),
                    vec![Event::Ignored("not pair-proof")],
                ))
            }
        };
        let Ok(mac) = fixed_field::<32>(&object, "mac") else {
            return fail(End::Auth("malformed pair-proof"));
        };
        let Some(pair) = hand.pair.as_ref() else {
            return fail(End::Protocol("pairing key missing"));
        };
        if pair.verify_mac(Side::Viewer, &hand.h, &mac).is_err() {
            return fail(End::PairCodeMismatch);
        }
        let pc_mac = pair.mac(Side::Pc, &hand.h);
        let request = PairRequest {
            viewer_id: hand.viewer.id.clone(),
            sas_display: Sas::from_transcript_hash(&hand.h).display(),
        };
        let proof = json!({"type": "pair-proof", "mac": b64(&pc_mac)}).to_string();
        Ok((
            State::AwaitDecision(hand.viewer),
            vec![Event::Send(proof), Event::PairRequest(request)],
        ))
    }

    /// The PC user allowed the pairing. The caller saves the trusted entry, then calls
    /// [`Session::pair_saved`].
    pub fn allow(&mut self) -> Result<Vec<Event>, SessionError> {
        match std::mem::replace(&mut self.state, State::Ended) {
            State::AwaitDecision(viewer) => {
                self.state = State::AwaitSave(viewer.clone());
                Ok(vec![Event::PairAllowed(viewer)])
            }
            other => {
                self.state = other;
                Err(SessionError::Misuse("no pairing request is waiting"))
            }
        }
    }

    /// The PC user denied the pairing.
    pub fn deny(&mut self) -> Result<Vec<Event>, SessionError> {
        match std::mem::replace(&mut self.state, State::Ended) {
            State::AwaitDecision(_) => Ok(vec![
                Event::Send(json!({"type": "pair-denied"}).to_string()),
                Event::Ended(End::PairDenied),
            ]),
            other => {
                self.state = other;
                Err(SessionError::Misuse("no pairing request is waiting"))
            }
        }
    }

    /// The caller's result of saving the trusted entry after `allow()`. Sends pair-done only when
    /// the save succeeded; otherwise pair-denied.
    pub fn pair_saved(&mut self, saved: bool) -> Result<Vec<Event>, SessionError> {
        match std::mem::replace(&mut self.state, State::Ended) {
            State::AwaitSave(_) if saved => Ok(vec![
                Event::Send(json!({"type": "pair-done"}).to_string()),
                Event::Ended(End::PairFinished),
            ]),
            State::AwaitSave(_) => Ok(vec![
                Event::Send(json!({"type": "pair-denied"}).to_string()),
                Event::Ended(End::PairSaveFailed),
            ]),
            other => {
                self.state = other;
                Err(SessionError::Misuse("no pairing save is waiting"))
            }
        }
    }

    /// Ends the session from the caller's side (pairing window closed or expired). Sends nothing.
    pub fn stop(&mut self, reason: End) -> Vec<Event> {
        if matches!(self.state, State::Idle | State::Ended) {
            return Vec::new();
        }
        self.state = State::Ended;
        vec![Event::Ended(reason)]
    }

    /// The viewer's WebRTC offer, as a ctl message. A viewer without dtls-bind-v1 gets a ctl error
    /// and the session ends.
    pub fn webrtc_offer(&mut self, offer_sdp: &str) -> Result<Vec<Event>, SessionError> {
        // The cap check comes before the SDP is read: a viewer without dtls-bind-v1 is refused
        // whatever the offer holds.
        let channel = self.channel_mut()?;
        if !channel.viewer.caps.iter().any(|cap| cap == CAP_DTLS) {
            let error = json!({
                "type": "error",
                "code": "dtls-bind-required",
                "message": "This viewer did not offer dtls-bind-v1, which WebRTC needs.",
            });
            let envelope = channel.seal("error", error.to_string().as_bytes())?;
            self.state = State::Ended;
            return Ok(vec![
                Event::Send(envelope),
                Event::Ended(End::DtlsBindRequired),
            ]);
        }
        let fingerprint = sdp_fingerprint(offer_sdp)?;
        let channel = self.channel_mut()?;
        if channel.offer_fp.is_some() {
            return Err(SessionError::Misuse("offer already received"));
        }
        channel.offer_fp = Some(fingerprint);
        Ok(Vec::new())
    }

    /// Our WebRTC answer. Its fingerprint is the PC's side of the DTLS bind.
    pub fn webrtc_answer(&mut self, answer_sdp: &str) -> Result<(), SessionError> {
        let fingerprint = sdp_fingerprint(answer_sdp)?;
        let channel = self.channel_mut()?;
        if channel.offer_fp.is_none() {
            return Err(SessionError::Misuse("answer before offer"));
        }
        if channel.answer_fp.is_some() {
            return Err(SessionError::Misuse("answer already set"));
        }
        channel.answer_fp = Some(fingerprint);
        Ok(())
    }

    /// Call once DTLS is up. Returns our auth-bind, plus `Bound` the first time both binds have
    /// verified. Idempotent: an ICE restart on the same DTLS calls it again, and the same signed
    /// bind is re-sent (Ed25519 signing is deterministic) without a second `Bound`.
    pub fn dtls_connected(&mut self) -> Result<Vec<Event>, SessionError> {
        let identity = &self.config.identity;
        let channel = match &mut self.state {
            State::Established(channel) => channel,
            _ => return Err(SessionError::Misuse("no established viewer")),
        };
        let (Some(offer_fp), Some(answer_fp)) =
            (channel.offer_fp.clone(), channel.answer_fp.clone())
        else {
            return Err(SessionError::Misuse("DTLS fingerprints are not set"));
        };
        let first_signing = channel.bind_signature.is_none();
        let signature = match channel.bind_signature {
            Some(signature) => signature,
            None => {
                let signature = sign_auth_bind(identity, &channel.h, &answer_fp, &offer_fp)
                    .map_err(SessionError::Crypto)?;
                channel.bind_signature = Some(signature);
                signature
            }
        };
        let mut events = vec![Event::Send(
            json!({"type": "auth-bind", "signature": b64(&signature)}).to_string(),
        )];
        if first_signing && channel.viewer_bound {
            events.push(Event::Bound);
        }
        Ok(events)
    }

    /// Seals an endpoint JSON message as a ctl envelope. The JSON `type` must equal `kind`.
    pub fn seal_text(&mut self, kind: &str, json_text: &str) -> Result<String, SessionError> {
        let channel = self.channel_mut()?;
        let typed = serde_json::from_str::<Value>(json_text).ok();
        if typed
            .as_ref()
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
            != Some(kind)
        {
            return Err(SessionError::Input("plaintext type differs from its kind"));
        }
        channel.seal(kind, json_text.as_bytes())
    }

    /// Seals a cursor frame as a ctl envelope. The bytes go out exactly as given.
    pub fn seal_binary(&mut self, kind: &str, bytes: &[u8]) -> Result<String, SessionError> {
        if kind != "cursor" {
            return Err(SessionError::Input("only cursor frames are sent as binary"));
        }
        self.channel_mut()?.seal(kind, bytes)
    }

    /// The normal handshake finished and the ctl channel is open.
    pub fn is_established(&self) -> bool {
        matches!(self.state, State::Established(_))
    }

    /// Input and media may flow: established, and in WebRTC mode both DTLS binds verified.
    /// Call `webrtc_offer` before starting WebRTC media; WSS has no offer and no bind.
    pub fn media_ready(&self) -> bool {
        matches!(&self.state, State::Established(channel) if channel.media_ready())
    }

    pub fn is_ended(&self) -> bool {
        matches!(self.state, State::Ended)
    }

    /// The authenticated viewer, once established.
    pub fn viewer(&self) -> Option<&Viewer> {
        match &self.state {
            State::Established(channel) => Some(&channel.viewer),
            _ => None,
        }
    }

    fn channel_mut(&mut self) -> Result<&mut Channel, SessionError> {
        match &mut self.state {
            State::Established(channel) => Ok(channel.as_mut()),
            _ => Err(SessionError::Misuse("no established viewer")),
        }
    }
}

impl Channel {
    /// Seals `plaintext` under `kind` and returns the ctl envelope text.
    fn seal(&mut self, kind: &str, plaintext: &[u8]) -> Result<String, SessionError> {
        let (seq, data) = self
            .sealer
            .seal(kind, plaintext)
            .map_err(SessionError::Crypto)?;
        Ok(json!({"type": "ctl", "kind": kind, "seq": seq, "data": b64(&data)}).to_string())
    }

    /// Input and media wait for both DTLS binds once an offer has arrived (WebRTC).
    fn media_ready(&self) -> bool {
        self.offer_fp.is_none() || (self.bind_signature.is_some() && self.viewer_bound)
    }
}

fn established(channel: Box<Channel>, text: &str) -> Result<(State, Vec<Event>), SessionError> {
    let Ok(object) = parse_object(text) else {
        return Ok((
            State::Established(channel),
            vec![Event::Ignored("plaintext message after the handshake")],
        ));
    };
    match object.get("type").and_then(Value::as_str) {
        Some("ctl") => open_ctl(channel, &object),
        Some("auth-bind") => bind(channel, &object),
        _ => Ok((
            State::Established(channel),
            vec![Event::Ignored("plaintext message after the handshake")],
        )),
    }
}

fn open_ctl(
    mut channel: Box<Channel>,
    object: &Map<String, Value>,
) -> Result<(State, Vec<Event>), SessionError> {
    let Ok(kind) = str_field(object, "kind") else {
        return fail(End::Ctl("ctl without a kind"));
    };
    let Some(seq) = object.get("seq").and_then(Value::as_u64) else {
        return fail(End::Ctl("ctl seq is not an integer"));
    };
    let Ok(data) = str_field(object, "data").and_then(decode_bytes) else {
        return fail(End::Ctl("ctl data is not base64url"));
    };
    let Ok(plain) = channel.opener.open(kind, seq, &data) else {
        return fail(End::Ctl("ctl did not open"));
    };
    if kind == "input" {
        if !channel.media_ready() {
            return Ok((
                State::Established(channel),
                vec![Event::Ignored("input before the DTLS bind")],
            ));
        }
        // Input is always the exact plaintext bytes, whatever the first byte: a binary frame may
        // start with `{`, and empty bytes still reach InputData, which validates them.
        return Ok((State::Established(channel), vec![Event::Input(plain)]));
    }
    if kind == "cursor" {
        // Cursor is a binary kind too, but only the PC produces it. Authenticate and
        // consume its sequence without interpreting arbitrary bytes as JSON or input.
        return Ok((
            State::Established(channel),
            vec![Event::Ignored("viewer cursor frame")],
        ));
    }
    // Every other kind is JSON whose `type` equals the kind. The parser takes leading whitespace.
    let Ok(json) = String::from_utf8(plain) else {
        return fail(End::Ctl("ctl plaintext is not UTF-8"));
    };
    let typed = serde_json::from_str::<Value>(&json).ok();
    if typed
        .as_ref()
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
        != Some(kind)
    {
        return fail(End::Ctl("ctl plaintext is not JSON with type == kind"));
    }
    Ok((
        State::Established(channel),
        vec![Event::Message {
            kind: kind.to_owned(),
            json,
        }],
    ))
}

fn bind(
    mut channel: Box<Channel>,
    object: &Map<String, Value>,
) -> Result<(State, Vec<Event>), SessionError> {
    if channel.viewer_bound {
        return Ok((
            State::Established(channel),
            vec![Event::Ignored("auth-bind repeated")],
        ));
    }
    let (Some(offer_fp), Some(answer_fp)) = (channel.offer_fp.clone(), channel.answer_fp.clone())
    else {
        return fail(End::Protocol("auth-bind before the SDP fingerprints"));
    };
    let Ok(signature) = fixed_field::<64>(object, "signature") else {
        return fail(End::Auth("malformed auth-bind"));
    };
    // The viewer signs its own view: its local fingerprint is our remote (the offer), and its
    // remote fingerprint is our local (the answer).
    if verify_auth_bind(
        &channel.viewer.public,
        &channel.h,
        &offer_fp,
        &answer_fp,
        &signature,
    )
    .is_err()
    {
        return fail(End::Auth("auth-bind signature"));
    }
    channel.viewer_bound = true;
    let events = if channel.bind_signature.is_some() {
        vec![Event::Bound]
    } else {
        Vec::new()
    };
    Ok((State::Established(channel), events))
}

/// Parses the relay's plaintext `peer-joined`. `mediaEpoch` is opaque printable ASCII, kept byte
/// for byte and never read as a number.
pub fn parse_peer_joined(text: &str) -> Result<PeerJoined, SessionError> {
    let object = parse_object(text).map_err(SessionError::Input)?;
    if object.get("type").and_then(Value::as_str) != Some("peer-joined") {
        return Err(SessionError::Input("not a peer-joined event"));
    }
    let media_epoch = str_field(&object, "mediaEpoch").map_err(SessionError::Input)?;
    check_epoch(media_epoch)?;
    let Some(pairing) = object.get("pairing").and_then(Value::as_bool) else {
        return Err(SessionError::Input("peer-joined pairing is not a boolean"));
    };
    Ok(PeerJoined {
        media_epoch: media_epoch.to_owned(),
        pairing,
    })
}

/// The `a=fingerprint` value of an SDP: `sha-256` plus 32 uppercase hex pairs. Every fingerprint
/// line must carry the same value.
pub fn sdp_fingerprint(sdp: &str) -> Result<String, SessionError> {
    let mut found: Option<&str> = None;
    for line in sdp.lines() {
        let Some(value) = line.strip_prefix("a=fingerprint:") else {
            continue;
        };
        match found {
            Some(existing) if existing != value => {
                return Err(SessionError::Input("SDP has conflicting fingerprints"));
            }
            _ => found = Some(value),
        }
    }
    let value = found.ok_or(SessionError::Input("SDP has no a=fingerprint line"))?;
    check_fingerprint(value)
        .map_err(|_| SessionError::Input("SDP fingerprint is not sha-256 with uppercase hex"))?;
    Ok(value.to_owned())
}

/// The epoch is opaque printable ASCII (no spaces, no control characters), 1 to 64 bytes.
fn check_epoch(epoch: &str) -> Result<(), SessionError> {
    let ok = (1..=MAX_EPOCH_LEN).contains(&epoch.len())
        && epoch.bytes().all(|byte| byte.is_ascii_graphic());
    if ok {
        Ok(())
    } else {
        Err(SessionError::Input(
            "mediaEpoch is not opaque printable ASCII",
        ))
    }
}

struct Hello {
    viewer: Viewer,
    id: DeviceId,
    nonce: [u8; 32],
    ephemeral: [u8; 32],
}

fn parse_hello(text: &str, peer: &Peer) -> Result<Hello, &'static str> {
    let object = parse_object(text)?;
    if object.get("type").and_then(Value::as_str) != Some("auth-hello") {
        return Err("not an auth-hello");
    }
    let caps = string_list(&object, "caps")?;
    if !caps.iter().any(|cap| cap == CAP_CTL) {
        return Err("viewer lacks control-aead-v1");
    }
    let offers_pairing = caps.iter().any(|cap| cap == CAP_PAIR);
    if offers_pairing != peer.pairing {
        return Err(if peer.pairing {
            "pairing viewer lacks pair-v1"
        } else {
            "pair-v1 offered outside pairing"
        });
    }
    if peer.pairing && peer.pair.is_none() {
        return Err("pairing without an open code");
    }
    let id_text = str_field(&object, "id")?;
    let id = DeviceId::parse(id_text).map_err(|_| "viewer id is not a device ID")?;
    if id.to_string() != id_text {
        return Err("viewer id is not canonical");
    }
    let public = fixed_field::<32>(&object, "public")?;
    if DeviceId::from_public_key(&public) != id {
        return Err("viewer id does not match its public key");
    }
    if !peer.pairing
        && !peer
            .trusted
            .iter()
            .any(|entry| entry.id == id && entry.public == public)
    {
        return Err("viewer is not trusted");
    }
    let nonce = fixed_field::<32>(&object, "nonce")?;
    let ephemeral = fixed_field::<32>(&object, "ephemeral")?;
    Ok(Hello {
        viewer: Viewer {
            id: id_text.to_owned(),
            public,
            caps,
        },
        id,
        nonce,
        ephemeral,
    })
}

fn fail(end: End) -> Result<(State, Vec<Event>), SessionError> {
    Ok((State::Ended, vec![Event::Ended(end)]))
}

fn message_type(text: &str) -> Option<String> {
    let object = parse_object(text).ok()?;
    object
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn parse_object(text: &str) -> Result<Map<String, Value>, &'static str> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(object)) => Ok(object),
        _ => Err("message is not a JSON object"),
    }
}

fn str_field<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, &'static str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or("missing or non-string field")
}

fn string_list(object: &Map<String, Value>, key: &str) -> Result<Vec<String>, &'static str> {
    let items = object
        .get(key)
        .and_then(Value::as_array)
        .ok_or("missing or non-array field")?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or("non-string list entry")
        })
        .collect()
}

fn fixed_field<const N: usize>(
    object: &Map<String, Value>,
    key: &str,
) -> Result<[u8; N], &'static str> {
    decode_fixed(str_field(object, key)?)
}

/// Strict unpadded base64url: the text must re-encode to itself.
fn decode_bytes(text: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| "field is not base64url")?;
    if URL_SAFE_NO_PAD.encode(&bytes) != text {
        return Err("field is not canonical base64url");
    }
    Ok(bytes)
}

fn decode_fixed<const N: usize>(text: &str) -> Result<[u8; N], &'static str> {
    let bytes = decode_bytes(text)?;
    <[u8; N]>::try_from(bytes.as_slice()).map_err(|_| "field has the wrong length")
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bp_device_key::phase2::sign_auth_finish;

    const FIXTURE: &str = include_str!("../../fixtures/device-keys-contract.json");
    const RELAY_ORIGIN: &str = "https://example.test";

    /// Fixture nonces and ephemeral secrets, injected through the randomness seam.
    struct FixedRandom {
        nonce: [u8; 32],
        ephemeral: [u8; 32],
    }

    impl Randomness for FixedRandom {
        fn nonce(&mut self) -> Result<[u8; 32], SessionError> {
            Ok(self.nonce)
        }

        fn ephemeral(&mut self) -> Result<[u8; 32], SessionError> {
            Ok(self.ephemeral)
        }
    }

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).expect("fixture parses")
    }

    fn s(value: &Value) -> &str {
        value.as_str().expect("string")
    }

    fn unhex(value: &str) -> Vec<u8> {
        (0..value.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&value[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn bytes32(value: &str) -> [u8; 32] {
        URL_SAFE_NO_PAD
            .decode(value)
            .expect("base64url")
            .try_into()
            .expect("32 bytes")
    }

    fn hex32(value: &str) -> [u8; 32] {
        unhex(value).try_into().expect("32 bytes")
    }

    fn pc_identity() -> DeviceIdentity {
        let f = fixture();
        DeviceIdentity::from_seed(&hex32(s(&f["handshake"]["pc"]["seedHex"])))
    }

    fn viewer_identity() -> DeviceIdentity {
        let f = fixture();
        DeviceIdentity::from_seed(&hex32(s(&f["handshake"]["viewer"]["seedHex"])))
    }

    fn trusted_viewer() -> Vec<Trusted> {
        let f = fixture();
        vec![Trusted {
            id: DeviceId::parse(s(&f["handshake"]["viewer"]["id"])).expect("id"),
            public: bytes32(s(&f["handshake"]["viewer"]["public"])),
        }]
    }

    fn fixture_epoch() -> String {
        let f = fixture();
        s(&f["handshake"]["sessionEpoch"]).to_owned()
    }

    fn peer(epoch: &str, pairing: bool) -> PeerJoined {
        PeerJoined {
            media_epoch: epoch.to_owned(),
            pairing,
        }
    }

    fn pc_random() -> Box<dyn Randomness> {
        let f = fixture();
        Box::new(FixedRandom {
            nonce: bytes32(s(&f["handshake"]["pc"]["nonce"])),
            ephemeral: hex32(s(&f["handshake"]["pc"]["ephemeralPrivateHex"])),
        })
    }

    fn pc_session() -> Session {
        Session::new(
            SessionConfig {
                identity: pc_identity(),
                relay_origin: RELAY_ORIGIN.to_owned(),
                pc_caps: vec![CAP_CTL.to_owned(), CAP_DTLS.to_owned()],
            },
            pc_random(),
        )
        .expect("session")
    }

    /// The first Send in `events`, parsed.
    fn sent(events: &[Event]) -> Value {
        match events.first() {
            Some(Event::Send(text)) => serde_json::from_str(text).expect("json"),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    /// A normal handshake through auth-finish, with the fixture's epoch and trusted viewer.
    fn established_session() -> Session {
        let f = fixture();
        let hs = &f["handshake"];
        let mut session = pc_session();
        session
            .peer_joined(peer(&fixture_epoch(), false), trusted_viewer(), None)
            .expect("peer");
        session
            .on_text(&hs["messages"]["authHello"].to_string())
            .expect("hello");
        session
            .on_text(&hs["messages"]["authFinish"].to_string())
            .expect("finish");
        assert!(session.is_established());
        session
    }

    fn pairing_session(code: &str) -> Session {
        let f = fixture();
        let hs = &f["handshake"];
        let mut session = pc_session();
        session
            .peer_joined(
                peer(&fixture_epoch(), true),
                Vec::new(),
                Some(PairingCode::parse(code).expect("code")),
            )
            .expect("peer");
        session
            .on_text(&f["phase2"]["pairing"]["messages"]["authHello"].to_string())
            .expect("hello");
        session
            .on_text(&hs["messages"]["authFinish"].to_string())
            .expect("finish");
        session
    }

    fn assert_ignored(session: &mut Session, text: &str) {
        let events = session.on_text(text).expect("on_text");
        assert!(
            matches!(events.as_slice(), [Event::Ignored(_)]),
            "{events:?}"
        );
        assert!(!session.is_ended());
    }

    #[test]
    fn normal_handshake_matches_fixture() {
        let f = fixture();
        let hs = &f["handshake"];
        let mut session = pc_session();
        let events = session
            .peer_joined(peer(&fixture_epoch(), false), trusted_viewer(), None)
            .expect("peer");
        assert!(events.is_empty());

        let events = session
            .on_text(&hs["messages"]["authHello"].to_string())
            .expect("hello");
        assert_eq!(sent(&events), hs["messages"]["authHost"]);

        let events = session
            .on_text(&hs["messages"]["authFinish"].to_string())
            .expect("finish");
        assert_eq!(events.len(), 2);
        let caps = sent(&events);
        assert_eq!(caps["kind"], "caps");
        assert_eq!(caps["seq"], 0);
        let mut opener = CtlOpener::new(&hex32(s(&hs["controlKeys"]["pcToViewerHex"])));
        let plain = opener
            .open(
                "caps",
                0,
                &URL_SAFE_NO_PAD.decode(s(&caps["data"])).expect("b64"),
            )
            .expect("caps opens");
        let plain: Value = serde_json::from_slice(&plain).expect("json");
        assert_eq!(plain, json!({"type": "caps", "caps": [CAP_CTL, CAP_DTLS]}));
        assert!(
            matches!(&events[1], Event::Established(viewer) if viewer.id == "47Z3-3QX1-AJH6-2RKB")
        );

        // Viewer-to-PC fixture messages open in order and dispatch with their plaintext.
        for message in hs["controlMessages"]
            .as_array()
            .expect("array")
            .iter()
            .filter(|message| message["direction"] == "viewer-to-pc")
        {
            let envelope = json!({
                "type": "ctl",
                "kind": message["type"],
                "seq": message["sequence"],
                "data": message["ciphertext"],
            })
            .to_string();
            let events = session.on_text(&envelope).expect("ctl");
            match events.as_slice() {
                // Input is always the exact plaintext bytes, even when they are JSON.
                [Event::Input(bytes)] => {
                    assert_eq!(bytes.as_slice(), s(&message["plaintextUtf8"]).as_bytes());
                }
                other => panic!("{other:?}"),
            }
        }

        // PC-to-viewer seq 1 is the fixture's second clipboard message.
        let sealed = session
            .seal_text("clipboard", r#"{"type":"clipboard","text":"world"}"#)
            .expect("seal");
        let sealed: Value = serde_json::from_str(&sealed).expect("json");
        assert_eq!(sealed["seq"], 1);
        assert_eq!(sealed["data"], hs["controlMessages"][3]["ciphertext"]);

        assert_ignored(&mut session, r#"{"type":"clipboard","text":"x"}"#);
    }

    #[test]
    fn phase2_ctl_vectors_and_failures_end_the_session() {
        let f = fixture();
        let hs = &f["handshake"];
        let ctl = &f["phase2"]["ctl"];

        let mut session = established_session();
        let input = &ctl["messages"][0]["message"];
        match session
            .on_text(&input.to_string())
            .expect("input")
            .as_slice()
        {
            [Event::Input(bytes)] => assert_eq!(bytes, &unhex("0d024b002500")),
            other => panic!("{other:?}"),
        }
        let file_cancel = &ctl["messages"][1]["message"];
        match session
            .on_text(&file_cancel.to_string())
            .expect("file-cancel")
            .as_slice()
        {
            [Event::Message { kind, json }] => {
                assert_eq!(kind.as_str(), "file-cancel");
                assert_eq!(json.as_str(), s(&ctl["messages"][1]["plaintextUtf8"]));
            }
            other => panic!("{other:?}"),
        }
        // Replayed sequence number.
        assert!(matches!(
            session
                .on_text(&input.to_string())
                .expect("replay")
                .as_slice(),
            [Event::Ended(End::Ctl(_))]
        ));
        assert!(session.is_ended());

        // Skipped sequence number.
        let mut session = established_session();
        assert!(matches!(
            session
                .on_text(&ctl["failures"][1]["message"].to_string())
                .expect("skip")
                .as_slice(),
            [Event::Ended(End::Ctl(_))]
        ));

        // Wrong kind for the authentication data.
        let mut session = established_session();
        assert!(matches!(
            session
                .on_text(&ctl["failures"][2]["message"].to_string())
                .expect("kind")
                .as_slice(),
            [Event::Ended(End::Ctl(_))]
        ));

        // A PC-to-viewer ciphertext offered in the viewer-to-PC direction.
        let mut session = established_session();
        let reflected = json!({
            "type": "ctl",
            "kind": "clipboard",
            "seq": 0,
            "data": hs["controlMessages"][2]["ciphertext"],
        });
        assert!(matches!(
            session
                .on_text(&reflected.to_string())
                .expect("direction")
                .as_slice(),
            [Event::Ended(End::Ctl(_))]
        ));

        // Plaintext endpoint messages are dropped, not dispatched.
        let mut session = established_session();
        assert_ignored(&mut session, r#"{"type":"input","action":"key","key":"A"}"#);
        assert!(session.is_established());
    }

    #[test]
    fn pairing_matches_fixture_and_allow_then_pair_done() {
        let f = fixture();
        let hs = &f["handshake"];
        let pair = &f["phase2"]["pairing"];
        let mut session = pairing_session(s(&pair["code"]));
        // pairing_session has already sent its auth-host; check that one in a fresh session.
        let mut check = session_for_pairing_host();
        let events = check
            .on_text(&pair["messages"]["authHello"].to_string())
            .expect("hello");
        assert_eq!(sent(&events), hs["messages"]["authHost"]);

        let events = session
            .on_text(&pair["messages"]["pairProofViewer"].to_string())
            .expect("proof");
        match events.as_slice() {
            [Event::Send(proof), Event::PairRequest(request)] => {
                let proof: Value = serde_json::from_str(proof).expect("json");
                assert_eq!(proof, pair["messages"]["pairProofPc"]);
                assert_eq!(request.viewer_id, "47Z3-3QX1-AJH6-2RKB");
                assert_eq!(request.sas_display, s(&pair["sasDisplay"]));
            }
            other => panic!("{other:?}"),
        }

        let events = session.allow().expect("allow");
        assert!(matches!(
            events.as_slice(),
            [Event::PairAllowed(viewer)] if viewer.public == bytes32(s(&hs["viewer"]["public"]))
        ));
        let events = session.pair_saved(true).expect("saved");
        assert_eq!(sent(&events), json!({"type": "pair-done"}));
        assert!(matches!(
            events.as_slice(),
            [Event::Send(_), Event::Ended(End::PairFinished)]
        ));
        assert!(session.is_ended());
    }

    /// A PC session at the pairing hello, for checking the auth-host it sends.
    fn session_for_pairing_host() -> Session {
        let f = fixture();
        let mut session = pc_session();
        session
            .peer_joined(
                peer(&fixture_epoch(), true),
                Vec::new(),
                Some(PairingCode::parse(s(&f["phase2"]["pairing"]["code"])).expect("code")),
            )
            .expect("peer");
        session
    }

    #[test]
    fn wrong_code_mac_ends_without_showing_a_number() {
        let f = fixture();
        let pair = &f["phase2"]["pairing"];
        let mut session = pairing_session(s(&pair["code"]));
        let events = session
            .on_text(&json!({"type": "pair-proof", "mac": pair["wrongCodeViewerMac"]}).to_string())
            .expect("proof");
        assert!(matches!(
            events.as_slice(),
            [Event::Ended(End::PairCodeMismatch)]
        ));
    }

    #[test]
    fn deny_sends_pair_denied_and_ends() {
        let f = fixture();
        let pair = &f["phase2"]["pairing"];
        let mut session = pairing_session(s(&pair["code"]));
        session
            .on_text(&pair["messages"]["pairProofViewer"].to_string())
            .expect("proof");
        let events = session.deny().expect("deny");
        assert_eq!(sent(&events), json!({"type": "pair-denied"}));
        assert!(matches!(
            events.as_slice(),
            [Event::Send(_), Event::Ended(End::PairDenied)]
        ));
        assert!(session.deny().is_err());
    }

    #[test]
    fn unknown_or_mismatched_viewers_are_ignored_silently() {
        let f = fixture();
        let hello = f["handshake"]["messages"]["authHello"].clone();
        let pairing_hello = f["phase2"]["pairing"]["messages"]["authHello"].clone();
        let viewer_id = DeviceId::parse(s(&f["handshake"]["viewer"]["id"])).expect("id");

        // Not in the trusted list.
        let mut session = pc_session();
        session
            .peer_joined(peer(&fixture_epoch(), false), Vec::new(), None)
            .expect("peer");
        assert_refused(&mut session, &hello.to_string());

        // Trusted ID with a different public key.
        let mut session = pc_session();
        session
            .peer_joined(
                peer(&fixture_epoch(), false),
                vec![Trusted {
                    id: viewer_id,
                    public: [7u8; 32],
                }],
                None,
            )
            .expect("peer");
        assert_refused(&mut session, &hello.to_string());

        // No control-aead-v1: no channel to explain the refusal.
        let mut no_ctl = hello.clone();
        no_ctl["caps"] = json!(["dtls-bind-v1"]);
        let mut session = pc_session();
        session
            .peer_joined(peer(&fixture_epoch(), false), trusted_viewer(), None)
            .expect("peer");
        assert_refused(&mut session, &no_ctl.to_string());

        // pair-v1 offered outside a pairing window.
        let mut session = pc_session();
        session
            .peer_joined(peer(&fixture_epoch(), false), trusted_viewer(), None)
            .expect("peer");
        assert_refused(&mut session, &pairing_hello.to_string());

        // Pairing window open, but the viewer did not offer pair-v1.
        let mut session = pc_session();
        session
            .peer_joined(
                peer(&fixture_epoch(), true),
                Vec::new(),
                Some(PairingCode::parse(s(&f["phase2"]["pairing"]["code"])).expect("code")),
            )
            .expect("peer");
        assert_refused(&mut session, &hello.to_string());

        // Pairing peer without an open code.
        let mut session = pc_session();
        session
            .peer_joined(peer(&fixture_epoch(), true), Vec::new(), None)
            .expect("peer");
        assert_refused(&mut session, &pairing_hello.to_string());
    }

    #[test]
    fn auth_hello_before_peer_joined_ends_the_attempt() {
        let f = fixture();
        let mut session = pc_session();
        let events = session
            .on_text(&f["handshake"]["messages"]["authHello"].to_string())
            .expect("on_text");
        assert!(matches!(
            events.as_slice(),
            [Event::Ended(End::Protocol(_))]
        ));
        assert!(session.is_ended());
        assert_ignored_after_end(
            &mut session,
            &f["handshake"]["messages"]["authHello"].to_string(),
        );
    }

    fn assert_refused(session: &mut Session, text: &str) {
        let events = session.on_text(text).expect("on_text");
        assert!(
            matches!(events.as_slice(), [Event::Ended(End::Refused(_))]),
            "{events:?}"
        );
        assert!(session.is_ended());
    }

    #[test]
    fn refused_auth_hello_sends_nothing_and_waits_for_a_new_peer() {
        let f = fixture();
        let hs = &f["handshake"];
        let hello = hs["messages"]["authHello"].to_string();
        let mut session = pc_session();
        // Untrusted: the attempt ends with nothing on the wire.
        session
            .peer_joined(peer(&fixture_epoch(), false), Vec::new(), None)
            .expect("peer");
        assert_refused(&mut session, &hello);
        // The same viewer retried in the same attempt gets nothing.
        assert_ignored_after_end(&mut session, &hello);
        // A new peer-joined starts a fresh attempt; the trusted viewer gets its auth-host.
        session
            .peer_joined(peer(&fixture_epoch(), false), trusted_viewer(), None)
            .expect("peer");
        let events = session.on_text(&hello).expect("hello");
        assert_eq!(sent(&events), hs["messages"]["authHost"]);
    }

    #[test]
    fn endpoint_messages_before_auth_finish_are_dropped_not_fatal() {
        let f = fixture();
        let hs = &f["handshake"];
        let mut session = pc_session();
        session
            .peer_joined(peer(&fixture_epoch(), false), trusted_viewer(), None)
            .expect("peer");
        session
            .on_text(&hs["messages"]["authHello"].to_string())
            .expect("hello");
        assert_ignored(&mut session, r#"{"type":"input","action":"key","key":"A"}"#);
        assert_ignored(&mut session, r#"{"type":"clipboard","text":"x"}"#);
        assert_ignored(&mut session, "not JSON at all");
        assert!(!session.is_ended());
        let events = session
            .on_text(&hs["messages"]["authFinish"].to_string())
            .expect("finish");
        assert!(matches!(
            events.as_slice(),
            [Event::Send(_), Event::Established(_)]
        ));
    }

    #[test]
    fn endpoint_messages_before_pair_proof_are_dropped_not_fatal() {
        let f = fixture();
        let pair = &f["phase2"]["pairing"];
        let mut session = pairing_session(s(&pair["code"]));
        assert_ignored(&mut session, r#"{"type":"input","action":"key","key":"A"}"#);
        let events = session
            .on_text(&pair["messages"]["pairProofViewer"].to_string())
            .expect("proof");
        assert!(matches!(
            events.as_slice(),
            [Event::Send(_), Event::PairRequest(_)]
        ));
    }

    #[test]
    fn malformed_auth_finish_and_pair_proof_still_end() {
        let f = fixture();
        let hs = &f["handshake"];
        let mut session = pc_session();
        session
            .peer_joined(peer(&fixture_epoch(), false), trusted_viewer(), None)
            .expect("peer");
        session
            .on_text(&hs["messages"]["authHello"].to_string())
            .expect("hello");
        assert!(matches!(
            session
                .on_text(r#"{"type":"auth-finish"}"#)
                .expect("finish")
                .as_slice(),
            [Event::Ended(End::Auth(_))]
        ));

        let pair = &f["phase2"]["pairing"];
        let mut session = pairing_session(s(&pair["code"]));
        assert!(matches!(
            session
                .on_text(r#"{"type":"pair-proof","mac":"AA"}"#)
                .expect("proof")
                .as_slice(),
            [Event::Ended(End::Auth(_))]
        ));
    }

    #[test]
    fn input_is_the_exact_bytes_whatever_they_start_with() {
        let f = fixture();
        let hs = &f["handshake"];
        let mut session = established_session();
        let mut viewer = CtlSealer::new(&hex32(s(&hs["controlKeys"]["viewerToPcHex"])));
        // A frame starting with `{` is still binary input; so are whitespace-led and empty bytes.
        let frames: [&[u8]; 5] = [b"{", b"{\"type\":\"input\"}", b" {", b"\n\t0", b""];
        for frame in frames {
            let (seq, data) = viewer.seal("input", frame).expect("seal");
            let envelope = json!({"type": "ctl", "kind": "input", "seq": seq, "data": b64(&data)});
            match session
                .on_text(&envelope.to_string())
                .expect("ctl")
                .as_slice()
            {
                [Event::Input(bytes)] => assert_eq!(bytes.as_slice(), frame),
                other => panic!("{other:?}"),
            }
        }
        // Other kinds are JSON and may lead with whitespace.
        let plain = " {\"type\":\"file-cancel\",\"id\":\"t1\"}";
        let (seq, data) = viewer.seal("file-cancel", plain.as_bytes()).expect("seal");
        let envelope =
            json!({"type": "ctl", "kind": "file-cancel", "seq": seq, "data": b64(&data)});
        match session
            .on_text(&envelope.to_string())
            .expect("ctl")
            .as_slice()
        {
            [Event::Message { kind, json: text }] => {
                assert_eq!(kind.as_str(), "file-cancel");
                assert_eq!(text.as_str(), plain);
            }
            other => panic!("{other:?}"),
        }
        assert!(session.is_established());
    }

    #[test]
    fn webrtc_offer_without_dtls_bind_is_refused_before_the_sdp_is_read() {
        let f = fixture();
        let hs = &f["handshake"];
        let mut hello = hs["messages"]["authHello"].clone();
        hello["caps"] = json!([CAP_CTL]);
        let mut session = pc_session();
        session
            .peer_joined(peer(&fixture_epoch(), false), trusted_viewer(), None)
            .expect("peer");
        session.on_text(&hello.to_string()).expect("hello");
        session
            .on_text(&hs["messages"]["authFinish"].to_string())
            .expect("finish");
        let events = session.webrtc_offer("this is not SDP").expect("offer");
        assert!(matches!(
            events.as_slice(),
            [Event::Send(_), Event::Ended(End::DtlsBindRequired)]
        ));
        assert!(session.is_ended());
    }

    #[test]
    fn malformed_offer_with_dtls_bind_is_an_input_error_and_changes_nothing() {
        let mut session = established_session();
        assert!(matches!(
            session.webrtc_offer("no fingerprint"),
            Err(SessionError::Input(_))
        ));
        assert!(session.is_established());
    }

    fn assert_ignored_after_end(session: &mut Session, text: &str) {
        assert!(matches!(
            session.on_text(text).expect("on_text").as_slice(),
            [Event::Ignored(_)]
        ));
    }

    #[test]
    fn auth_finish_with_the_wrong_signature_ends_the_session() {
        let f = fixture();
        let hs = &f["handshake"];
        let mut session = pc_session();
        session
            .peer_joined(peer(&fixture_epoch(), false), trusted_viewer(), None)
            .expect("peer");
        session
            .on_text(&hs["messages"]["authHello"].to_string())
            .expect("hello");
        // The PC's own signature in place of the viewer's.
        let forged =
            json!({"type": "auth-finish", "signature": hs["messages"]["authHost"]["signature"]});
        assert!(matches!(
            session
                .on_text(&forged.to_string())
                .expect("finish")
                .as_slice(),
            [Event::Ended(End::Auth(_))]
        ));
    }

    #[test]
    fn peer_reset_ends_an_established_viewer() {
        let mut session = established_session();
        let events = session
            .peer_joined(peer(&fixture_epoch(), false), trusted_viewer(), None)
            .expect("peer");
        assert!(matches!(events.as_slice(), [Event::Ended(End::PeerReset)]));
        assert!(session.viewer().is_none());
        assert!(!session.is_established());
    }

    #[test]
    fn webrtc_bind_matches_fixture_and_gates_media() {
        let f = fixture();
        let hs = &f["handshake"];
        let dtls = &hs["dtls"];
        let mut session = established_session();
        let offer = format!(
            "v=0\r\na=fingerprint:{}\r\n",
            s(&dtls["viewer"]["localFingerprint"])
        );
        assert!(session.webrtc_offer(&offer).expect("offer").is_empty());
        let answer = format!(
            "v=0\r\na=fingerprint:{}\r\n",
            s(&dtls["pc"]["localFingerprint"])
        );
        session.webrtc_answer(&answer).expect("answer");
        assert!(!session.media_ready());

        // The viewer's auth-bind may arrive before our DTLS is up.
        let events = session
            .on_text(&hs["messages"]["authBind"].to_string())
            .expect("viewer bind");
        assert!(events.is_empty());
        assert!(!session.media_ready());

        let events = session.dtls_connected().expect("bind");
        match events.as_slice() {
            [Event::Send(text), Event::Bound] => {
                let text: Value = serde_json::from_str(text).expect("json");
                assert_eq!(text["type"], "auth-bind");
                assert_eq!(text["signature"], dtls["pc"]["signature"]);
            }
            other => panic!("{other:?}"),
        }
        assert!(session.media_ready());
        // A repeated call (an ICE restart on the same DTLS) re-sends the same bind, with no second
        // Bound.
        let again = session.dtls_connected().expect("repeat");
        assert_eq!(sent(&again)["signature"], dtls["pc"]["signature"]);
        assert!(matches!(again.as_slice(), [Event::Send(_)]));
    }

    #[test]
    fn webrtc_offer_without_dtls_bind_sends_error_and_ends() {
        let f = fixture();
        let hs = &f["handshake"];
        let mut hello = hs["messages"]["authHello"].clone();
        hello["caps"] = json!([CAP_CTL]);
        let mut session = pc_session();
        session
            .peer_joined(peer(&fixture_epoch(), false), trusted_viewer(), None)
            .expect("peer");
        session.on_text(&hello.to_string()).expect("hello");
        let finish = session
            .on_text(&hs["messages"]["authFinish"].to_string())
            .expect("finish");
        let offer = format!(
            "a=fingerprint:{}\n",
            s(&hs["dtls"]["viewer"]["localFingerprint"])
        );
        let events = session.webrtc_offer(&offer).expect("offer");
        match events.as_slice() {
            [Event::Send(error), Event::Ended(End::DtlsBindRequired)] => {
                // The error is the second ctl message (the caps came first) and opens under the
                // PC-to-viewer key.
                let mut opener = CtlOpener::new(&hex32(s(&hs["controlKeys"]["pcToViewerHex"])));
                let caps: Value = serde_json::from_str(match &finish[0] {
                    Event::Send(text) => text,
                    other => panic!("{other:?}"),
                })
                .expect("json");
                opener
                    .open(
                        "caps",
                        0,
                        &URL_SAFE_NO_PAD.decode(s(&caps["data"])).expect("b64"),
                    )
                    .expect("caps");
                let error: Value = serde_json::from_str(error).expect("json");
                let plain = opener
                    .open(
                        "error",
                        1,
                        &URL_SAFE_NO_PAD.decode(s(&error["data"])).expect("b64"),
                    )
                    .expect("error opens");
                let plain: Value = serde_json::from_slice(&plain).expect("json");
                assert_eq!(plain["code"], "dtls-bind-required");
            }
            other => panic!("{other:?}"),
        }
        assert!(session.is_ended());
    }

    #[test]
    fn sdp_fingerprints_are_extracted_exactly() {
        let value = format!("sha-256 {}", ["AB"; 32].join(":"));
        let sdp = format!(
            "v=0\r\na=setup:actpass\r\na=fingerprint:{value}\r\nm=application 9 UDP/DTLS/SCTP\r\na=fingerprint:{value}\r\n"
        );
        assert_eq!(sdp_fingerprint(&sdp).expect("fingerprint"), value);
        assert!(sdp_fingerprint("v=0\r\n").is_err());
        let other = format!("sha-256 {}", ["CD"; 32].join(":"));
        assert!(
            sdp_fingerprint(&format!("a=fingerprint:{value}\na=fingerprint:{other}\n")).is_err()
        );
        let lowercase = format!("sha-256 {}", ["ab"; 32].join(":"));
        assert!(sdp_fingerprint(&format!("a=fingerprint:{lowercase}\n")).is_err());
    }

    #[test]
    fn peer_joined_epoch_is_opaque_ascii() {
        assert_eq!(
            parse_peer_joined(
                r#"{"type":"peer-joined","mediaEpoch":"9007199254740993","pairing":false}"#
            )
            .expect("decimal"),
            peer("9007199254740993", false)
        );
        let hex = "0123456789abcdef0123456789abcdef";
        let line = format!(r#"{{"type":"peer-joined","mediaEpoch":"{hex}","pairing":true}}"#);
        assert_eq!(parse_peer_joined(&line).expect("hex"), peer(hex, true));
        let long = "a".repeat(MAX_EPOCH_LEN + 1);
        for bad in [
            r#"{"type":"peer-joined","mediaEpoch":12,"pairing":false}"#.to_owned(),
            r#"{"type":"peer-joined","mediaEpoch":"","pairing":false}"#.to_owned(),
            r#"{"type":"peer-joined","mediaEpoch":"a b","pairing":false}"#.to_owned(),
            r#"{"type":"peer-joined","mediaEpoch":"x","pairing":"no"}"#.to_owned(),
            r#"{"type":"hello-ok","mediaEpoch":"x","pairing":false}"#.to_owned(),
            format!(r#"{{"type":"peer-joined","mediaEpoch":"{long}","pairing":false}}"#),
        ] {
            assert!(parse_peer_joined(&bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn live_hex_epoch_is_bound_into_the_transcript_byte_for_byte() {
        let f = fixture();
        let hs = &f["handshake"];
        let epoch = "0123456789abcdef0123456789abcdef";
        let line = format!(r#"{{"type":"peer-joined","mediaEpoch":"{epoch}","pairing":false}}"#);
        let mut session = pc_session();
        session
            .peer_joined(
                parse_peer_joined(&line).expect("peer"),
                trusted_viewer(),
                None,
            )
            .expect("peer_joined");
        let events = session
            .on_text(&hs["messages"]["authHello"].to_string())
            .expect("hello");

        // The transcript hash the crate computes over the epoch bytes under test.
        let pc = pc_identity();
        let transcript_hash = |epoch: &str| -> [u8; 32] {
            Transcript {
                relay_origin: RELAY_ORIGIN,
                epoch,
                viewer_id: DeviceId::parse(s(&hs["viewer"]["id"])).expect("id"),
                viewer_public: bytes32(s(&hs["viewer"]["public"])),
                pc_id: pc.id(),
                pc_public: pc.public_key(),
                viewer_nonce: bytes32(s(&hs["viewer"]["nonce"])),
                pc_nonce: bytes32(s(&hs["pc"]["nonce"])),
                viewer_ephemeral: bytes32(s(&hs["viewer"]["ephemeral"])),
                pc_ephemeral: bytes32(s(&hs["pc"]["ephemeral"])),
            }
            .hash()
        };
        let h = transcript_hash(epoch);
        let pc_signature = sign_auth_host(&pc, &h);
        // A different transcript (the fixture epoch) gives a different signature, so this equality
        // proves the session hashed the hex epoch's exact bytes.
        assert_ne!(
            transcript_hash(fixture_epoch().as_str()),
            h,
            "the epoch must change the transcript"
        );
        assert_eq!(sent(&events)["signature"], b64(&pc_signature));

        // A viewer that signed the fixture-epoch transcript is refused on the hex-epoch session.
        let mut stale = pc_session();
        stale
            .peer_joined(
                parse_peer_joined(&line).expect("peer"),
                trusted_viewer(),
                None,
            )
            .expect("peer_joined");
        stale
            .on_text(&hs["messages"]["authHello"].to_string())
            .expect("hello");
        let stale_signature = sign_auth_finish(
            &viewer_identity(),
            &transcript_hash(fixture_epoch().as_str()),
            &pc_signature,
        );
        let finish = json!({"type": "auth-finish", "signature": b64(&stale_signature)});
        assert!(matches!(
            stale
                .on_text(&finish.to_string())
                .expect("finish")
                .as_slice(),
            [Event::Ended(End::Auth(_))]
        ));

        // The viewer signs the hex-epoch transcript; the session accepts it and establishes.
        let viewer_signature = sign_auth_finish(&viewer_identity(), &h, &pc_signature);
        let events = session
            .on_text(
                &json!({"type": "auth-finish", "signature": b64(&viewer_signature)}).to_string(),
            )
            .expect("finish");
        assert!(matches!(
            events.as_slice(),
            [Event::Send(_), Event::Established(_)]
        ));
    }
}
