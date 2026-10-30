//! The real [`Backend`] for the Flutter app on Windows: the viewer key and PC list in
//! `%LOCALAPPDATA%\BubblePlay\viewer`, and pairing over the relay.
//!
//! `act` never blocks: key work is quick and runs inline; pairing runs on its own thread and
//! reports through the shared state and the notifier. M1 stops at the session boundary:
//! Connect moves to a placeholder session (no media), and the This PC section is not backed
//! yet (`host` stays `None`; M3 backs it with the host's named pipe).

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use anyhow::Result;
use bp_device_key::{phase2::PairingCode, DeviceId, DeviceIdentity, DeviceKeyError, KeyFile};
use bubbleplay_client::model::{
    Action, Answerer, AppState, Backend, EnrollStep, Flow, Notifier, Outcome, PcKind, RemotePc,
    Session, StreamMode,
};
use bubbleplay_client::net::{self, Codec, NetConfig, ViewerAuth};
use bubbleplay_client::store::key::{KeyStore, KEY_FILE_NAME};
use bubbleplay_client::store::pcs::PcStore;
use zeroize::Zeroizing;

use crate::link;

/// Where the key and the PC list live.
pub struct Paths {
    pub keys: KeyStore,
    pub pcs: PcStore,
}

impl Paths {
    /// `%LOCALAPPDATA%\BubblePlay\viewer`.
    pub fn default_location() -> Result<Self> {
        Ok(Self {
            keys: KeyStore::default_location()?,
            pcs: PcStore::default_location()?,
        })
    }

    pub fn in_dir(dir: &std::path::Path) -> Self {
        Self {
            keys: KeyStore::in_dir(dir),
            pcs: PcStore::in_dir(dir),
        }
    }
}

struct Inner {
    state: AppState,
    /// The unlocked viewer key, once there is one.
    identity: Option<DeviceIdentity>,
    /// The stored key is protected by a passphrase and not unlocked yet.
    stored_locked: Option<KeyFile>,
    /// An imported file with a passphrase, waiting for it.
    import_pending: Option<KeyFile>,
    /// Bumps when a pairing is cancelled, so a late result from its thread is dropped.
    pair_generation: u64,
    /// When the pairing wait began, for `waiting_secs`.
    pair_started: Option<Instant>,
}

pub struct AppBackend {
    paths: Arc<Paths>,
    inner: Arc<Mutex<Inner>>,
    notifier: Arc<Mutex<Option<Notifier>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn notify(notifier: &Mutex<Option<Notifier>>) {
    let callback = lock(notifier).clone();
    if let Some(callback) = callback {
        callback();
    }
}

fn computer_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "Windows viewer".to_owned())
}

impl AppBackend {
    pub fn new(paths: Paths) -> Self {
        let mut inner = Inner {
            state: AppState::default(),
            identity: None,
            stored_locked: None,
            import_pending: None,
            pair_generation: 0,
            pair_started: None,
        };
        inner.state.this_device.key_path = paths.keys.path().display().to_string();
        load_key(&paths, &mut inner);
        refresh_pcs(&paths, &mut inner);
        Self {
            paths: Arc::new(paths),
            inner: Arc::new(Mutex::new(inner)),
            notifier: Arc::new(Mutex::new(None)),
        }
    }

    /// The portable `.bpkey` text of the stored key, for the host to save where the user
    /// picks. Not part of the snapshot: the key text never rides in `AppState`.
    pub fn export_key(&self) -> Result<Zeroizing<String>> {
        self.paths.keys.export()
    }
}

fn refused(file_name: &str, reason: String) -> Flow {
    Flow::Enroll(EnrollStep::ImportRefused {
        file_name: file_name.to_owned(),
        reason,
    })
}

