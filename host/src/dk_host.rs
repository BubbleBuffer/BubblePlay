//! Host-side ownership of pairing, trust persistence and the device-key session.
//! The UI and signaling socket consume ordered effects; secret values never go to logs.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use bp_device_key::phase2::PairingCode;
use serde_json::{json, Value};

use crate::devices_ui_api::{pair_qr_uri, DeviceRow, DevicesCommand, DevicesEvent, HostEnds};
use crate::dk_session::{
    self, End, Event, OsRandomness, Randomness, Session, SessionConfig, Trusted,
};
use crate::trusted_devices::{self, TrustedDevices, UNNAMED_DEVICE};

pub type SharedDevicesUi = Arc<Mutex<HostEnds>>;

pub enum Effect {
    Send(Value),
    Ui(DevicesEvent),
    Park,
    Bound,
}

pub enum Inbound {
    Json(String),
    Input(Vec<u8>),
}

struct PairWindow {
    code: PairingCode,
    deadline: Instant,
    claimed: bool,
}

pub fn admit_message(ids: impl IntoIterator<Item = String>) -> Value {
    json!({"type":"admit", "viewers":ids.into_iter().collect::<Vec<_>>(), "remotePairing":false})
}

pub fn pair_open_message(code: &PairingCode, pc: &bp_device_key::DeviceId) -> Value {
    json!({"type":"pair-open", "admit":URL_SAFE_NO_PAD.encode(code.admit_hash(pc))})
}

pub fn pair_close_message() -> Value {
    json!({"type":"pair-close"})
}

const PAIR_WINDOW: Duration = Duration::from_secs(300);
const DROP_LOG_INTERVAL: Duration = Duration::from_secs(1);
const MAX_COMMANDS_PER_POLL: usize = 64;
const MAX_WIRE_BYTES: usize = 512 * 1024;

/// Owns the trust file, the PC's pairing window and one viewer session. The caller must drain
/// effects after every mutating call, in order, before accepting another viewer message.
/// `Park` fences old media/input work; it does not close the PC's signaling socket.
pub struct HostProtocol {
    session: Session,
    pc_id: bp_device_key::DeviceId,
    origin: String,
    trusted: TrustedDevices,
    ui: SharedDevicesUi,
    pairing: Option<PairWindow>,
    peer_pairing: bool,
    epoch: Option<String>,
    /// The device ID of the pair request the UI is deciding on, if any.
    waiting_decision: Option<String>,
    first_ctl_seen: bool,
    offer_seen: bool,
    answer_seen: bool,
    dtls_seen: bool,
    effects: Vec<Effect>,
    last_drop_log: Option<Instant>,
    dropped: u64,
}

impl HostProtocol {
    /// A damaged trust file is an error, never an empty replacement list. The UI receives the
    /// load error even when startup cannot proceed.
    pub fn open(
        identity: &crate::device_key::DeviceIdentity,
        origin: &str,
        ui: SharedDevicesUi,
    ) -> Result<Self> {
        let trusted = match trusted_devices::open_default() {
            Ok(trusted) => trusted,
            Err(error) => {
                show_on(&ui, DevicesEvent::SaveError(format!("{error:#}")));
                return Err(error).context("opening the trusted device list");
            }
        };
        Self::with_store(
            identity.to_phase2_identity(),
            origin,
            ui,
            trusted,
            Box::new(OsRandomness),
        )
    }

    pub(crate) fn with_store(
        identity: bp_device_key::DeviceIdentity,
        origin: &str,
        ui: SharedDevicesUi,
        trusted: TrustedDevices,
        random: Box<dyn Randomness>,
    ) -> Result<Self> {
        let pc_id = identity.id();
        let session = Session::new(
            SessionConfig {
                identity,
                relay_origin: origin.to_owned(),
                pc_caps: vec!["control-aead-v1".into(), "dtls-bind-v1".into()],
            },
            random,
        )?;
        Ok(Self {
            session,
            pc_id,
            origin: origin.to_owned(),
            trusted,
            ui,
            pairing: None,
            peer_pairing: false,
            epoch: None,
            waiting_decision: None,
            first_ctl_seen: false,
            offer_seen: false,
            answer_seen: false,
            dtls_seen: false,
            effects: Vec::new(),
            last_drop_log: None,
            dropped: 0,
        })
    }

    pub fn admit(&self) -> Value {
        admit_message(
            self.trusted
                .devices()
                .iter()
                .map(|device| device.id.clone()),
        )
    }

    /// Kept as an explicit name for the signaling registration call site.
    pub fn admit_message(&self) -> Value {
        self.admit()
    }

    pub fn application_ready(&self) -> bool {
        !self.peer_pairing && self.session.media_ready()
    }

    /// The exact relay epoch, available only for an authenticated normal viewer. Pairing never
    /// obtains data sockets. Callers must also check `application_ready` before enabling them.
    pub fn viewer_epoch(&self) -> Option<&str> {
        (self.session.is_established() && !self.peer_pairing)
            .then_some(self.epoch.as_deref())
            .flatten()
    }

    /// A new relay event always fences the old producer, including a warm WSS/RTC connection.
    pub fn peer_joined(&mut self, text: &str) -> Result<()> {
        self.expire_pairing(Instant::now());
        let peer = match dk_session::parse_peer_joined(text) {
            Ok(peer) => peer,
            Err(error) => {
                self.terminate();
                return Err(error.into());
            }
        };
        // Stop before peer_joined: Session's PeerReset event otherwise describes the *old*
        // session while its state already contains the new peer.
        if self.peer_pairing {
            self.close_pairing(DevicesEvent::PairStopped, "pairing peer replaced");
        }
        self.reset_peer();
        self.effects.push(Effect::Park);
        let code = if peer.pairing {
            match self.pairing.as_mut().filter(|window| !window.claimed) {
                Some(window) => {
                    window.claimed = true;
                    Some(PairingCode::parse(window.code.as_str())?)
                }
                None => {
                    self.record_drop("pairing peer without an unused pairing window");
                    self.effects.push(Effect::Send(pair_close_message()));
                    self.effects.push(Effect::Ui(DevicesEvent::PairFailed));
                    return Ok(());
                }
            }
        } else {
            None
        };
        let trusted = self.trusted_snapshot()?;
        self.epoch = Some(peer.media_epoch.clone());
        self.peer_pairing = peer.pairing;
        let events = self.session.peer_joined(peer, trusted, code)?;
        self.consume(events, false)?;
        self.queue_list();
        Ok(())
    }

    pub fn peer_left(&mut self) {
        if self.peer_pairing {
            self.close_pairing(DevicesEvent::PairStopped, "pairing peer left");
        } else {
            self.reset_peer();
            self.effects.push(Effect::Park);
            self.queue_list();
        }
    }

    /// Relay-side brute-force protection closed the code early. It cannot end a normal viewer.
    pub fn pair_closed(&mut self) {
        if self.pairing.is_some() || self.peer_pairing {
            self.close_pairing(DevicesEvent::PairStopped, "relay closed pairing");
        }
    }

