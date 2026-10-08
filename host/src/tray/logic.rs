//! Platform-independent tray logic: restart backoff, size-capped log rotation,
//! persisted settings, stored host configuration and host argument building.
//! Everything here is unit-tested; Win32 glue lives in the tray binary.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A child that stays up this long is considered healthy and resets backoff.
pub const HEALTHY_RUN: Duration = Duration::from_secs(5 * 60);

pub const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Total files kept per log: the live file plus `LOG_KEEP - 1` rotations.
pub const LOG_KEEP: usize = 5;

/// Exponential restart backoff: 1 s doubling to 30 s, reset after a healthy run.
#[derive(Debug, Clone)]
pub struct Backoff {
    next: Duration,
    min: Duration,
    max: Duration,
    healthy: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(BACKOFF_MIN, BACKOFF_MAX, HEALTHY_RUN)
    }
}

impl Backoff {
    pub fn new(min: Duration, max: Duration, healthy: Duration) -> Self {
        Self {
            next: min,
            min,
            max,
            healthy,
        }
    }

    /// Record a child exit after it ran for `ran_for` and return the delay
    /// before the next start.
    pub fn on_exit(&mut self, ran_for: Duration) -> Duration {
        if ran_for >= self.healthy {
            self.next = self.min;
        }
        let delay = self.next;
        self.next = (self.next * 2).min(self.max);
        delay
    }

    /// Record a failure to even start the child.
    pub fn on_start_failure(&mut self) -> Duration {
        self.on_exit(Duration::ZERO)
    }
}

/// Size-capped rotating log: `name.log`, `name.1.log` .. `name.{keep-1}.log`.
/// Every I/O failure is swallowed (and counted) so logging can never take the
/// tray or the host down.
#[derive(Debug)]
pub struct RotatingLog {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    file: Option<File>,
    size: u64,
    failures: u64,
}

impl RotatingLog {
    pub fn new(path: PathBuf, max_bytes: u64, keep: usize) -> Self {
        let mut log = Self {
            path,
            max_bytes: max_bytes.max(1),
            keep: keep.max(1),
            file: None,
            size: 0,
            failures: 0,
        };
        log.open();
        log
    }

    #[cfg(test)]
    pub fn failures(&self) -> u64 {
        self.failures
    }

    pub fn rotated_path(&self, index: usize) -> PathBuf {
        rotated_path(&self.path, index)
    }

    fn open(&mut self) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => {
                self.size = file.metadata().map(|m| m.len()).unwrap_or(0);
                self.file = Some(file);
            }
            Err(_) => {
                self.failures += 1;
                self.file = None;
            }
        }
    }

    fn rotate(&mut self) {
        self.file = None;
        if self.keep > 1 {
            let _ = fs::remove_file(self.rotated_path(self.keep - 1));
            for index in (1..self.keep - 1).rev() {
                let from = self.rotated_path(index);
                if from.exists() {
                    let _ = fs::rename(&from, self.rotated_path(index + 1));
                }
            }
            if fs::rename(&self.path, self.rotated_path(1)).is_err() {
                self.failures += 1;
                // Could not move the live file (e.g. another process holds it
                // without FILE_SHARE_DELETE); truncate rather than grow forever.
                let _ = File::create(&self.path);
            }
        } else {
            let _ = File::create(&self.path);
        }
        self.open();
        // If reopening failed the size would never reset; force it.
        if self.file.is_none() {
            self.size = 0;
        }
    }

    /// Append `data`, rotating first when it would exceed the cap. Never fails.
    pub fn append(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if self.size > 0 && self.size + data.len() as u64 > self.max_bytes {
            self.rotate();
        }
        if self.file.is_none() {
            self.open();
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        // A single oversized write is truncated to the cap rather than refused.
        let len = data.len().min(self.max_bytes as usize);
        match file.write_all(&data[..len]) {
            Ok(()) => self.size += len as u64,
            Err(_) => {
                self.failures += 1;
                self.file = None;
            }
        }
    }
}

