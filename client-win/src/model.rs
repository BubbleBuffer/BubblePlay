//! The boundary between the UI and whatever drives it (the mock today, the
//! real net/media/host-control plumbing later).
//!
//! The UI reads one plain [`AppState`] snapshot per frame and sends
//! [`Action`]s back. Everything here is plain data: no handles, no locks.
//! A backend that changes state on another thread calls the notifier it was
//! given in [`Backend::set_notifier`] so the shell repaints.

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
    Restarting { in_secs: u32 },
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
    /// Temporary until device keys land: the relay password step.
    Password { pc_name: String, relay: String, error: Option<String> },
    PairEnter { error: Option<String> },
    PairConfirm { pc_name: String, pc_id: String, number: u32, waiting_secs: u32 },
    /// PC is at the Windows sign-in screen: a fresh ntfy approval sent by
    /// the PC itself is needed.
    Approval { pc_id: String, waiting_secs: u32 },
    Connecting { pc_id: String, mode: StreamMode, steps: Vec<ConnStep> },
    Failed { pc_id: String, steps: Vec<ConnStep>, message: String },
    /// The PC did not accept this device's key (revoked or never paired).
    Refused { pc_id: String },
    Rotate { old_id: String, new_id: String, pcs: Vec<RotatePc> },
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
    pub quality: Quality,
    pub audio: bool,
    /// Input capture (driven by the input module later).
    pub captured: bool,
    pub link: Link,
    pub notice: Option<SessionNotice>,
    pub stats: Stats,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Monitor {
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
    Reconnecting { attempt: u32, secs: u32 },
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
    Connect { pc_id: String },
    CancelFlow,
    Retry,
    RetryWebSocket,
    Forget { pc_id: String },
    Rename { pc_id: String, name: String },
    SubmitPassword { password: String },
    // pairing (this device joins a PC)
    StartPairing,
    SubmitPairing { link: Option<String>, pc_id: String, code: String, relay: String },
    // vouching for another device on a remote PC
    Vouch { request_id: String, accept: bool, permissions: Vec<String> },
    // This PC
    AnswerPairing { request_id: String, allow: bool },
    Revoke { device_id: String },
    SetMayVouch { device_id: String, may_vouch: bool },
    SetHostSetting(HostSetting),
    StartHosting,
    StopHosting,
    RestartHost,
    RollBackHost,
    OpenLogsFolder,
    ShowPairDevice,
    // this device's key
    ExportKey,
    ImportKey,
    ReplaceKey,
    // session
    SetMode(StreamMode),
    SetMonitor(usize),
    SetQuality(Quality),
    SetAudio(bool),
    Disconnect,
}