/// Reads the stored key into `inner`: a plain key is unlocked, a protected one waits for its
/// passphrase (`ImportUnlock` names the stored file), no key asks to enroll.
fn load_key(paths: &Paths, inner: &mut Inner) {
    inner.identity = None;
    inner.stored_locked = None;
    if !paths.keys.exists() {
        inner.state.this_device.id.clear();
        inner.state.flow = Some(Flow::Enroll(EnrollStep::Choose));
        return;
    }
    let file = match paths.keys.load() {
        Ok(file) => file,
        Err(error) => {
            inner.state.flow = Some(refused(
                KEY_FILE_NAME,
                format!("The stored key cannot be read: {error}"),
            ));
            return;
        }
    };
    inner.state.this_device.id = file.id().to_string();
    if file.is_protected() {
        inner.state.flow = Some(Flow::Enroll(EnrollStep::ImportUnlock {
            file_name: KEY_FILE_NAME.to_owned(),
            error: None,
        }));
        inner.stored_locked = Some(file);
        return;
    }
    match file.unlock(None) {
        Ok(signing) => {
            inner.identity = Some(DeviceIdentity::from_signing_key(signing));
            if matches!(inner.state.flow, Some(Flow::Enroll(_))) {
                inner.state.flow = None;
            }
        }
        Err(error) => {
            inner.state.flow = Some(refused(
                KEY_FILE_NAME,
                format!("The stored key cannot be opened: {error}"),
            ));
        }
    }
}

fn refresh_pcs(paths: &Paths, inner: &mut Inner) {
    let entries = paths.pcs.load().unwrap_or_default();
    let known: std::collections::HashMap<String, RemotePc> = inner
        .state
        .pcs
        .drain(..)
        .map(|pc| (pc.id.clone(), pc))
        .collect();
    inner.state.pcs = entries
        .into_iter()
        .map(|entry| {
            let before = known.get(&entry.id);
            RemotePc {
                id: entry.id,
                name: entry.name,
                kind: PcKind::Desktop,
                relay: entry.relay,
                // Presence is not asked yet: false means "not known to be online".
                online: false,
                last_mode: before.and_then(|pc| pc.last_mode),
                last_seen: before.and_then(|pc| pc.last_seen.clone()),
                allowed: "View and input".to_owned(),
                vouch_request: None,
                problem: None,
                default_monitor: None,
            }
        })
        .collect();
}

impl Backend for AppBackend {
    fn snapshot(&self) -> AppState {
        let inner = lock(&self.inner);
        let mut state = inner.state.clone();
        if let Some(started) = inner.pair_started {
            let secs = started.elapsed().as_secs() as u32;
            match &mut state.flow {
                Some(Flow::PairConfirm { waiting_secs, .. })
                | Some(Flow::Approval { waiting_secs, .. }) => *waiting_secs = secs,
                _ => {}
            }
        }
        state
    }

    fn act(&mut self, action: Action) {
        let changed = {
            let mut inner = lock(&self.inner);
            self.apply(&mut inner, action)
        };
        if changed {
            notify(&self.notifier);
        }
    }

    fn set_notifier(&mut self, notify: Notifier) {
        *lock(&self.notifier) = Some(notify);
    }
}