pub fn rotated_path(path: &Path, index: usize) -> PathBuf {
    if index == 0 {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match path.extension() {
        Some(ext) => format!("{stem}.{index}.{}", ext.to_string_lossy()),
        None => format!("{stem}.{index}"),
    };
    path.with_file_name(name)
}

/// Cloneable handle usable as an `io::Write` and a tracing `MakeWriter`.
#[derive(Debug, Clone)]
pub struct SharedLog(Arc<Mutex<RotatingLog>>);

impl SharedLog {
    pub fn new(path: PathBuf) -> Self {
        Self(Arc::new(Mutex::new(RotatingLog::new(
            path,
            LOG_MAX_BYTES,
            LOG_KEEP,
        ))))
    }

    pub fn append(&self, data: &[u8]) {
        let mut guard = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.append(data);
    }
}

impl Write for SharedLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.append(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Persisted tray preferences (`%LOCALAPPDATA%\BubblePlay\tray.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TraySettings {
    pub admin_mode: bool,
    pub start_at_login: bool,
}

impl Default for TraySettings {
    fn default() -> Self {
        Self {
            admin_mode: false,
            start_at_login: true,
        }
    }
}

impl TraySettings {
    /// Missing or unreadable settings fall back to defaults; the tray must
    /// always come up.
    pub fn load(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }
}

/// Stored host configuration (`%LOCALAPPDATA%\BubblePlay\host.json`). Secrets
/// are DPAPI blobs (CurrentUser scope when written by setup-tray.ps1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredHostConfig {
    pub worker: String,
    pub token_dpapi_hex: String,
    #[serde(default)]
    pub host_gate_token_dpapi_hex: Option<String>,
    #[serde(default)]
    pub monitor: Option<String>,
}

impl StoredHostConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        bubbleplay_host::relay_url::require_secure_relay_url(&self.worker)?;
        if self.token_dpapi_hex.trim().is_empty() {
            anyhow::bail!("host.json tokenDpapiHex is empty");
        }
        Ok(())
    }
}

/// Command-line arguments for `bubbleplay-host.exe`. Secrets never appear
/// here; they travel as environment variables.
pub fn build_host_args(config: &StoredHostConfig) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--mode"),
        OsString::from("serve"),
        OsString::from("--worker"),
        OsString::from(&config.worker),
    ];
    if let Some(monitor) = config.monitor.as_deref().filter(|m| !m.is_empty()) {
        args.push(OsString::from("--monitor"));
        args.push(OsString::from(monitor));
    }
    args
}

pub fn decode_hex(value: &str) -> Result<Vec<u8>> {
    let value = value.trim();
    if value.len() % 2 != 0 {
        anyhow::bail!("DPAPI ciphertext has an odd hexadecimal length");
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).context("DPAPI ciphertext is not ASCII")?;
            u8::from_str_radix(pair, 16).context("DPAPI ciphertext contains non-hex data")
        })
        .collect()
}

/// State published by the host supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum HostState {
    Starting,
    Running { pid: u32 },
    Restarting { delay_ms: u64, reason: String },
    NotConfigured { reason: String },
    Stopped,
}

impl HostState {
    pub fn label(&self) -> String {
        match self {
            HostState::Starting => "Starting".into(),
            HostState::Running { .. } => "Running".into(),
            HostState::Restarting { delay_ms, .. } => {
                format!("Restarting in {}s", delay_ms.div_ceil(1000))
            }
            HostState::NotConfigured { .. } => "Stopped (not configured)".into(),
            HostState::Stopped => "Stopped".into(),
        }
    }
}

/// Admin-mode input path, as the tray sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    /// Admin mode is off: the host injects at user level.
    Off,
    /// Admin mode is on but no viewer is connected: the agent is not running
    /// and starts when a viewer session starts.
    Armed,
    /// The agent task was started; waiting for it to answer.
    Starting,
    /// The agent answered the probe: the host injects through it.
    Ready,
    /// The agent is missing, speaks another protocol, or does not start: the
    /// host injects at user level until the (one-UAC) agent update ran.
    NeedsUpdate { reason: String },
}

impl AgentStatus {
    pub fn needs_update(&self) -> bool {
        matches!(self, AgentStatus::NeedsUpdate { .. })
    }
}

/// Menu item that runs the elevated agent install/update.
pub const AGENT_UPDATE_LABEL: &str = "Admin agent needs update (one admin prompt)";

/// The status line shown at the top of the tray menu and in the tooltip.
pub fn status_line(state: &HostState, agent: &AgentStatus) -> String {
    let input = match agent {
        AgentStatus::Off => "user",
        AgentStatus::Armed => "user input, Admin mode armed",
        AgentStatus::Starting => "user input, admin agent starting",
        AgentStatus::Ready => "admin input",
        AgentStatus::NeedsUpdate { .. } => "user input, admin agent needs update",
    };
    format!("BubblePlay: {} ({input})", state.label())
}

/// The Admin mode line shown under the "Admin mode" menu item.
pub fn admin_mode_line(agent: &AgentStatus) -> Option<&'static str> {
    match agent {
        AgentStatus::Off => None,
        AgentStatus::Armed => Some("Admin mode: armed (starts when a viewer connects)"),
        AgentStatus::Starting => Some("Admin mode: starting (viewer connected)"),
        AgentStatus::Ready => Some("Admin mode: active"),
        AgentStatus::NeedsUpdate { .. } => Some("Admin mode: agent needs update"),
    }
}

/// Secure-desktop helper state is independent of Admin mode. The tray only
/// launches it; its authenticated host connection owns shutdown and timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecureHelperStatus {
    Armed,
    Starting,
    Ready,
    NeedsUpdate { reason: String },
}

impl SecureHelperStatus {
    pub fn needs_update(&self) -> bool {
        matches!(self, Self::NeedsUpdate { .. })
    }

    pub fn line(&self) -> String {
        match self {
            Self::Armed => "Secure desktop: armed (starts when a viewer connects)".into(),
            Self::Starting => "Secure desktop: helper starting".into(),
            Self::Ready => "Secure desktop: helper ready".into(),
            Self::NeedsUpdate { reason } => format!("Secure desktop: {reason}"),
        }
    }
}

pub const SECURE_HELPER_UPDATE_LABEL: &str =
    "Secure desktop helper needs update (one admin prompt)";

/// Launch only for an active viewer, independent of Admin mode. An idle
/// helper exits through the host pipe; the tray must never restart it idle.
pub fn secure_helper_should_launch(
    viewer_connected: bool,
    status: &SecureHelperStatus,
    updating: bool,
) -> bool {
    viewer_connected && !updating && matches!(status, SecureHelperStatus::Armed)
}

/// Handle each new viewer session with a real launch attempt. A stale
/// missing/incompatible result from an earlier session must not veto it:
/// installation may have been repaired while the tray remained running.
/// Admin mode is intentionally not an input to this lifecycle action.
pub fn start_secure_helper_for_viewer(
    status: &mut SecureHelperStatus,
    updating: bool,
    launch: impl FnOnce() -> Result<(), String>,
) -> Result<bool, String> {
    if updating {
        return Ok(false);
    }
    match launch() {
        Ok(()) => {
            *status = SecureHelperStatus::Starting;
            Ok(true)
        }
        Err(reason) => {
            *status = SecureHelperStatus::NeedsUpdate {
                reason: reason.clone(),
            };
            Err(reason)
        }
    }
}

/// How long the agent keeps running after the last viewer session ended, so
/// a reconnect or page reload does not restart it.
pub const AGENT_IDLE_GRACE: Duration = Duration::from_secs(30);

/// Decides when the Admin mode agent should run: while a viewer session is
/// active, and for [`AGENT_IDLE_GRACE`] after the last one ended. Fed from
/// the host's "viewer session started/ended" log lines; a host exit ends
/// every session.
#[derive(Debug, Clone, Default)]
pub struct ViewerGate {
    sessions: u32,
    idle_since: Option<Instant>,
}

impl ViewerGate {
    pub fn on_started(&mut self) {
        self.sessions = self.sessions.saturating_add(1);
        self.idle_since = None;
    }

    pub fn on_ended(&mut self, now: Instant) {
        if self.sessions > 0 {
            self.sessions -= 1;
            if self.sessions == 0 {
                self.idle_since = Some(now);
            }
        }
    }

    /// The host exited: its sessions are gone (it logs no end line then).
    pub fn on_host_exited(&mut self, now: Instant) {
        if self.sessions > 0 {
            self.sessions = 0;
            self.idle_since = Some(now);
        }
    }

    pub fn viewer_connected(&self) -> bool {
        self.sessions > 0
    }

    /// True while a session is active or the idle grace has not run out.
    pub fn wants_agent(&self, now: Instant) -> bool {
        self.sessions > 0
            || self
                .idle_since
                .is_some_and(|since| now.saturating_duration_since(since) < AGENT_IDLE_GRACE)
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes).with_context(|| format!("writing {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Quote a path for the HKCU Run value.
pub fn run_key_command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

pub fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bubbleplay-tray-test-{name}-{}-{}",
            std::process::id(),
            unix_ms_now()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn backoff_doubles_to_cap_and_resets_after_healthy_run() {
        let mut backoff = Backoff::default();
        let short = Duration::from_secs(2);
        let delays: Vec<u64> = (0..7).map(|_| backoff.on_exit(short).as_secs()).collect();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(backoff.on_exit(HEALTHY_RUN).as_secs(), 1);
        assert_eq!(backoff.on_exit(short).as_secs(), 2);
        assert_eq!(backoff.on_start_failure().as_secs(), 4);
    }

    #[test]
    fn rotating_log_caps_size_and_file_count() {
        let dir = temp_dir("rotate");
        let path = dir.join("host.log");
        let mut log = RotatingLog::new(path.clone(), 100, 3);
        for _ in 0..20 {
            log.append(&[b'x'; 40]);
        }
        assert!(fs::metadata(&path).unwrap().len() <= 100);
        assert!(rotated_path(&path, 1).exists());
        assert!(rotated_path(&path, 2).exists());
        assert!(!rotated_path(&path, 3).exists());
        for index in 0..3 {
            assert!(fs::metadata(rotated_path(&path, index)).unwrap().len() <= 100);
        }
        assert_eq!(log.failures(), 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rotating_log_truncates_oversized_write_and_keeps_existing_size() {
        let dir = temp_dir("oversize");
        let path = dir.join("tray.log");
        fs::write(&path, b"existing\n").unwrap();
        let mut log = RotatingLog::new(path.clone(), 64, 2);
        log.append(&[b'y'; 200]);
        // Existing content forced a rotation before the oversized write.
        assert_eq!(fs::read(rotated_path(&path, 1)).unwrap(), b"existing\n");
        assert_eq!(fs::metadata(&path).unwrap().len(), 64);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rotating_log_never_panics_when_directory_is_unusable() {
        let dir = temp_dir("unusable");
        // A file where the log directory should be makes every open fail.
        let blocker = dir.join("logs");
        fs::write(&blocker, b"").unwrap();
        let mut log = RotatingLog::new(blocker.join("host.log"), 10, 2);
        for _ in 0..5 {
            log.append(b"hello world\n");
        }
        assert!(log.failures() > 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rotated_path_names() {
        let path = Path::new(r"C:\logs\host.log");
        assert_eq!(rotated_path(path, 0), PathBuf::from(r"C:\logs\host.log"));
        assert_eq!(rotated_path(path, 3), PathBuf::from(r"C:\logs\host.3.log"));
    }

    #[test]
    fn settings_default_roundtrip_and_tolerate_garbage() {
        let dir = temp_dir("settings");
        let path = dir.join("tray.json");
        assert_eq!(TraySettings::load(&path), TraySettings::default());
        assert!(TraySettings::default().start_at_login);
        assert!(!TraySettings::default().admin_mode);
        let settings = TraySettings {
            admin_mode: true,
            start_at_login: false,
        };
        settings.save(&path).unwrap();
        assert_eq!(TraySettings::load(&path), settings);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"adminMode\": true"));
        fs::write(&path, b"{\"adminMode\": true}").unwrap();
        assert_eq!(
            TraySettings::load(&path),
            TraySettings {
                admin_mode: true,
                start_at_login: true
            }
        );
        fs::write(&path, b"not json").unwrap();
        assert_eq!(TraySettings::load(&path), TraySettings::default());
        let _ = fs::remove_dir_all(dir);
    }

    fn config() -> StoredHostConfig {
        StoredHostConfig {
            worker: "wss://play.example/host-ws".into(),
            token_dpapi_hex: "01AB".into(),
            host_gate_token_dpapi_hex: Some("CD".into()),
            monitor: None,
        }
    }

    #[test]
    fn host_args_never_include_secrets() {
        let args = build_host_args(&config());
        assert_eq!(
            args,
            ["--mode", "serve", "--worker", "wss://play.example/host-ws"]
                .map(OsString::from)
                .to_vec()
        );
        assert!(!args.iter().any(|a| a == "--privileged-agent"));
        assert!(!args
            .iter()
            .any(|a| a == "--token" || a == "--host-gate-token"));
        assert!(!args.iter().any(|a| a.to_string_lossy().contains("01AB")));
        let mut with_monitor = config();
        with_monitor.monitor = Some("primary".into());
        let args = build_host_args(&with_monitor);
        assert_eq!(
            &args[4..],
            &[OsString::from("--monitor"), OsString::from("primary")]
        );
        with_monitor.monitor = Some(String::new());
        assert_eq!(build_host_args(&with_monitor).len(), 4);
    }

    #[test]
    fn host_config_parses_service_shape_and_validates() {
        let dir = temp_dir("config");
        let path = dir.join("host.json");
        fs::write(
            &path,
            br#"{"worker":"wss://w.example/host-ws","tokenDpapiHex":"0102","hostGateTokenDpapiHex":null}"#,
        )
        .unwrap();
        let parsed = StoredHostConfig::load(&path).unwrap();
        assert_eq!(parsed.host_gate_token_dpapi_hex, None);
        assert_eq!(parsed.monitor, None);
        fs::write(&path, br#"{"worker":"ftp://x","tokenDpapiHex":"01"}"#).unwrap();
        assert!(StoredHostConfig::load(&path).is_err());
        fs::write(&path, br#"{"worker":"wss://x","tokenDpapiHex":""}"#).unwrap();
        assert!(StoredHostConfig::load(&path).is_err());
        assert!(StoredHostConfig::load(&dir.join("missing.json")).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn tray_and_host_share_relay_transport_and_scheme_rules() {
        for (url, accepted) in [
            ("wss://play.example/host-ws", true),
            ("https://play.example/host-ws", true),
            ("ws://localhost:8787/host-ws", true),
            ("ws://127.0.0.1/host-ws", true),
            ("ws://[::1]/host-ws", true),
            ("ws://play.example/host-ws", false),
            ("ws://192.168.1.20/host-ws", false),
            ("http://play.example/host-ws", false),
            ("WSS://play.example/host-ws", false),
            ("WS://127.0.0.1/host-ws", false),
            ("Https://play.example/host-ws", false),
        ] {
            let mut stored = config();
            stored.worker = url.into();
            assert_eq!(stored.validate().is_ok(), accepted, "tray: {url}");
            assert_eq!(
                bubbleplay_host::relay_url::require_secure_relay_url(url).is_ok(),
                accepted,
                "host: {url}"
            );
        }
    }

    #[test]
    fn hex_decoding() {
        assert_eq!(decode_hex("00ff10").unwrap(), vec![0, 255, 16]);
        assert!(decode_hex("abc").is_err());
        assert!(decode_hex("zz").is_err());
    }

    #[test]
    fn status_text_names_the_input_path() {
        assert_eq!(
            status_line(&HostState::Running { pid: 1 }, &AgentStatus::Ready),
            "BubblePlay: Running (admin input)"
        );
        assert_eq!(
            status_line(
                &HostState::Restarting {
                    delay_ms: 1500,
                    reason: String::new()
                },
                &AgentStatus::Off
            ),
            "BubblePlay: Restarting in 2s (user)"
        );
        assert_eq!(
            status_line(&HostState::Stopped, &AgentStatus::Off),
            "BubblePlay: Stopped (user)"
        );
        assert_eq!(
            status_line(&HostState::Running { pid: 1 }, &AgentStatus::Armed),
            "BubblePlay: Running (user input, Admin mode armed)"
        );
        assert_eq!(admin_mode_line(&AgentStatus::Off), None);
        assert_eq!(
            admin_mode_line(&AgentStatus::Armed),
            Some("Admin mode: armed (starts when a viewer connects)")
        );
        assert_eq!(
            admin_mode_line(&AgentStatus::Ready),
            Some("Admin mode: active")
        );
        let stale = AgentStatus::NeedsUpdate { reason: "x".into() };
        assert!(stale.needs_update());
        assert!(!AgentStatus::Starting.needs_update());
        assert_eq!(
            status_line(&HostState::Running { pid: 1 }, &stale),
            "BubblePlay: Running (user input, admin agent needs update)"
        );
        assert_eq!(
            AGENT_UPDATE_LABEL,
            "Admin agent needs update (one admin prompt)"
        );
    }

    #[test]
    fn viewer_gate_runs_agent_only_around_sessions() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut gate = ViewerGate::default();
        // Armed: nothing connected, nothing wanted.
        assert!(!gate.wants_agent(t0));
        assert!(!gate.viewer_connected());
        // A stray end line (no session) changes nothing.
        gate.on_ended(at(1));
        assert!(!gate.wants_agent(at(1)));
        // First session starts the agent.
        gate.on_started();
        assert!(gate.viewer_connected());
        assert!(gate.wants_agent(at(2)));
        // Still wanted long into the session.
        assert!(gate.wants_agent(at(3600)));
        // Session ends at 100 s: kept through the grace, stopped after it.
        gate.on_ended(at(100));
        assert!(!gate.viewer_connected());
        assert!(gate.wants_agent(at(100)));
        assert!(gate.wants_agent(at(129)));
        assert!(!gate.wants_agent(at(130)));
        assert!(!gate.wants_agent(at(500)));
    }

    #[test]
    fn secure_helper_launch_is_viewer_scoped_and_does_not_restart_idle() {
        assert!(!secure_helper_should_launch(
            false,
            &SecureHelperStatus::Armed,
            false
        ));
        // Admin mode is deliberately absent from this decision.
        assert!(secure_helper_should_launch(
            true,
            &SecureHelperStatus::Armed,
            false
        ));
        assert!(!secure_helper_should_launch(
            true,
            &SecureHelperStatus::Armed,
            true
        ));
        assert!(!secure_helper_should_launch(
            true,
            &SecureHelperStatus::Starting,
            false
        ));
        assert!(!secure_helper_should_launch(
            true,
            &SecureHelperStatus::Ready,
            false
        ));
        let unavailable = SecureHelperStatus::NeedsUpdate {
            reason: "missing".into(),
        };
        assert!(!secure_helper_should_launch(true, &unavailable, false));
        assert!(unavailable.needs_update());
        let mut gate = ViewerGate::default();
        gate.on_started();
        gate.on_ended(Instant::now());
        assert!(gate.wants_agent(Instant::now()));
        assert!(!secure_helper_should_launch(
            gate.viewer_connected(),
            &SecureHelperStatus::Armed,
            false
        ));
    }

    #[test]
    fn every_viewer_start_launches_helper_with_admin_off_or_on_even_after_failure() {
        for admin_mode in [false, true] {
            let settings = TraySettings {
                admin_mode,
                ..TraySettings::default()
            };
            let mut gate = ViewerGate::default();
            let mut helper = SecureHelperStatus::NeedsUpdate {
                reason: "old startup failure".into(),
            };
            let mut launches = 0;
            // The actual action runs even when a startup query/probe had failed.
            gate.on_started();
            assert_eq!(
                start_secure_helper_for_viewer(&mut helper, false, || {
                    launches += 1;
                    Ok(())
                }),
                Ok(true)
            );
            assert!(gate.viewer_connected());
            assert_eq!(helper, SecureHelperStatus::Starting);
            // Every viewer start, including overlapping sessions or reconnects,
            // invokes the launcher even while a compatible helper was ready.
            helper = SecureHelperStatus::Ready;
            gate.on_started();
            assert_eq!(
                start_secure_helper_for_viewer(&mut helper, false, || {
                    launches += 1;
                    Ok(())
                }),
                Ok(true)
            );
            assert_eq!(launches, 2);
            assert_eq!(helper, SecureHelperStatus::Starting);
            assert_eq!(settings.admin_mode, admin_mode);
        }
    }

    #[test]
    fn viewer_helper_start_reports_launch_failure_and_defers_during_update() {
        let mut helper = SecureHelperStatus::Armed;
        assert_eq!(
            start_secure_helper_for_viewer(&mut helper, false, || Err("task missing".into())),
            Err("task missing".into())
        );
        assert_eq!(
            helper,
            SecureHelperStatus::NeedsUpdate {
                reason: "task missing".into()
            }
        );
        assert_eq!(
            start_secure_helper_for_viewer(&mut helper, true, || panic!(
                "must not launch during installation"
            )),
            Ok(false)
        );
        assert!(helper.needs_update());
        assert_eq!(
            start_secure_helper_for_viewer(&mut helper, false, || Ok(())),
            Ok(true)
        );
        assert_eq!(helper, SecureHelperStatus::Starting);
    }

    #[test]
    fn viewer_gate_reconnect_within_grace_keeps_agent() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut gate = ViewerGate::default();
        gate.on_started();
        gate.on_ended(at(10));
        // Page reload: the viewer is back 5 s later.
        assert!(gate.wants_agent(at(15)));
        gate.on_started();
        // The old grace deadline (40 s) no longer applies.
        assert!(gate.wants_agent(at(41)));
        assert!(gate.wants_agent(at(300)));
        // The new session's end starts a fresh grace.
        gate.on_ended(at(300));
        assert!(gate.wants_agent(at(329)));
        assert!(!gate.wants_agent(at(330)));
    }

    #[test]
    fn viewer_gate_counts_overlapping_sessions_and_host_exit() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut gate = ViewerGate::default();
        // A new session logged before the old one's end line.
        gate.on_started();
        gate.on_started();
        gate.on_ended(at(5));
        assert!(gate.viewer_connected());
        assert!(gate.wants_agent(at(1000)));
        gate.on_ended(at(1000));
        assert!(!gate.wants_agent(at(1030)));
        // A host that exits mid-session logs no end line: the exit ends it.
        gate.on_started();
        gate.on_host_exited(at(2000));
        assert!(!gate.viewer_connected());
        assert!(gate.wants_agent(at(2029)));
        assert!(!gate.wants_agent(at(2030)));
        // An exit with no session does not open a grace window.
        let mut idle = ViewerGate::default();
        idle.on_host_exited(at(1));
        assert!(!idle.wants_agent(at(1)));
    }

    #[test]
    fn run_key_is_quoted() {
        assert_eq!(
            run_key_command(Path::new(r"C:\Users\A B\bubbleplay_tray.exe")),
            r#""C:\Users\A B\bubbleplay_tray.exe""#
        );
    }
}