    /// Ends all local access on a signaling failure; never leaves a reusable pairing code.
    pub fn terminate(&mut self) {
        self.pairing = None;
        self.reset_peer();
        self.effects.push(Effect::Park);
        self.queue_list();
    }

    /// Auth/pair/ctl only. Relay-generated controls are handled by Signaling. Endpoint plaintext
    /// and all ws binary frames are dropped, with one debug diagnostic per second at most.
    pub fn receive(&mut self, text: &str) -> Result<Vec<Inbound>> {
        self.expire_pairing(Instant::now());
        if text.len() > MAX_WIRE_BYTES {
            self.record_drop("oversized device-key message");
            self.terminate();
            return Ok(Vec::new());
        }
        let is_ctl = serde_json::from_str::<Value>(text)
            .ok()
            .is_some_and(|value| value["type"] == "ctl");
        let established_ctl = is_ctl && self.session.is_established();
        let first_ctl = established_ctl && !self.first_ctl_seen;
        let events = match self.session.on_text(text) {
            Ok(events) => events,
            Err(error) => {
                self.terminate();
                return Err(error.into());
            }
        };
        let incoming = self.consume(events, first_ctl)?;
        if established_ctl {
            self.first_ctl_seen = true;
        }
        Ok(incoming)
    }

    pub fn drop_plaintext(&mut self) {
        self.record_drop("plaintext binary on signaling socket");
    }

    /// Returns no envelope while no normal authenticated session exists. SDP and ICE may flow
    /// before DTLS binding; clipboard, cursor, file and application traffic wait for the bind.
    pub fn seal_text(&mut self, text: &str) -> Result<Option<Value>> {
        if !self.session.is_established() || self.peer_pairing {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(text).context("serializing an endpoint message")?;
        let kind = value["type"]
            .as_str()
            .context("endpoint message has no type")?;
        if !self.application_ready()
            && !matches!(kind, "answer" | "ice" | "caps" | "error" | "ping" | "pong")
        {
            return Ok(None);
        }
        match self.session.seal_text(kind, text) {
            Ok(envelope) => Ok(Some(serde_json::from_str(&envelope)?)),
            Err(error) => {
                self.terminate();
                Err(error.into())
            }
        }
    }

    pub fn seal_cursor(&mut self, bytes: &[u8]) -> Result<Option<Value>> {
        if !self.application_ready() {
            return Ok(None);
        }
        match self.session.seal_binary("cursor", bytes) {
            Ok(envelope) => Ok(Some(serde_json::from_str(&envelope)?)),
            Err(error) => {
                self.terminate();
                Err(error.into())
            }
        }
    }

    /// Called before sealing the answer so auth-bind signs the exact SDP fingerprints sent.
    pub fn local_answer(&mut self, sdp: &str) -> Result<bool> {
        if !self.session.is_established() || !self.offer_seen || self.answer_seen {
            return Ok(false);
        }
        match self.session.webrtc_answer(sdp) {
            Ok(events) => {
                // A non-empty result is the dtls-bind-required error and the end of the session.
                if !events.is_empty() {
                    self.consume(events, false)?;
                    return Ok(false);
                }
            }
            Err(error) => {
                self.terminate();
                return Err(error.into());
            }
        }
        self.answer_seen = true;
        Ok(true)
    }

    /// ICE can reconnect on the same DTLS peer. Send auth-bind exactly once per handshake.
    pub fn dtls_connected(&mut self) -> Result<()> {
        if self.dtls_seen || !self.session.is_established() || !self.offer_seen {
            return Ok(());
        }
        let events = match self.session.dtls_connected() {
            Ok(events) => events,
            Err(error) => {
                self.terminate();
                return Err(error.into());
            }
        };
        self.dtls_seen = true;
        self.consume(events, false)?;
        Ok(())
    }

    /// Bounded UI drain. No UI mutex is held during disk or cryptographic work.
    pub fn poll(&mut self) -> Result<()> {
        let now = Instant::now();
        self.expire_pairing(now);
        let commands: Vec<_> = self
            .ui
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .commands
            .try_iter()
            .take(MAX_COMMANDS_PER_POLL)
            .collect();
        for command in commands {
            self.command(command, now)?;
        }
        Ok(())
    }

    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    pub fn show(&self, event: DevicesEvent) {
        show_on(&self.ui, event);
    }

    fn trusted_snapshot(&self) -> Result<Vec<Trusted>> {
        self.trusted
            .devices()
            .iter()
            .map(|device| {
                Ok(Trusted {
                    id: bp_device_key::DeviceId::parse(&device.id)?,
                    public: URL_SAFE_NO_PAD
                        .decode(&device.public)?
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("trusted public key is not 32 bytes"))?,
                })
            })
            .collect()
    }

    /// A viewer `media-control` start for WSS protocol 4 with this session's epoch.
    fn is_wss_start(&self, json: &str) -> bool {
        let Ok(value) = serde_json::from_str::<Value>(json) else {
            return false;
        };
        value["action"] == "start"
            && value["protocol"].as_u64() == Some(4)
            && value["epoch"].as_str().is_some()
            && value["epoch"].as_str() == self.epoch.as_deref()
    }

    fn reset_peer(&mut self) {
        self.session.stop(End::Stopped("viewer reset"));
        self.epoch = None;
        self.peer_pairing = false;
        self.waiting_decision = None;
        self.first_ctl_seen = false;
        self.offer_seen = false;
        self.answer_seen = false;
        self.dtls_seen = false;
    }

    fn close_pairing(&mut self, ui: DevicesEvent, reason: &'static str) {
        self.pairing = None;
        self.effects.push(Effect::Send(pair_close_message()));
        if self.peer_pairing {
            self.session.stop(End::Stopped(reason));
            self.reset_peer();
            self.effects.push(Effect::Park);
        }
        self.effects.push(Effect::Ui(ui));
        self.queue_list();
    }

    fn expire_pairing(&mut self, now: Instant) {
        if self
            .pairing
            .as_ref()
            .is_some_and(|window| now >= window.deadline)
        {
            self.close_pairing(DevicesEvent::PairExpired, "pairing expired");
        }
    }

    fn start_pairing(&mut self, code: PairingCode, now: Instant, wall_now: SystemTime) {
        if self.pairing.is_some() || self.peer_pairing {
            self.close_pairing(DevicesEvent::PairStopped, "pairing restarted");
        }
        let event = DevicesEvent::PairWaiting {
            code_display: code.display().to_string(),
            qr_uri: pair_qr_uri(&self.origin, &self.pc_id.to_string(), code.as_str()),
            pc_id: self.pc_id.to_string(),
            expires_at: wall_now + PAIR_WINDOW,
        };
        self.effects
            .push(Effect::Send(pair_open_message(&code, &self.pc_id)));
        self.pairing = Some(PairWindow {
            code,
            deadline: now + PAIR_WINDOW,
            claimed: false,
        });
        self.effects.push(Effect::Ui(event));
    }