impl AppBackend {
    /// Applies one action; returns whether the snapshot may have changed.
    fn apply(&self, inner: &mut Inner, action: Action) -> bool {
        match action {
            Action::CreateKey => self.create_key(inner),
            Action::ImportKeyText { file_name, text } => self.import_key(inner, file_name, &text),
            Action::UnlockImport { passphrase } => {
                self.unlock_import(inner, Zeroizing::new(passphrase))
            }
            Action::CancelFlow => self.cancel_flow(inner),
            Action::StartPairing => {
                inner.state.flow = Some(Flow::PairEnter {
                    remote: false,
                    error: None,
                });
            }
            Action::StartRemotePairing => {
                inner.state.flow = Some(Flow::PairEnter {
                    remote: true,
                    error: None,
                });
            }
            Action::RequestRemotePairing { .. } => {
                inner.state.flow = Some(Flow::PairEnter {
                    remote: true,
                    error: Some("Remote pairing is not available in this version.".to_owned()),
                });
            }
            Action::SubmitPairing {
                link,
                pc_id,
                code,
                relay,
            } => self.submit_pairing(inner, link, pc_id, code, relay),
            Action::Forget { pc_id } => {
                let _ = self.paths.pcs.remove(&pc_id);
                refresh_pcs(&self.paths, inner);
            }
            Action::Rename { pc_id, name } => self.rename(inner, &pc_id, &name),
            Action::Connect { pc_id } => self.connect(inner, &pc_id),
            Action::Disconnect => inner.state.session = None,
            Action::SetMode(mode) => {
                if let Some(session) = &mut inner.state.session {
                    session.mode = mode;
                }
            }
            Action::SetQuality(quality) => {
                if let Some(session) = &mut inner.state.session {
                    session.quality = quality;
                }
            }
            Action::SetAudio(audio) => {
                if let Some(session) = &mut inner.state.session {
                    session.audio = audio;
                }
            }
            Action::SetMonitor(index) => {
                if let Some(session) = &mut inner.state.session {
                    if index < session.monitors.len() {
                        session.monitor = index;
                    }
                }
            }
            // Not backed in M1: retries need a real connect; the This PC actions need the
            // host pipe (M3); vouching and key replacement are not in the live client yet;
            // the host app picks key files and calls ImportKeyText / export_key itself.
            Action::Retry
            | Action::RetryWebSocket
            | Action::Vouch { .. }
            | Action::AnswerPairing { .. }
            | Action::Revoke { .. }
            | Action::SetMayVouch { .. }
            | Action::SetHostSetting(_)
            | Action::StartHosting
            | Action::StopHosting
            | Action::RestartHost
            | Action::RollBackHost
            | Action::OpenLogsFolder
            | Action::ShowPairDevice
            | Action::ReplaceKey
            | Action::ImportKey
            | Action::ExportKey => return false,
        }
        true
    }

    fn create_key(&self, inner: &mut Inner) {
        if self.paths.keys.exists() {
            return;
        }
        match self.paths.keys.create(&computer_name(), None) {
            Ok(_) => {
                load_key(&self.paths, inner);
                inner.state.flow = Some(Flow::Enroll(EnrollStep::Created));
            }
            Err(error) => {
                inner.state.flow = Some(refused(
                    "",
                    format!("The key could not be created: {error:#}"),
                ));
            }
        }
    }

    fn import_key(&self, inner: &mut Inner, file_name: String, text: &str) {
        if self.paths.keys.exists() {
            return;
        }
        let file = match KeyFile::parse(text) {
            Ok(file) => file,
            Err(DeviceKeyError::UnsupportedFormat) => {
                inner.state.flow = Some(refused(
                    &file_name,
                    "This is not a BubblePlay key file.".to_owned(),
                ));
                return;
            }
            Err(error) => {
                inner.state.flow = Some(refused(
                    &file_name,
                    format!("This key file cannot be used: {error}."),
                ));
                return;
            }
        };
        if file.is_protected() {
            inner.import_pending = Some(file);
            inner.state.flow = Some(Flow::Enroll(EnrollStep::ImportUnlock {
                file_name,
                error: None,
            }));
            return;
        }
        self.store_imported(inner, file, None, &file_name);
    }

    /// Stores an imported key without its passphrase (Windows data protection keeps it), after
    /// unlocking it once: a file whose seed does not match its public key and ID is refused.
    fn store_imported(
        &self,
        inner: &mut Inner,
        file: KeyFile,
        passphrase: Option<&str>,
        file_name: &str,
    ) {
        let plain = match file.rewrap(passphrase, None) {
            Ok(plain) => plain,
            Err(error) => {
                inner.state.flow = Some(refused(
                    file_name,
                    format!("This key file cannot be used: {error}."),
                ));
                return;
            }
        };
        if plain.unlock(None).is_err() {
            inner.state.flow = Some(refused(
                file_name,
                "The key file's ID and key do not match.".to_owned(),
            ));
            return;
        }
        if let Err(error) = self.paths.keys.save_new(&plain) {
            inner.state.flow = Some(refused(
                file_name,
                format!("The key could not be stored: {error:#}"),
            ));
            return;
        }
        // load_key clears the enroll flow once the stored key opens.
        load_key(&self.paths, inner);
    }

