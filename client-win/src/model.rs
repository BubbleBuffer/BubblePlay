//! The boundary between the UI and whatever drives it (the mock today, the
//! real net/media/host-control plumbing later).
//!
//! The UI reads one plain [`AppState`] snapshot per frame and sends
//! [`Action`]s back. Everything here is plain data: no handles, no locks.
//! A backend that changes state on another thread calls the notifier it was
//! given in [`Backend::set_notifier`] so the shell repaints.
//!
//! # Device-keys seam
//!
//! The real device-keys backend (docs/architecture/device-keys.md) plugs in
//! here by implementing [`Backend`]; the UI needs nothing else. What it owes
//! the UI, step by step (`src/mock.rs` plays each one):
//!
//! - **No key yet** (first start): `this_device.id` is empty and `flow` is
//!   `Flow::Enroll(EnrollStep::Choose)`. [`Action::CreateKey`] leads to
//!   `EnrollStep::Created`; [`Action::ImportKey`] opens the file picker and
//!   leads to `ImportUnlock` (the file has a passphrase), `ImportRefused`
//!   (wrong format, ID or key mismatch) or straight to no flow (a plain
//!   file). [`Action::UnlockImport`] answers `ImportUnlock`.
//! - **Pairing** at the PC: [`Action::SubmitPairing`] (link, or PC ID, code
//!   and relay) leads to `Flow::PairConfirm` with `remote: false` and
//!   `Outcome::Waiting`; remote pairing: [`Action::RequestRemotePairing`],
//!   `remote: true`. The backend moves `outcome` to `Approved`, `Denied`,
//!   `TimedOut` (5 minutes for a code, 10 for a remote request) or
//!   `Cancelled` (closed at the PC), and on `Approved` adds the PC to `pcs`.
//!   A code the PC refuses comes back as `Flow::PairEnter { error }`.
//! - **Connecting**: `Flow::Connecting`, then the session; a failure is
//!   `Flow::Failed` with a [`ConnError`], a key the PC does not accept is
//!   `Flow::Refused`. A PC at the sign-in screen first shows
//!   `Flow::Approval` (before sign-in) until its outcome is known.
//! - **Saved PCs**: `RemotePc::problem` keeps the last known trouble with a
//!   PC (from the relay or the last connect) for Your PCs.
//!
//! [`Action::CancelFlow`] always means "back": it leaves the flow, except in
//! the import steps of `Flow::Enroll`, where it returns to `Choose`.

use serde::{Deserialize, Serialize};

/// What the UI talks to. The mock implements it; so will the real client.
pub trait Backend {
    /// Current state. Called once per frame, so keep it cheap (a clone of
    /// a small struct behind a lock is fine).
    fn snapshot(&self) -> AppState;
    /// A user action. Must not block; results arrive as later snapshots.
    fn act(&mut self, action: Action);
    /// Called once at start. The backend calls `notify` (from any thread)
    /// whenever the snapshot changed and the window should repaint.
    fn set_notifier(&mut self, notify: Notifier);
}

pub type Notifier = std::sync::Arc<dyn Fn() + Send + Sync>;