    fn command(&mut self, command: DevicesCommand, now: Instant) -> Result<()> {
        match command {
            DevicesCommand::Open => self.queue_list(),
            DevicesCommand::StartPairing => {
                let code = PairingCode::generate()?;
                self.start_pairing(code, now, SystemTime::now());
            }
            DevicesCommand::CancelPairing => {
                if self.pairing.is_some() || self.peer_pairing {
                    self.close_pairing(DevicesEvent::PairStopped, "pairing cancelled");
                }
            }
            // A decision counts only for the request the window was showing: a click that
            // arrives after that request ended, or names another device, is ignored.
            DevicesCommand::Allow(id) if self.waiting_decision.as_deref() == Some(id.as_str()) => {
                let events = self.session.allow()?;
                self.waiting_decision = None;
                self.consume(events, false)?;
            }
            DevicesCommand::Deny(id) if self.waiting_decision.as_deref() == Some(id.as_str()) => {
                let events = self.session.deny()?;
                self.waiting_decision = None;
                self.consume(events, false)?;
            }
            DevicesCommand::Allow(id) | DevicesCommand::Deny(id) => {
                tracing::info!(
                    device = %id,
                    pending = ?self.waiting_decision,
                    "ignored a pairing decision for a request that is not pending"
                );
                self.record_drop("stale pairing decision");
            }
            DevicesCommand::Revoke(id) => {
                let connected = self.session.viewer().is_some_and(|viewer| viewer.id == id);
                match self.trusted.revoke(&id) {
                    Ok(true) => {
                        // Persistence commits before either the live gate or relay admit changes.
                        // An in-flight handshake holds a trust snapshot. Invalidate it too,
                        // so auth-finish cannot establish a key revoked after auth-hello.
                        if self.peer_pairing {
                            self.close_pairing(
                                DevicesEvent::PairStopped,
                                "pairing cancelled by revoke",
                            );
                        } else if connected || !self.session.is_established() {
                            self.reset_peer();
                            self.effects.push(Effect::Park);
                        }
                        self.effects.push(Effect::Send(self.admit()));
                        self.queue_list();
                    }
                    Ok(false) => self.queue_list(),
                    Err(error) => self.save_error(error),
                }
            }
        }
        Ok(())
    }

    fn consume(&mut self, events: Vec<Event>, first_ctl: bool) -> Result<Vec<Inbound>> {
        let mut incoming = Vec::new();
        for event in events {
            match event {
                Event::Send(text) => self
                    .effects
                    .push(Effect::Send(serde_json::from_str(&text)?)),
                Event::Established(viewer) => {
                    match self.trusted.touch(&viewer.id, SystemTime::now()) {
                        Ok(true) => self.effects.push(Effect::Send(self.admit())),
                        Ok(false) => {}
                        Err(error) => self.save_error(error),
                    }
                    self.queue_list();
                }
                Event::PairRequest(request) => {
                    self.waiting_decision = Some(request.viewer_id.clone());
                    let name = self
                        .trusted
                        .get(&request.viewer_id)
                        .map(|device| device.name.clone())
                        .unwrap_or_else(|| UNNAMED_DEVICE.into());
                    self.effects.push(Effect::Ui(DevicesEvent::PairRequest {
                        id: request.viewer_id,
                        name,
                        sas_display: request.sas_display,
                    }));
                }
                Event::PairAllowed(viewer) => {
                    let saved = self.trusted.add(
                        &viewer.id,
                        &URL_SAFE_NO_PAD.encode(viewer.public),
                        UNNAMED_DEVICE,
                        SystemTime::now(),
                    );
                    // The durable add must happen before pair_saved can emit pair-done.
                    let events = self.session.pair_saved(saved.is_ok())?;
                    self.consume(events, false)?;
                    match saved {
                        Ok(device) => self.effects.push(Effect::Ui(DevicesEvent::PairDone {
                            id: device.id,
                            name: device.name,
                        })),
                        Err(error) => self.save_error(error),
                    }
                }
                Event::Message { kind, json } => {
                    if kind == "ping" {
                        if let Some(envelope) = self.seal_text(r#"{"type":"pong"}"#)? {
                            self.effects.push(Effect::Send(envelope));
                        }
                        continue;
                    }
                    if kind == "pong" {
                        continue;
                    }
                    if kind == "bye" {
                        self.end_peer(End::Stopped("viewer bye"));
                        incoming.clear();
                        continue;
                    }
                    if kind == "caps" {
                        if first_ctl {
                            self.update_name(&json);
                        }
                        continue;
                    }
                    // Decision 24 (see dk_session::Mode): a WSS media start for protocol 4 and this
                    // session's epoch latches WSS, from Unset or after WebRTC (the fallback).
                    if kind == "media-control" && self.is_wss_start(&json) {
                        self.session.latch_wss()?;
                        incoming.push(Inbound::Json(json));
                        continue;
                    }
                    if kind == "offer" {
                        // Clients send at most one offer per /ws, so a second one is dropped today.
                        // The session's clear-binds rule covers the first offer and any future
                        // accepted re-offer.
                        if self.offer_seen {
                            self.record_drop("repeated offer in one device-key session");
                            continue;
                        }
                        let value: Value = serde_json::from_str(&json)?;
                        let Some(sdp) = value["sdp"].as_str() else {
                            self.end_peer(End::Protocol("offer without an SDP"));
                            continue;
                        };
                        match self.session.webrtc_offer(sdp) {
                            Ok(events) => {
                                self.consume(events, false)?;
                                if self.session.is_established() {
                                    self.offer_seen = true;
                                    incoming.push(Inbound::Json(json));
                                }
                            }
                            Err(_) => self.end_peer(End::Protocol("invalid WebRTC offer")),
                        }
                    } else if self.application_ready() || kind == "ice" {
                        incoming.push(Inbound::Json(json));
                    } else {
                        self.record_drop(
                            "endpoint message before the transport latch or the DTLS bind",
                        );
                    }
                }
                Event::Input(bytes) => {
                    if self.application_ready() {
                        incoming.push(Inbound::Input(bytes));
                    } else {
                        self.record_drop("input before the transport latch or the DTLS bind");
                    }
                }
                Event::Bound => self.effects.push(Effect::Bound),
                Event::Ignored(reason) => self.record_drop(reason),
                Event::Ended(reason) => {
                    self.ended(reason);
                    incoming.clear();
                }
            }
        }
        Ok(incoming)
    }

    fn end_peer(&mut self, reason: End) {
        self.session.stop(reason);
        self.ended(reason);
    }