    fn unlock_import(&self, inner: &mut Inner, passphrase: Zeroizing<String>) {
        let unlock_step = |name: String, message: Option<String>| {
            Some(Flow::Enroll(EnrollStep::ImportUnlock {
                file_name: name,
                error: message,
            }))
        };
        let shown_name = match &inner.state.flow {
            Some(Flow::Enroll(EnrollStep::ImportUnlock { file_name, .. })) => file_name.clone(),
            _ => String::new(),
        };
        if let Some(file) = inner.import_pending.take() {
            match file.unlock(Some(passphrase.as_str())) {
                Ok(_) => self.store_imported(inner, file, Some(passphrase.as_str()), &shown_name),
                Err(DeviceKeyError::AuthenticationFailed) => {
                    inner.import_pending = Some(file);
                    inner.state.flow = unlock_step(
                        shown_name,
                        Some("That passphrase did not open the key.".to_owned()),
                    );
                }
                Err(error) => {
                    inner.state.flow = Some(refused(
                        &shown_name,
                        format!("This key file cannot be used: {error}."),
                    ));
                }
            }
        } else if let Some(file) = inner.stored_locked.take() {
            // The stored key is protected: unlock it for this run; the file stays as it is.
            match file.unlock(Some(passphrase.as_str())) {
                Ok(signing) => {
                    inner.identity = Some(DeviceIdentity::from_signing_key(signing));
                    inner.state.flow = None;
                }
                Err(error) => {
                    let message = if matches!(error, DeviceKeyError::AuthenticationFailed) {
                        "That passphrase did not open the key.".to_owned()
                    } else {
                        format!("The key cannot be opened: {error}.")
                    };
                    inner.stored_locked = Some(file);
                    inner.state.flow = unlock_step(KEY_FILE_NAME.to_owned(), Some(message));
                }
            }
        }
    }

    fn cancel_flow(&self, inner: &mut Inner) {
        match &inner.state.flow {
            // The stored key still needs its passphrase, and the first choice stays: no way back.
            Some(Flow::Enroll(EnrollStep::Choose)) => {}
            Some(Flow::Enroll(EnrollStep::ImportUnlock { .. }))
                if inner.stored_locked.is_some() => {}
            Some(Flow::Enroll(EnrollStep::Created)) => inner.state.flow = None,
            Some(Flow::Enroll(_)) => {
                inner.import_pending = None;
                inner.state.flow = Some(Flow::Enroll(EnrollStep::Choose));
            }
            Some(Flow::PairConfirm { .. }) | Some(Flow::Approval { .. }) => {
                inner.pair_generation += 1;
                inner.pair_started = None;
                refresh_pcs(&self.paths, inner);
                inner.state.flow = None;
            }
            _ => inner.state.flow = None,
        }
    }

    fn rename(&self, inner: &mut Inner, pc_id: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        if let Ok(mut entries) = self.paths.pcs.load() {
            if let Some(entry) = entries.iter_mut().find(|entry| entry.id == pc_id) {
                entry.name = name.to_owned();
                let _ = self.paths.pcs.save(&entries);
            }
        }
        refresh_pcs(&self.paths, inner);
    }

    fn connect(&self, inner: &mut Inner, pc_id: &str) {
        let Some(pc) = inner.state.pcs.iter().find(|pc| pc.id == pc_id) else {
            return;
        };
        inner.state.session = Some(Session {
            pc_id: pc.id.clone(),
            pc_name: pc.name.clone(),
            mode: pc.last_mode.unwrap_or(StreamMode::Work),
            ..Session::default()
        });
        inner.state.flow = None;
    }