// ---------------------------------------------------------------- state

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppState {
    pub this_device: ThisDevice,
    /// Remote PCs paired with this device's key ("Your PCs").
    pub pcs: Vec<RemotePc>,
    /// The BubblePlay host on this computer ("This PC"). None when no host
    /// is installed here.
    pub host: Option<HostStatus>,
    /// The connect / pair / rotate flow, when one is in progress.
    pub flow: Option<Flow>,
    /// The live session, when connected (or reconnecting).
    pub session: Option<Session>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ThisDevice {
    /// Device ID, 16 Crockford base32 chars shown in four groups.
    pub id: String,
    /// Where the key file lives, as shown in Settings.
    pub key_path: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamMode {
    /// The default for a PC never connected to (matches the browser client).
    #[default]
    Work,
    Game,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PcKind {
    #[default]
    Desktop,
    Laptop,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RemotePc {
    /// PC ID (rooms are keyed by it).
    pub id: String,
    pub name: String,
    pub kind: PcKind,
    pub relay: String,
    pub online: bool,
    /// None for a PC never connected to; the UI then says Work.
    pub last_mode: Option<StreamMode>,
    /// Display text, e.g. "Today 09:12" or "2 Oct".
    pub last_seen: Option<String>,
    /// Display text of what this device may do there, e.g. "View and input".
    pub allowed: String,
    /// A pairing request for this PC that this device could vouch for.
    pub vouch_request: Option<VouchRequest>,
    /// The last known trouble with this PC, shown on its row and in its
    /// detail panel. None when the last connect worked (or none was tried).
    pub problem: Option<PcProblem>,
    /// The monitor this PC opens on, saved when a monitor is picked in a
    /// session (the browser's default monitor). Holds the monitor's selector
    /// from the host's monitor list; None means the host's default.
    #[serde(default)]
    pub default_monitor: Option<String>,
}

/// Trouble with a saved PC that Your PCs shows before anyone clicks Connect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PcProblem {
    /// The PC's relay did not answer, so its online state is unknown.
    RelayUnreachable,
    /// The last connect was refused: this device's key is not on the PC's
    /// list (revoked there, or never paired).
    NotAccepted,
    /// The PC answered with a key that does not match the pinned one and no
    /// signed rotation. Connecting is blocked until it is paired again.
    KeyChanged,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VouchRequest {
    pub request_id: String,
    pub device_name: String,
    pub device_id: String,
    /// The 6-digit comparison number.
    pub number: u32,
    pub expires_in_secs: u32,
    /// Permissions the vouch would grant; "Before sign-in" is never offered.
    pub permissions: Vec<Permission>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permission {
    pub key: String,
    pub label: String,
    pub granted: bool,
}

// ------------------------------------------------------------- This PC

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostStatus {
    pub pc_name: String,
    /// None until device keys land on the host.
    pub pc_id: Option<String>,
    pub run: HostRun,
    /// The viewer currently connected to this PC.
    pub viewer: Option<ConnectedViewer>,
    pub settings: HostSettings,
    pub build: BuildInfo,
    pub can_roll_back: bool,
    pub paired: Vec<PairedDevice>,
    /// An incoming pairing request waiting for Allow or Deny.
    pub request: Option<PairingRequest>,
    /// A request answered elsewhere (tray or another device); the card shows
    /// it briefly and then the backend clears it.
    pub answered: Option<AnsweredRequest>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostRun {
    Starting,
    #[default]
    Running,
    Restarting {
        in_secs: u32,
    },
    Stopped,
    NotConfigured,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConnectedViewer {
    pub name: String,
    /// Display text, e.g. "09:12".
    pub since: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSettings {
    pub stream_sound: bool,
    pub admin_mode: bool,
    pub start_at_login: bool,
    /// Takes effect after a host restart.
    pub cpu_mode: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostSetting {
    StreamSound(bool),
    AdminMode(bool),
    StartAtLogin(bool),
    CpuMode(bool),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuildInfo {
    pub hash: String,
    pub state: BuildState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildState {
    NoRecord,
    Pending,
    Probation,
    #[default]
    Good,
    RolledBack,
    Failed,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PairedDevice {
    pub id: String,
    pub name: String,
    pub connected: bool,
    /// Display text, e.g. "View, input".
    pub allowed: String,
    /// Display text, e.g. "Seen today", "Not seen for 41 days".
    pub last_seen: String,
    pub vouched_by: Option<String>,
    pub may_vouch: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PairingRequest {
    pub request_id: String,
    pub device_name: String,
    pub device_id: String,
    pub number: u32,
    pub expires_in_secs: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AnsweredRequest {
    pub request: PairingRequest,
    pub answered_on: String,
    pub allowed: bool,
}

// ---------------------------------------------------------------- flows

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Flow {
    /// First start without a device key: create or import one.
    Enroll(EnrollStep),
    /// Pairing input. `remote: false` takes the PC ID and the one-time code
    /// from the tray (or the pairing link); `remote: true` asks a PC with
    /// remote pairing on, by PC ID only.
    PairEnter { remote: bool, error: Option<String> },
    /// The pairing wait: the comparison number is shown here and confirmed
    /// at the PC or by a paired device that vouches.
    PairConfirm {
        pc_name: String,
        pc_id: String,
        number: u32,
        waiting_secs: u32,
        remote: bool,
        outcome: Outcome,
    },
    /// PC is at the Windows sign-in screen: a fresh ntfy approval sent by
    /// the PC itself is needed. `Approved` is never shown: the flow moves on
    /// to `Connecting`.
    Approval {
        pc_id: String,
        waiting_secs: u32,
        outcome: Outcome,
    },
    Connecting {
        pc_id: String,
        mode: StreamMode,
        steps: Vec<ConnStep>,
    },
    Failed {
        pc_id: String,
        steps: Vec<ConnStep>,
        error: ConnError,
    },
    /// The PC did not accept this device's key (revoked or never paired).
    Refused { pc_id: String },
    Rotate {
        old_id: String,
        new_id: String,
        pcs: Vec<RotatePc>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnrollStep {
    /// No key: Create key or Import key file.
    Choose,
    /// A new key was made; `AppState::this_device` has its ID and path.
    Created,
    /// The chosen key file has a passphrase; it is asked once, then the key
    /// is kept with Windows data protection like a created one.
    ImportUnlock {
        file_name: String,
        error: Option<String>,
    },
    /// The file is not a usable key (format, version, or ID and key do not
    /// match). `reason` is the sentence shown.
    ImportRefused { file_name: String, reason: String },
}

/// Where an approval stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Waiting,
    Approved {
        by: Answerer,
    },
    Denied {
        by: Answerer,
    },
    /// Nobody answered before the request expired.
    TimedOut,
    /// The request was closed at the PC before anyone answered (the tray's
    /// pairing window closed). Cancelling here leaves the flow instead.
    Cancelled,
}

/// Who answered a pairing request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answerer {
    /// Someone at the PC (tray or Devices window).
    Pc,
    /// A paired device, by name (a voucher).
    Device(String),
}

/// Why a connect attempt ended, for `Flow::Failed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnError {
    /// Key accepted and stream negotiated, but no video within `secs`.
    NoFirstFrame { secs: u32 },
    /// The relay did not answer.
    RelayUnreachable,
    /// Another viewer is connected (one viewer per PC).
    PcBusy,
    /// A running session dropped and reconnecting gave up.
    Lost { attempts: u32 },
    /// This computer has no hardware decoder for the stream's codec
    /// (`media::NoHardwareVideoDecoder`); `detail` is its one-line reason.
    NoHardwareDecoder { codec: String, detail: String },
    /// The PC's key does not match the pinned one and no signed rotation
    /// came with it ("PC key changed"); nothing was sent.
    PcKeyChanged,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnStep {
    pub label: String,
    /// Evidence text on the right, e.g. relay host or "1.4 s".
    pub detail: String,
    pub status: StepStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Done,
    Now,
    Failed,
    Waiting,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RotatePc {
    pub name: String,
    pub status: StepStatus,
    pub detail: String,
}

// -------------------------------------------------------------- session

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Session {
    pub pc_id: String,
    pub pc_name: String,
    pub mode: StreamMode,
    pub rtt_ms: u32,
    pub monitors: Vec<Monitor>,
    pub monitor: usize,
    /// A monitor pick is waiting for the host to switch.
    #[serde(default)]
    pub monitor_pending: bool,
    /// The last monitor pick the host refused or failed, as shown to the user.
    #[serde(default)]
    pub monitor_error: Option<String>,
    pub quality: Quality,
    /// Whether the quality picker can change anything here (the live client
    /// cannot yet send a quality change to the host).
    #[serde(default)]
    pub quality_controls_available: bool,
    pub audio: bool,
    /// Whether the audio toggle can change anything here.
    #[serde(default)]
    pub audio_controls_available: bool,
    /// Input capture (driven by the input module later).
    pub captured: bool,
    pub link: Link,
    pub notice: Option<SessionNotice>,
    pub stats: Stats,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Monitor {
    /// The host's id for this monitor, sent back when it is picked.
    #[serde(default)]
    pub selector: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// Position in the host's desktop, for the monitor map.
    pub x: i32,
    pub y: i32,
    pub primary: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quality {
    #[default]
    Auto,
    Sharp,
    Smooth,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Link {
    #[default]
    Live,
    Reconnecting {
        attempt: u32,
        secs: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionNotice {
    /// Input to an elevated window is blocked until Admin mode is on.
    AdminRequired,
    /// UAC prompt or sign-in screen on the PC.
    SecureDesktop,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    pub resolution: String,
    pub fps: f32,
    pub bitrate_mbps: f32,
    pub rtt_ms: u32,
    pub decode_ms: f32,
    pub dropped: u32,
    pub transport: String,
    pub codec: String,
    /// Recent frame times in ms, oldest first, for the sparkline.
    pub frame_ms: Vec<f32>,
}

// -------------------------------------------------------------- actions

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Action {
    // Your PCs and connecting
    Connect {
        pc_id: String,
    },
    CancelFlow,
    Retry,
    RetryWebSocket,
    Forget {
        pc_id: String,
    },
    Rename {
        pc_id: String,
        name: String,
    },
    // pairing (this device joins a PC)
    StartPairing,
    /// Switch the pairing input to a remote request (no code).
    StartRemotePairing,
    SubmitPairing {
        link: Option<String>,
        pc_id: String,
        code: String,
        relay: String,
    },
    RequestRemotePairing {
        pc_id: String,
        relay: String,
    },
    // vouching for another device on a remote PC
    Vouch {
        request_id: String,
        accept: bool,
        permissions: Vec<String>,
    },
    // This PC
    AnswerPairing {
        request_id: String,
        allow: bool,
    },
    Revoke {
        device_id: String,
    },
    SetMayVouch {
        device_id: String,
        may_vouch: bool,
    },
    SetHostSetting(HostSetting),
    StartHosting,
    StopHosting,
    RestartHost,
    RollBackHost,
    OpenLogsFolder,
    ShowPairDevice,
    // this device's key
    CreateKey,
    /// Answers `EnrollStep::ImportUnlock`. The passphrase only unlocks the
    /// file locally; it is never sent anywhere.
    UnlockImport {
        passphrase: String,
    },
    ExportKey,
    ImportKey,
    /// `ImportKey` for hosts that pick the file themselves (the Flutter app, on both
    /// platforms): the host read the chosen file and hands over its name and text.
    ImportKeyText {
        file_name: String,
        text: String,
    },
    ReplaceKey,
    // session
    SetMode(StreamMode),
    SetMonitor(usize),
    SetQuality(Quality),
    SetAudio(bool),
    Disconnect,
}