    fn ended(&mut self, reason: End) {
        match reason {
            End::Protocol(reason) | End::Auth(reason) | End::Ctl(reason) | End::Refused(reason) => {
                self.record_drop(reason)
            }
            End::DtlsBindRequired => {
                self.record_drop("DTLS bind impossible: no dtls-bind-v1 or no sha-256 fingerprint")
            }
            End::PairCodeMismatch => self.record_drop("pair proof verification failed"),
            _ => {}
        }
        let pairing = self.peer_pairing;
        self.reset_peer();
        if pairing {
            self.pairing = None;
            if reason == End::PairFinished {
                self.effects.push(Effect::Send(self.admit()));
            }
            self.effects.push(Effect::Send(pair_close_message()));
            match reason {
                End::PairFinished => {}
                End::PairDenied => self.effects.push(Effect::Ui(DevicesEvent::PairDenied)),
                _ => self.effects.push(Effect::Ui(DevicesEvent::PairFailed)),
            }
        }
        self.effects.push(Effect::Park);
        self.queue_list();
    }

    fn update_name(&mut self, text: &str) {
        let Some(id) = self.session.viewer().map(|viewer| viewer.id.clone()) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let Some(name) = value["name"].as_str() else {
            return;
        };
        // TrustedDevices owns sanitisation (control stripping, trimming and 64-character cap)
        // and commits before replacing its in-memory value.
        match self.trusted.update_name(&id, name) {
            Ok(true) => {
                self.effects.push(Effect::Send(self.admit()));
                self.queue_list();
            }
            Ok(false) => self.queue_list(),
            Err(error) => self.save_error(error),
        }
    }

    fn queue_list(&mut self) {
        let current = self.session.viewer().map(|viewer| viewer.id.as_str());
        self.effects.push(Effect::Ui(DevicesEvent::List(
            self.trusted
                .devices()
                .iter()
                .map(|device| DeviceRow {
                    id: device.id.clone(),
                    name: device.name.clone(),
                    last_seen: Some(device.last_seen.clone()),
                    connected: current == Some(device.id.as_str()),
                })
                .collect(),
        )));
    }

    fn save_error(&mut self, error: anyhow::Error) {
        self.effects
            .push(Effect::Ui(DevicesEvent::SaveError(format!("{error:#}"))));
    }

    fn record_drop(&mut self, reason: &'static str) {
        self.dropped = self.dropped.saturating_add(1);
        let now = Instant::now();
        if self
            .last_drop_log
            .is_none_or(|last| now.duration_since(last) >= DROP_LOG_INTERVAL)
        {
            tracing::debug!(
                reason,
                dropped = self.dropped,
                "device-key messages dropped"
            );
            self.last_drop_log = Some(now);
            self.dropped = 0;
        }
    }
}