    fn submit_pairing(
        &self,
        inner: &mut Inner,
        link_text: Option<String>,
        pc_id: String,
        code: String,
        relay: String,
    ) {
        let fail = |inner: &mut Inner, message: &str| {
            inner.state.flow = Some(Flow::PairEnter {
                remote: false,
                error: Some(message.to_owned()),
            });
        };
        let (mut pc_id, mut code, mut relay) = (pc_id, code, relay);
        if let Some(text) = link_text.filter(|text| !text.trim().is_empty()) {
            match link::parse(&text) {
                Ok(parsed) => {
                    pc_id = parsed.pc_id;
                    code = parsed.code;
                    if !parsed.relay.is_empty() {
                        relay = parsed.relay;
                    }
                }
                Err(message) => return fail(inner, &message),
            }
        }
        let Some(identity) = inner.identity.as_ref().map(DeviceIdentity::duplicate) else {
            return fail(inner, "This device has no unlocked key yet.");
        };
        let Ok(pc) = DeviceId::parse(pc_id.trim()) else {
            return fail(
                inner,
                "The PC ID is not valid. It has 16 characters, like 77VH-7M56-8GJK-Y12J.",
            );
        };
        let Ok(code) = PairingCode::parse(code.trim()) else {
            return fail(
                inner,
                "A pairing code has 10 characters from 0-9 and A-Z (XXXXX-XXXXX).",
            );
        };
        let relay = relay.trim().to_owned();
        if relay.is_empty() {
            return fail(inner, "Enter the relay address shown on the PC.");
        }
        let relay = if relay.contains("://") {
            relay
        } else {
            format!("https://{relay}")
        };
        if inner
            .state
            .pcs
            .iter()
            .any(|known| known.id == pc.to_string())
        {
            return fail(inner, "This PC is already in Your PCs.");
        }

        inner.pair_generation += 1;
        let generation = inner.pair_generation;
        inner.pair_started = Some(Instant::now());
        inner.state.flow = Some(Flow::PairConfirm {
            pc_name: pc.to_string(),
            pc_id: pc.to_string(),
            number: 0,
            waiting_secs: 0,
            remote: false,
            outcome: Outcome::Waiting,
        });

        let shared = Arc::clone(&self.inner);
        let notifier = Arc::clone(&self.notifier);
        let paths = Arc::clone(&self.paths);
        std::thread::spawn(move || {
            let viewer = ViewerAuth::new(identity, Some(pc));
            let config = NetConfig::new(relay.clone(), Codec::H264, viewer);
            let result = net::pair(&config, code, &mut |text: &str| {
                let number: u32 = text
                    .chars()
                    .filter(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0);
                let mut inner = lock(&shared);
                if inner.pair_generation == generation {
                    if let Some(Flow::PairConfirm { number: shown, .. }) = &mut inner.state.flow {
                        *shown = number;
                    }
                }
                drop(inner);
                notify(&notifier);
            });
            let mut inner = lock(&shared);
            if inner.pair_generation != generation {
                return;
            }
            inner.pair_started = None;
            match result {
                Ok((id, public)) => match paths.pcs.pin(&id, &public, &relay) {
                    Ok(_) => {
                        refresh_pcs(&paths, &mut inner);
                        if let Some(Flow::PairConfirm { outcome, .. }) = &mut inner.state.flow {
                            *outcome = Outcome::Approved { by: Answerer::Pc };
                        }
                    }
                    Err(error) => {
                        inner.state.flow = Some(Flow::PairEnter {
                            remote: false,
                            error: Some(format!("The PC could not be saved: {error:#}")),
                        });
                    }
                },
                Err(error) => {
                    let text = error.to_string();
                    let outcome = if text.contains("denied") {
                        Some(Outcome::Denied { by: Answerer::Pc })
                    } else if text.contains("timed out") {
                        Some(Outcome::TimedOut)
                    } else if text.contains("window on the PC closed") {
                        Some(Outcome::Cancelled)
                    } else {
                        None
                    };
                    match (outcome, &mut inner.state.flow) {
                        (Some(new), Some(Flow::PairConfirm { outcome, .. })) => *outcome = new,
                        _ => {
                            inner.state.flow = Some(Flow::PairEnter {
                                remote: false,
                                error: Some(pair_error_text(&error)),
                            })
                        }
                    }
                }
            }
            drop(inner);
            notify(&notifier);
        });
    }
}

/// The sentence shown on the pairing form when pairing could not start or finish.
fn pair_error_text(error: &net::NetError) -> String {
    match error {
        net::NetError::NotAdmitted => "The relay did not accept the code. Check the PC ID and \
            the code, and that the pairing window on the PC is open."
            .to_owned(),
        net::NetError::Socket(_) | net::NetError::Handshake(_) | net::NetError::Stalled(_) => {
            format!(
                "The relay could not be reached ({error}). Check the relay address and the network."
            )
        }
        other => other.to_string(),
    }
}