fn show_on(ui: &SharedDevicesUi, event: DevicesEvent) {
    let sender = ui
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .events
        .clone();
    let _ = sender.send(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bp_device_key::phase2::{CtlOpener, CtlSealer};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    const ORIGIN: &str = "https://example.test";
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "bubbleplay-dk-host-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            // Fail on a stale test directory rather than using its contents as trust.
            std::fs::create_dir(&path).expect("fresh test directory");
            Self(path)
        }
        fn list(&self) -> PathBuf {
            self.0.join("trusted.json")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../fixtures/device-keys-contract.json")).unwrap()
    }
    fn string(value: &Value) -> &str {
        value.as_str().expect("fixture string")
    }
    fn hex32(value: &Value) -> [u8; 32] {
        let text = string(value);
        std::array::from_fn(|index| {
            u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).unwrap()
        })
    }
    fn b64_32(value: &Value) -> [u8; 32] {
        URL_SAFE_NO_PAD
            .decode(string(value))
            .unwrap()
            .try_into()
            .unwrap()
    }
    struct FixtureRandom;
    impl Randomness for FixtureRandom {
        fn nonce(&mut self) -> std::result::Result<[u8; 32], dk_session::SessionError> {
            Ok(b64_32(&fixture()["handshake"]["pc"]["nonce"]))
        }
        fn ephemeral(&mut self) -> std::result::Result<[u8; 32], dk_session::SessionError> {
            Ok(hex32(&fixture()["handshake"]["pc"]["ephemeralPrivateHex"]))
        }
    }
    fn controller(path: &Path, paired: bool) -> (HostProtocol, crate::devices_ui_api::UiEnds) {
        controller_at(path, paired, SystemTime::now())
    }
    fn controller_at(
        path: &Path,
        paired: bool,
        last_seen: SystemTime,
    ) -> (HostProtocol, crate::devices_ui_api::UiEnds) {
        let f = fixture();
        let viewer = &f["handshake"]["viewer"];
        let mut trusted = trusted_devices::load(path).unwrap();
        if paired {
            trusted
                .add(
                    string(&viewer["id"]),
                    string(&viewer["public"]),
                    "Old name",
                    last_seen,
                )
                .unwrap();
        }
        let (host, ui) = crate::devices_ui_api::channel();
        let host = HostProtocol::with_store(
            bp_device_key::DeviceIdentity::from_seed(&hex32(&f["handshake"]["pc"]["seedHex"])),
            ORIGIN,
            Arc::new(Mutex::new(host)),
            trusted,
            Box::new(FixtureRandom),
        )
        .unwrap();
        (host, ui)
    }
    fn joined(pairing: bool) -> String {
        json!({"type":"peer-joined", "mediaEpoch":fixture()["handshake"]["sessionEpoch"], "pairing":pairing}).to_string()
    }
    /// The device-key handshake only: the transport mode is still unset.
    fn handshake(host: &mut HostProtocol) {
        let f = fixture();
        host.peer_joined(&joined(false)).unwrap();
        host.receive(&f["handshake"]["messages"]["authHello"].to_string())
            .unwrap();
        host.receive(&f["handshake"]["messages"]["authFinish"].to_string())
            .unwrap();
        assert!(host.viewer_epoch().is_some());
        assert!(!host.application_ready());
    }
    /// The handshake plus a WSS latch, so application traffic flows.
    fn establish(host: &mut HostProtocol) {
        handshake(host);
        host.session.latch_wss().unwrap();
        assert!(host.application_ready());
    }
    fn wss_start(epoch: &Value, protocol: u64) -> Value {
        json!({"type":"media-control","action":"start","protocol":protocol,"epoch":epoch})
    }
    fn webrtc_offer_and_binds(host: &mut HostProtocol, viewer: &mut CtlSealer) {
        let f = fixture();
        let offer = format!(
            "v=0\r\na=fingerprint:{}\r\n",
            string(&f["handshake"]["dtls"]["viewer"]["localFingerprint"])
        );
        assert_eq!(
            host.receive(&ctl_json(viewer, json!({"type":"offer","sdp":offer})))
                .unwrap()
                .len(),
            1
        );
        assert!(!host.application_ready());
        let answer = format!(
            "v=0\r\na=fingerprint:{}\r\n",
            string(&f["handshake"]["dtls"]["pc"]["localFingerprint"])
        );
        assert!(host.local_answer(&answer).unwrap());
        host.dtls_connected().unwrap();
        assert!(!host.application_ready());
        host.receive(&f["handshake"]["messages"]["authBind"].to_string())
            .unwrap();
        assert!(host.application_ready());
    }

    #[test]
    fn input_and_application_ctl_before_the_mode_latch_are_dropped() {
        let scratch = Scratch::new();
        let (mut host, _) = controller(&scratch.list(), true);
        handshake(&mut host);
        host.take_effects();
        let mut viewer = viewer_sealer();
        assert!(host
            .receive(&ctl(&mut viewer, "input", &[13, 0]))
            .unwrap()
            .is_empty());
        for kind in ["clipboard", "file-offer"] {
            assert!(host
                .receive(&ctl_json(&mut viewer, json!({"type":kind})))
                .unwrap()
                .is_empty());
        }
        // Dropped, not fatal: the viewer stays connected but not live.
        assert!(host.viewer_epoch().is_some());
        assert!(!host.application_ready());
        assert!(!host
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Park)));
    }

    #[test]
    fn input_flows_after_both_webrtc_binds() {
        let scratch = Scratch::new();
        let (mut host, _) = controller(&scratch.list(), true);
        handshake(&mut host);
        host.take_effects();
        let mut viewer = viewer_sealer();
        webrtc_offer_and_binds(&mut host, &mut viewer);
        assert_eq!(host.session.mode(), Some(dk_session::Mode::WebRtc));
        assert!(matches!(
            host.receive(&ctl(&mut viewer, "input", &[13, 0]))
                .unwrap()
                .as_slice(),
            [Inbound::Input(_)]
        ));
    }

    #[test]
    fn wss_start_relatches_after_webrtc_and_input_flows_without_binds() {
        let scratch = Scratch::new();
        let (mut host, _) = controller(&scratch.list(), true);
        handshake(&mut host);
        host.take_effects();
        let f = fixture();
        let mut viewer = viewer_sealer();
        webrtc_offer_and_binds(&mut host, &mut viewer);
        let start = wss_start(&f["handshake"]["sessionEpoch"], 4);
        assert!(matches!(
            host.receive(&ctl_json(&mut viewer, start))
                .unwrap()
                .as_slice(),
            [Inbound::Json(_)]
        ));
        assert_eq!(host.session.mode(), Some(dk_session::Mode::Wss));
        assert!(host.application_ready());
        assert!(matches!(
            host.receive(&ctl(&mut viewer, "input", &[13, 0]))
                .unwrap()
                .as_slice(),
            [Inbound::Input(_)]
        ));
        // The host still drops a second offer in the same device-key session.
        let offer = format!(
            "v=0\r\na=fingerprint:{}\r\n",
            string(&f["handshake"]["dtls"]["viewer"]["localFingerprint"])
        );
        assert!(host
            .receive(&ctl_json(&mut viewer, json!({"type":"offer","sdp":offer})))
            .unwrap()
            .is_empty());
        assert_eq!(host.session.mode(), Some(dk_session::Mode::Wss));
    }

    #[test]
    fn wss_start_from_unset_latches_only_with_protocol_4_and_this_epoch() {
        let scratch = Scratch::new();
        let (mut host, _) = controller(&scratch.list(), true);
        handshake(&mut host);
        host.take_effects();
        let f = fixture();
        let mut viewer = viewer_sealer();
        for start in [
            wss_start(&json!("not-this-epoch"), 4),
            wss_start(&f["handshake"]["sessionEpoch"], 3),
            json!({"type":"media-control","action":"start","protocol":4}),
            json!({"type":"media-control","action":"stop","protocol":4,"epoch":f["handshake"]["sessionEpoch"]}),
        ] {
            assert!(host
                .receive(&ctl_json(&mut viewer, start))
                .unwrap()
                .is_empty());
            assert_eq!(host.session.mode(), Some(dk_session::Mode::Unset));
            assert!(!host.application_ready());
        }
        let start = wss_start(&f["handshake"]["sessionEpoch"], 4);
        assert!(matches!(
            host.receive(&ctl_json(&mut viewer, start))
                .unwrap()
                .as_slice(),
            [Inbound::Json(_)]
        ));
        assert_eq!(host.session.mode(), Some(dk_session::Mode::Wss));
        assert!(host.application_ready());
    }
    fn begin_pair(host: &mut HostProtocol) {
        let f = fixture();
        host.start_pairing(
            PairingCode::parse(string(&f["phase2"]["pairing"]["code"])).unwrap(),
            Instant::now(),
            SystemTime::now(),
        );
        host.peer_joined(&joined(true)).unwrap();
        host.receive(&f["phase2"]["pairing"]["messages"]["authHello"].to_string())
            .unwrap();
        host.receive(&f["handshake"]["messages"]["authFinish"].to_string())
            .unwrap();
        assert!(!host.application_ready());
        assert!(host.viewer_epoch().is_none());
    }
    /// Runs pairing up to the PC-side decision and returns the pending request's device ID.
    fn request_pair(host: &mut HostProtocol) -> String {
        begin_pair(host);
        host.receive(&fixture()["phase2"]["pairing"]["messages"]["pairProofViewer"].to_string())
            .unwrap();
        assert!(!host.application_ready());
        host.waiting_decision.clone().expect("pending pair request")
    }
    fn sent(effects: &[Effect]) -> Vec<Value> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Send(value) => Some(value.clone()),
                _ => None,
            })
            .collect()
    }
    fn viewer_sealer() -> CtlSealer {
        CtlSealer::new(&hex32(
            &fixture()["handshake"]["controlKeys"]["viewerToPcHex"],
        ))
    }
    fn ctl(sealer: &mut CtlSealer, kind: &str, bytes: &[u8]) -> String {
        let (seq, data) = sealer.seal(kind, bytes).unwrap();
        json!({"type":"ctl", "kind":kind, "seq":seq, "data":URL_SAFE_NO_PAD.encode(data)})
            .to_string()
    }
    fn ctl_json(sealer: &mut CtlSealer, value: Value) -> String {
        ctl(sealer, string(&value["type"]), value.to_string().as_bytes())
    }

    #[test]
    fn relay_commands_match_the_shared_fixture() {
        let f = fixture();
        let pair = &f["phase2"]["pairing"];
        let pc = bp_device_key::DeviceId::parse(string(&pair["pcId"])).unwrap();
        let code = PairingCode::parse(string(&pair["code"])).unwrap();
        assert_eq!(pair_open_message(&code, &pc), pair["messages"]["pairOpen"]);
        assert_eq!(pair_close_message(), pair["messages"]["pairClose"]);
        assert_eq!(
            admit_message([string(&f["handshake"]["viewer"]["id"]).to_owned()]),
            f["phase2"]["admit"]
        );
    }

    #[test]
    fn allow_commits_trust_before_pair_done_admit_and_close() {
        let scratch = Scratch::new();
        let (mut host, ui) = controller(&scratch.list(), false);
        let id = request_pair(&mut host);
        let request_effects = host.take_effects();
        assert!(request_effects.iter().any(|effect| matches!(effect, Effect::Ui(DevicesEvent::PairRequest { sas_display, .. }) if sas_display == "830 687")));
        ui.commands.send(DevicesCommand::Allow(id)).unwrap();
        host.poll().unwrap();
        // The real persisted DPAPI file is readable before any queued wire effect is delivered.
        let saved = trusted_devices::load(scratch.list()).unwrap();
        assert_eq!(saved.devices().len(), 1);
        assert_eq!(saved.devices()[0].name, UNNAMED_DEVICE);
        let effects = host.take_effects();
        let messages = sent(&effects);
        assert_eq!(
            messages,
            vec![
                fixture()["phase2"]["pairing"]["messages"]["pairDone"].clone(),
                fixture()["phase2"]["admit"].clone(),
                pair_close_message()
            ]
        );
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::Ui(DevicesEvent::PairDone { .. }))));
        assert!(!host.application_ready());
        assert!(host.pairing.is_none());
        assert!(host.viewer_epoch().is_none());
        // Pair-done never promotes a pairing socket into an endpoint channel.
        assert!(host
            .receive(r#"{"type":"media-control","action":"start"}"#)
            .unwrap()
            .is_empty());
        assert!(host.seal_cursor(&[128, 0]).unwrap().is_none());
    }

    #[test]
    fn failed_pair_save_never_admits_or_sends_success() {
        let scratch = Scratch::new();
        let path = scratch.0.join("blocked").join("trusted.json");
        let (mut host, ui) = controller(&path, false);
        std::fs::write(scratch.0.join("blocked"), b"not a directory").unwrap();
        let id = request_pair(&mut host);
        host.take_effects();
        ui.commands.send(DevicesCommand::Allow(id)).unwrap();
        host.poll().unwrap();
        let effects = host.take_effects();
        assert_eq!(
            sent(&effects),
            vec![json!({"type":"pair-denied"}), pair_close_message()]
        );
        assert!(host.trusted.devices().is_empty());
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::Ui(DevicesEvent::SaveError(_)))));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::Ui(DevicesEvent::PairDone { .. }))));
        assert!(!host.application_ready());
    }

    #[test]
    fn wrong_code_shows_no_number_consumes_the_window_and_parks() {
        let scratch = Scratch::new();
        let (mut host, _) = controller(&scratch.list(), false);
        begin_pair(&mut host);
        host.take_effects();
        let f = fixture();
        let wrong =
            json!({"type":"pair-proof", "mac":f["phase2"]["pairing"]["wrongCodeViewerMac"]});
        host.receive(&wrong.to_string()).unwrap();
        let effects = host.take_effects();
        assert_eq!(sent(&effects), vec![pair_close_message()]);
        assert!(effects.iter().any(|effect| matches!(effect, Effect::Park)));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::Ui(DevicesEvent::PairFailed))));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::Ui(DevicesEvent::PairRequest { .. }))));
        assert!(host.pairing.is_none());
        assert!(host.trusted.devices().is_empty());
        // An unsolicited second pairing peer cannot reuse the consumed code.
        host.peer_joined(&joined(true)).unwrap();
        assert!(host
            .receive(&f["phase2"]["pairing"]["messages"]["authHello"].to_string())
            .unwrap()
            .is_empty());
        assert!(!sent(&host.take_effects())
            .iter()
            .any(|message| message["type"] == "auth-host"));
    }

    #[test]
    fn expiry_beats_a_queued_allow_and_cancel_preserves_a_normal_viewer() {
        let scratch = Scratch::new();
        let (mut host, ui) = controller(&scratch.list(), false);
        let id = request_pair(&mut host);
        let deadline = host.pairing.as_ref().unwrap().deadline;
        host.take_effects();
        host.expire_pairing(deadline);
        ui.commands.send(DevicesCommand::Allow(id)).unwrap();
        host.poll().unwrap();
        let effects = host.take_effects();
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::Ui(DevicesEvent::PairExpired))));
        assert!(!sent(&effects)
            .iter()
            .any(|message| message["type"] == "pair-done"));
        assert!(host.trusted.devices().is_empty());

        // Opening or closing the PC's code is independent of its existing normal viewer.
        let second = scratch.0.join("other.json");
        let (mut host, ui) = controller(&second, true);
        establish(&mut host);
        ui.commands.send(DevicesCommand::StartPairing).unwrap();
        host.poll().unwrap();
        ui.commands.send(DevicesCommand::CancelPairing).unwrap();
        host.poll().unwrap();
        assert!(host.application_ready());
        assert!(host.pairing.is_none());
    }

    #[test]
    fn revoke_closes_an_established_viewer_and_invalidates_a_handshake_snapshot() {
        let scratch = Scratch::new();
        let (mut host, ui) = controller(&scratch.list(), true);
        establish(&mut host);
        host.take_effects();
        let id = string(&fixture()["handshake"]["viewer"]["id"]).to_owned();
        ui.commands
            .send(DevicesCommand::Revoke(id.clone()))
            .unwrap();
        host.poll().unwrap();
        assert!(!host.application_ready());
        assert!(trusted_devices::load(scratch.list())
            .unwrap()
            .get(&id)
            .is_none());
        assert_eq!(sent(&host.take_effects()), vec![admit_message([])]);

        let (mut host, ui) = controller(&scratch.0.join("pending.json"), true);
        host.peer_joined(&joined(false)).unwrap();
        host.receive(&fixture()["handshake"]["messages"]["authHello"].to_string())
            .unwrap();
        ui.commands.send(DevicesCommand::Revoke(id)).unwrap();
        host.poll().unwrap();
        host.receive(&fixture()["handshake"]["messages"]["authFinish"].to_string())
            .unwrap();
        assert!(!host.application_ready());
        assert!(host.viewer_epoch().is_none());
    }

    #[test]
    fn failed_revoke_keeps_the_trust_file_and_live_session() {
        let scratch = Scratch::new();
        let (mut host, ui) = controller(&scratch.list(), true);
        establish(&mut host);
        host.take_effects();
        // The first touch is throttled, so this backup path does not exist yet. A directory
        // there forces the atomic commit to fail without corrupting the original trust file.
        std::fs::create_dir(scratch.0.join("trusted.json.bak")).unwrap();
        ui.commands
            .send(DevicesCommand::Revoke(
                string(&fixture()["handshake"]["viewer"]["id"]).to_owned(),
            ))
            .unwrap();
        host.poll().unwrap();
        let effects = host.take_effects();
        assert!(host.application_ready());
        assert_eq!(
            trusted_devices::load(scratch.list())
                .unwrap()
                .devices()
                .len(),
            1
        );
        assert!(sent(&effects).is_empty());
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::Ui(DevicesEvent::SaveError(_)))));
    }

    #[test]
    fn only_first_ctl_caps_updates_the_sanitized_device_name() {
        let scratch = Scratch::new();
        let (mut host, _) = controller(&scratch.list(), true);
        establish(&mut host);
        let mut viewer = viewer_sealer();
        let name = format!(" \u{0000}{}\n", "é".repeat(80));
        host.receive(&ctl_json(
            &mut viewer,
            json!({"type":"caps", "caps":[], "name":name}),
        ))
        .unwrap();
        assert_eq!(host.trusted.devices()[0].name, "é".repeat(64));
        host.receive(&ctl_json(
            &mut viewer,
            json!({"type":"caps", "caps":[], "name":"Replacement"}),
        ))
        .unwrap();
        assert_eq!(host.trusted.devices()[0].name, "é".repeat(64));
        assert_eq!(
            trusted_devices::load(scratch.list()).unwrap().devices()[0].name,
            "é".repeat(64)
        );

        // A viewer whose first ctl is another endpoint need not send caps. Later caps are not
        // allowed to rename it, and raw plaintext caps can never mutate the trust file.
        establish(&mut host);
        let mut viewer = viewer_sealer();
        let first = ctl_json(&mut viewer, json!({"type":"stats"}));
        assert_eq!(host.receive(&first).unwrap().len(), 1);
        host.receive(&ctl_json(
            &mut viewer,
            json!({"type":"caps", "caps":[], "name":"Late"}),
        ))
        .unwrap();
        host.receive(r#"{"type":"caps","name":"Plaintext"}"#)
            .unwrap();
        assert_eq!(host.trusted.devices()[0].name, "é".repeat(64));
    }

    #[test]
    fn webrtc_gates_input_clipboard_and_files_until_both_binds_and_binds_once() {
        let scratch = Scratch::new();
        let (mut host, _) = controller(&scratch.list(), true);
        establish(&mut host);
        host.take_effects();
        let f = fixture();
        let mut viewer = viewer_sealer();
        // Decision 23: lowercase hex (and an uppercase algorithm) in the SDPs is normalised before
        // signing and verifying, so the binds still match the fixture.
        let offer = format!(
            "v=0\r\na=fingerprint:{}\r\n",
            string(&f["handshake"]["dtls"]["viewer"]["localFingerprint"]).to_lowercase()
        );
        let inbound = host
            .receive(&ctl_json(&mut viewer, json!({"type":"offer","sdp":offer})))
            .unwrap();
        assert_eq!(inbound.len(), 1);
        assert!(!host.application_ready());
        for kind in ["clipboard", "file-offer", "media-control"] {
            assert!(host
                .receive(&ctl_json(&mut viewer, json!({"type":kind})))
                .unwrap()
                .is_empty());
            assert!(host
                .seal_text(&json!({"type":kind}).to_string())
                .unwrap()
                .is_none());
        }
        assert!(host
            .receive(&ctl(&mut viewer, "input", &[13, 0]))
            .unwrap()
            .is_empty());
        assert!(host.seal_cursor(&[128, 0]).unwrap().is_none());
        let answer = format!(
            "v=0\r\na=fingerprint:sha-1 {}\r\na=fingerprint:{}\r\n",
            ["0f"; 20].join(":"),
            string(&f["handshake"]["dtls"]["pc"]["localFingerprint"])
                .to_lowercase()
                .replace("sha-256", "SHA-256")
        );
        assert!(host.local_answer(&answer).unwrap());
        host.dtls_connected().unwrap();
        assert!(!host.application_ready());
        assert_eq!(
            sent(&host.take_effects()),
            vec![f["handshake"]["messages"]["authBindPc"].clone()]
        );
        host.dtls_connected().unwrap();
        assert!(sent(&host.take_effects()).is_empty());
        host.receive(&f["handshake"]["messages"]["authBind"].to_string())
            .unwrap();
        assert!(host.application_ready());
        assert!(host
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Bound)));
        assert!(matches!(
            host.receive(&ctl(&mut viewer, "input", &[13, 0]))
                .unwrap()
                .as_slice(),
            [Inbound::Input(_)]
        ));
    }

    #[test]
    fn webrtc_offer_without_sha256_fingerprint_ends_the_viewer() {
        let scratch = Scratch::new();
        let (mut host, _) = controller(&scratch.list(), true);
        establish(&mut host);
        host.take_effects();
        let mut viewer = viewer_sealer();
        let offer = format!("v=0\r\na=fingerprint:sha-1 {}\r\n", ["0F"; 20].join(":"));
        let inbound = host
            .receive(&ctl_json(&mut viewer, json!({"type":"offer","sdp":offer})))
            .unwrap();
        assert!(inbound.is_empty());
        assert!(!host.application_ready());
        assert!(host.viewer_epoch().is_none());
        let effects = host.take_effects();
        // The ctl error goes out before the viewer is parked.
        let messages = sent(&effects);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["type"], "ctl");
        assert!(effects.iter().any(|effect| matches!(effect, Effect::Park)));
    }

    #[test]
    fn peer_replacement_fences_old_channels_and_preserves_opaque_epoch_bytes() {
        let scratch = Scratch::new();
        let (mut host, _) = controller(&scratch.list(), true);
        establish(&mut host);
        assert_eq!(
            host.viewer_epoch(),
            Some(string(&fixture()["handshake"]["sessionEpoch"]))
        );
        host.take_effects();
        let opaque = "1a2b3c4d5e6f78901234567890abcdef0";
        host.peer_joined(
            &json!({"type":"peer-joined","mediaEpoch":opaque,"pairing":false}).to_string(),
        )
        .unwrap();
        assert_eq!(host.epoch.as_deref(), Some(opaque));
        assert!(host.viewer_epoch().is_none());
        assert!(!host.application_ready());
        assert!(matches!(host.take_effects().first(), Some(Effect::Park)));
        assert!(host
            .seal_text(r#"{"type":"clipboard","text":"secret"}"#)
            .unwrap()
            .is_none());
        assert!(host.seal_cursor(&[128]).unwrap().is_none());
        let mut viewer = viewer_sealer();
        assert!(host
            .receive(&ctl(&mut viewer, "input", &[13, 0]))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn host_caps_are_first_and_plaintext_drops_do_not_consume_ctl_sequence() {
        let scratch = Scratch::new();
        let (mut host, _) = controller(&scratch.list(), true);
        establish(&mut host);
        let effects = host.take_effects();
        let messages = sent(&effects);
        assert_eq!(messages[0], fixture()["handshake"]["messages"]["authHost"]);
        assert_eq!(messages[1]["kind"], "caps");
        assert_eq!(messages[1]["seq"], 0);
        host.receive(r#"{"type":"clipboard","text":"untrusted"}"#)
            .unwrap();
        for _ in 0..100 {
            host.drop_plaintext();
        }
        assert!(host.application_ready());
        let envelope = host
            .seal_text(r#"{"type":"clipboard","text":"fresh"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(envelope["seq"], 1);
        let mut opener = CtlOpener::new(&hex32(
            &fixture()["handshake"]["controlKeys"]["pcToViewerHex"],
        ));
        opener
            .open(
                "caps",
                0,
                &URL_SAFE_NO_PAD
                    .decode(string(&messages[1]["data"]))
                    .unwrap(),
            )
            .unwrap();
        let plaintext = opener
            .open(
                "clipboard",
                1,
                &URL_SAFE_NO_PAD.decode(string(&envelope["data"])).unwrap(),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&plaintext).unwrap(),
            json!({"type":"clipboard","text":"fresh"})
        );
    }

    #[test]
    fn last_seen_and_name_commits_republish_admission() {
        let scratch = Scratch::new();
        let (mut host, _) = controller_at(&scratch.list(), true, SystemTime::UNIX_EPOCH);
        establish(&mut host);
        let messages = sent(&host.take_effects());
        assert_eq!(messages.last(), Some(&fixture()["phase2"]["admit"]));
        assert_ne!(host.trusted.devices()[0].last_seen, "1970-01-01T00:00:00Z");
        let mut viewer = viewer_sealer();
        host.receive(&ctl_json(
            &mut viewer,
            json!({"type":"caps", "caps":[], "name":"Phone"}),
        ))
        .unwrap();
        assert_eq!(
            sent(&host.take_effects()),
            vec![fixture()["phase2"]["admit"].clone()]
        );
        assert_eq!(
            trusted_devices::load(scratch.list()).unwrap().devices()[0].name,
            "Phone"
        );
    }

    #[test]
    fn ctl_ping_and_pong_stay_encrypted_and_bye_parks_the_peer() {
        let scratch = Scratch::new();
        let (mut host, _) = controller(&scratch.list(), true);
        establish(&mut host);
        let initial = sent(&host.take_effects());
        let caps = initial
            .iter()
            .find(|message| message["kind"] == "caps")
            .unwrap();
        let mut viewer = viewer_sealer();
        assert!(host
            .receive(&ctl_json(&mut viewer, json!({"type":"ping"})))
            .unwrap()
            .is_empty());
        let messages = sent(&host.take_effects());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["type"], "ctl");
        assert_eq!(messages[0]["kind"], "pong");
        assert_eq!(messages[0]["seq"], 1);
        // Open seq zero as well: ping replies share the normal authenticated ctl counter.
        let mut opener = CtlOpener::new(&hex32(
            &fixture()["handshake"]["controlKeys"]["pcToViewerHex"],
        ));
        opener
            .open(
                "caps",
                0,
                &URL_SAFE_NO_PAD.decode(string(&caps["data"])).unwrap(),
            )
            .unwrap();
        let plaintext = opener
            .open(
                "pong",
                1,
                &URL_SAFE_NO_PAD
                    .decode(string(&messages[0]["data"]))
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&plaintext).unwrap(),
            json!({"type":"pong"})
        );
        assert!(host
            .receive(&ctl_json(&mut viewer, json!({"type":"pong"})))
            .unwrap()
            .is_empty());
        assert!(host.take_effects().is_empty());
        assert!(host
            .receive(&ctl_json(&mut viewer, json!({"type":"bye"})))
            .unwrap()
            .is_empty());
        assert!(!host.application_ready());
        assert!(host
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Park)));
    }

    #[test]
    fn denied_or_relay_stopped_pairing_never_adds_trust() {
        let scratch = Scratch::new();
        let (mut host, ui) = controller(&scratch.list(), false);
        let id = request_pair(&mut host);
        host.take_effects();
        ui.commands.send(DevicesCommand::Deny(id)).unwrap();
        host.poll().unwrap();
        let effects = host.take_effects();
        assert_eq!(
            sent(&effects),
            vec![json!({"type":"pair-denied"}), pair_close_message()]
        );
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::Ui(DevicesEvent::PairDenied))));
        assert!(host.trusted.devices().is_empty());
        request_pair(&mut host);
        host.take_effects();
        host.pair_closed();
        let effects = host.take_effects();
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::Ui(DevicesEvent::PairStopped))));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::Ui(DevicesEvent::PairFailed))));
        assert!(host.pairing.is_none());
        assert!(host.waiting_decision.is_none());
        assert!(host.trusted.devices().is_empty());
    }

    #[test]
    fn revoke_during_pairing_consumes_the_claimed_window() {
        let scratch = Scratch::new();
        // A different device is already paired. Revoking it during this new pairing must
        // neither retain a claimed window nor leave an actionable stale comparison number.
        let (mut host, ui) = controller(&scratch.list(), false);
        let other = bp_device_key::DeviceIdentity::from_seed(&[7; 32]);
        let other_id = other.id().to_string();
        host.trusted
            .add(
                &other_id,
                &URL_SAFE_NO_PAD.encode(other.public_key()),
                "Other",
                SystemTime::now(),
            )
            .unwrap();
        let id = request_pair(&mut host);
        host.take_effects();
        ui.commands.send(DevicesCommand::Revoke(other_id)).unwrap();
        host.poll().unwrap();
        assert!(host.pairing.is_none());
        assert!(!host.peer_pairing);
        assert!(host.waiting_decision.is_none());
        ui.commands.send(DevicesCommand::Allow(id)).unwrap();
        host.poll().unwrap();
        let effects = host.take_effects();
        assert!(!sent(&effects)
            .iter()
            .any(|message| message["type"] == "pair-done"));
        assert!(host.trusted.devices().is_empty());
    }

    #[test]
    fn a_decision_counts_only_for_the_pending_request() {
        let scratch = Scratch::new();
        let (mut host, ui) = controller(&scratch.list(), false);
        let id = request_pair(&mut host);
        host.take_effects();
        // A click left over from another request (another device ID) is ignored, both ways.
        let other = "0000-0000-0000-0000".to_owned();
        assert_ne!(other, id);
        ui.commands
            .send(DevicesCommand::Allow(other.clone()))
            .unwrap();
        ui.commands.send(DevicesCommand::Deny(other)).unwrap();
        host.poll().unwrap();
        let effects = host.take_effects();
        assert!(sent(&effects).is_empty());
        assert_eq!(host.waiting_decision.as_deref(), Some(id.as_str()));
        assert!(host.trusted.devices().is_empty());
        // The decision for the request on screen still goes through.
        ui.commands.send(DevicesCommand::Allow(id.clone())).unwrap();
        host.poll().unwrap();
        let effects = host.take_effects();
        assert!(sent(&effects)
            .iter()
            .any(|message| message["type"] == "pair-done"));
        assert_eq!(host.trusted.devices().len(), 1);
        assert!(host.waiting_decision.is_none());
        // A second click for the same, now decided, request changes nothing.
        ui.commands.send(DevicesCommand::Deny(id)).unwrap();
        host.poll().unwrap();
        assert!(sent(&host.take_effects()).is_empty());
        assert_eq!(host.trusted.devices().len(), 1);
    }
}
