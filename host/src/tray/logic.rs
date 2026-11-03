//! Platform-independent tray logic: restart backoff, child-output log classification,
//! persisted settings, stored host configuration and host argument building.
//! Everything here is unit-tested; Win32 glue lives in the tray binary.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A child that stays up this long is considered healthy and resets backoff.
pub const HEALTHY_RUN: Duration = Duration::from_secs(5 * 60);

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

/// What a line of host or agent child output means for the tray log: `None` to skip it,
/// otherwise the level to log it at. Tracing lines (`<ts> LEVEL target: msg`) are skipped at
/// every level: the child writes the same event to its own JSONL log, so echoing it would
/// record it twice. Anything else (a panic message, a backtrace, native library output, an
/// early startup failure before the child's log is up) is a warning, or an error when it
/// says so.
pub fn child_line_level(line: &str) -> Option<tracing::Level> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut words = line.split_whitespace();
    let first = words.next().unwrap_or("");
    let looks_like_tracing =
        first.len() >= 10 && first.as_bytes()[0].is_ascii_digit() && first.contains('T');
    if looks_like_tracing {
        return None;
    }
    let lower = line.to_ascii_lowercase();
    if lower.contains("panicked") || lower.contains("fatal") || lower.contains("error") {
        Some(tracing::Level::ERROR)
    } else {
        Some(tracing::Level::WARN)
    }
}

/// Write one line of child output into the tray log (target `child`), redacted by the log
/// layer. `source` names the child (`host`, `agent`), `stream` is `stdout` or `stderr`.
pub fn log_child_line(source: &'static str, stream: &'static str, line: &[u8]) {
    let text = String::from_utf8_lossy(line);
    let text = text.trim_end();
    match child_line_level(text) {
        Some(tracing::Level::ERROR) => {
            tracing::error!(target: "child", source, stream, "{text}")
        }
        Some(_) => tracing::warn!(target: "child", source, stream, "{text}"),
        None => {}
    }
}

/// Persisted tray preferences (`%LOCALAPPDATA%\BubblePlay\tray.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TraySettings {
    pub start_at_login: bool,
    /// Send PC sound to the viewer. Missing from older files means on.
    pub audio: bool,
    /// Encode on the CPU instead of the graphics card. Missing means off.
    pub cpu_mode: bool,
}

impl Default for TraySettings {
    fn default() -> Self {
        Self {
            start_at_login: true,
            audio: true,
            cpu_mode: false,
        }
    }
}

/// A connected viewer cannot request additional rights; revocation stays available.
pub fn admin_menu_enabled(admin_on: bool, viewer_connected: bool) -> bool {
    admin_on || !viewer_connected
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
/// here; they travel as environment variables. `audio` and `cpu_mode` come
/// from `TraySettings`; `--no-audio` is passed only when audio is off and
/// `--cpu-mode` only when CPU mode is on.
pub fn build_host_args(config: &StoredHostConfig, audio: bool, cpu_mode: bool) -> Vec<OsString> {
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
    if !audio {
        args.push(OsString::from("--no-audio"));
    }
    if cpu_mode {
        args.push(OsString::from("--cpu-mode"));
    }
    args
}

pub fn decode_hex(value: &str) -> Result<Vec<u8>> {
    let value = value.trim();
    if !value.len().is_multiple_of(2) {
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
    /// The agent is running but did not answer the hello in time. It may be
    /// slow or stalled, not out of date, so the update prompt is not offered;
    /// the tray keeps retrying.
    NotResponding { reason: String },
}

impl AgentStatus {
    pub fn needs_update(&self) -> bool {
        matches!(self, AgentStatus::NeedsUpdate { .. })
    }
}

/// Status after a failed probe of a running agent. Only a missed hello
/// deadline is "not responding"; every other failure keeps the update prompt.
pub fn running_probe_status(timed_out: bool, reason: String) -> AgentStatus {
    if timed_out {
        AgentStatus::NotResponding { reason }
    } else {
        AgentStatus::NeedsUpdate { reason }
    }
}

/// Minimum time between probes that are not forced.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// A probe still unreported after this long is treated as lost (its thread
/// died) and a new one may start. A live probe ends well before this.
pub const PROBE_LOST_AFTER: Duration = Duration::from_secs(60);

/// Single-flight and throttle state for the agent probe. The probe runs on a
/// background thread, so the tray asks this gate before starting one.
#[derive(Debug, Default)]
pub struct ProbeGate {
    next_ticket: u64,
    in_flight: Option<(u64, Instant)>,
    last_started: Option<Instant>,
}

impl ProbeGate {
    /// Starts a probe and returns its ticket, unless one is in flight or the
    /// throttle holds it back. A forced probe skips the throttle, never the
    /// in-flight check.
    pub fn try_start(&mut self, now: Instant, force: bool) -> Option<u64> {
        if let Some((_, started)) = self.in_flight {
            if now.saturating_duration_since(started) < PROBE_LOST_AFTER {
                return None;
            }
        }
        if !force
            && self
                .last_started
                .is_some_and(|at| now.saturating_duration_since(at) < PROBE_INTERVAL)
        {
            return None;
        }
        self.next_ticket += 1;
        self.in_flight = Some((self.next_ticket, now));
        self.last_started = Some(now);
        Some(self.next_ticket)
    }

    /// Records that the probe with `ticket` reported. A lost probe that
    /// reports late does not clear a newer probe in flight.
    pub fn finish(&mut self, ticket: u64) {
        if self.in_flight.is_some_and(|(current, _)| current == ticket) {
            self.in_flight = None;
        }
    }

    /// The agent was started or stopped: the next probe may run at once.
    pub fn reset_throttle(&mut self) {
        self.last_started = None;
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
        AgentStatus::NotResponding { .. } => "user input, admin agent not responding",
    };
    format!("BubblePlay: {} ({input})", state.label())
}

/// Tray icon resource for light taskbars (dark outline), linked by build.rs.
pub const TRAY_ICON_LIGHT: u16 = 2;
/// Tray icon resource for dark taskbars (light outline), linked by build.rs.
pub const TRAY_ICON_DARK: u16 = 3;

/// Picks the tray icon from `SystemUsesLightTheme`: 0 or missing means a dark taskbar.
pub fn tray_icon_resource(system_uses_light_theme: Option<u32>) -> u16 {
    match system_uses_light_theme {
        Some(0) | None => TRAY_ICON_DARK,
        Some(_) => TRAY_ICON_LIGHT,
    }
}

/// Lets one left-click through per double-click interval, so a double-click
/// (button-up, double-click, button-up) opens BubblePlay once.
#[derive(Debug, Default)]
pub struct ClickDebounce {
    last: Option<Instant>,
}

impl ClickDebounce {
    /// True when this click should act; `window` is the system double-click time.
    pub fn accept(&mut self, now: Instant, window: Duration) -> bool {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < window)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// The Admin mode line shown under the "Admin mode" menu item.
pub fn admin_mode_line(agent: &AgentStatus) -> Option<&'static str> {
    match agent {
        AgentStatus::Off => None,
        AgentStatus::Armed => Some("Admin mode: armed (starts when a viewer connects)"),
        AgentStatus::Starting => Some("Admin mode: starting (viewer connected)"),
        AgentStatus::Ready => Some("Admin mode: active"),
        AgentStatus::NeedsUpdate { .. } => Some("Admin mode: agent needs update"),
        AgentStatus::NotResponding { .. } => Some("Admin mode: agent not responding"),
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
/// the host's "viewer session started/ended" log lines and reconciled with
/// the current demand signal; a host exit ends every session.
#[derive(Debug, Clone, Default)]
pub struct ViewerGate {
    sessions: u32,
    idle_since: Option<Instant>,
}

impl ViewerGate {
    /// Reconcile with current host state, including after a tray restart or
    /// filtered/missed lifecycle logs. Repeated snapshots do not extend grace.
    pub fn observe_active(&mut self, active: bool, now: Instant) {
        if active {
            self.sessions = 1;
            self.idle_since = None;
        } else {
            self.on_host_exited(now);
        }
    }

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

// Tray menu command ids. TrackPopupMenu returns them; greyed lines reuse STATUS and BUILD.
pub const CMD_STATUS: usize = 1;
pub const CMD_ADMIN: usize = 2;
pub const CMD_LOGIN: usize = 3;
pub const CMD_RESTART: usize = 4;
pub const CMD_LOGS: usize = 5;
pub const CMD_QUIT: usize = 6;
pub const CMD_ROLLBACK: usize = 7;
pub const CMD_BUILD: usize = 8;
pub const CMD_AGENT_UPDATE: usize = 9;
pub const CMD_SECURE_HELPER_UPDATE: usize = 10;
pub const CMD_AUDIO: usize = 11;
pub const CMD_CPU_MODE: usize = 12;
pub const CMD_EXPORT_KEY: usize = 13;
pub const CMD_IMPORT_KEY: usize = 14;
/// "Open BubblePlay": opens the Devices window. Also what a left-click on the icon does.
pub const CMD_DEVICES: usize = 15;
/// "Report a problem": brings the app forward and opens its report sheet.
pub const CMD_FEEDBACK: usize = 16;

/// What the tray menu shows, gathered from the app when the menu opens.
#[derive(Debug, Clone, Default)]
pub struct MenuState {
    pub status: String,
    pub error: Option<String>,
    pub build: String,
    pub admin: bool,
    pub admin_enabled: bool,
    pub admin_line: Option<&'static str>,
    /// (enabled, label) when an admin-agent update item is shown.
    pub agent_update: Option<(bool, &'static str)>,
    pub helper_line: String,
    /// (enabled, label) when a privileged-helper update item is shown.
    pub helper_update: Option<(bool, &'static str)>,
    pub login: bool,
    pub audio: bool,
    pub cpu_mode: bool,
    pub key_busy: bool,
    pub can_roll_back: bool,
}

/// One tray menu entry, before it becomes a Win32 menu item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    /// `default` items show bold; disabled items are greyed.
    Item {
        id: usize,
        text: String,
        enabled: bool,
        checked: bool,
        default: bool,
    },
    Separator,
    Submenu {
        text: String,
        entries: Vec<MenuEntry>,
    },
}

impl MenuEntry {
    fn item(id: usize, text: &str, enabled: bool, checked: bool) -> Self {
        Self::Item {
            id,
            text: text.to_owned(),
            enabled,
            checked,
            default: false,
        }
    }

    fn command(id: usize, text: &str) -> Self {
        Self::item(id, text, true, false)
    }

    fn grey(id: usize, text: &str) -> Self {
        Self::item(id, text, false, false)
    }
}

/// The tray menu: Open BubblePlay and status on top, the everyday toggles, then More, then
/// Restart host and Quit. Settings, key transfer, logs and diagnostics live under More.
pub fn menu_layout(state: &MenuState) -> Vec<MenuEntry> {
    let mut top = vec![MenuEntry::Item {
        id: CMD_DEVICES,
        text: "Open BubblePlay".to_owned(),
        enabled: true,
        checked: false,
        default: true,
    }];
    top.push(MenuEntry::grey(CMD_STATUS, &state.status));
    if let Some(error) = &state.error {
        top.push(MenuEntry::grey(CMD_BUILD, error));
    }
    top.push(MenuEntry::Separator);
    top.push(MenuEntry::item(
        CMD_ADMIN,
        "Admin mode",
        state.admin_enabled,
        state.admin,
    ));
    if let Some(line) = state.admin_line {
        top.push(MenuEntry::grey(CMD_STATUS, line));
    }
    if let Some((enabled, label)) = state.agent_update {
        top.push(MenuEntry::item(CMD_AGENT_UPDATE, label, enabled, false));
    }
    if let Some((enabled, label)) = state.helper_update {
        top.push(MenuEntry::item(
            CMD_SECURE_HELPER_UPDATE,
            label,
            enabled,
            false,
        ));
    }
    top.push(MenuEntry::item(
        CMD_AUDIO,
        "Stream PC sound",
        true,
        state.audio,
    ));

    let keys = !state.key_busy;
    let mut more = vec![
        MenuEntry::item(CMD_LOGIN, "Start at login", true, state.login),
        MenuEntry::item(CMD_CPU_MODE, "CPU mode", true, state.cpu_mode),
        MenuEntry::Separator,
        MenuEntry::item(CMD_EXPORT_KEY, "Export PC key...", keys, false),
        MenuEntry::item(CMD_IMPORT_KEY, "Import PC key...", keys, false),
        MenuEntry::Separator,
    ];
    if state.can_roll_back {
        more.push(MenuEntry::command(CMD_ROLLBACK, "Roll back host"));
    }
    more.push(MenuEntry::command(CMD_FEEDBACK, "Report a problem"));
    more.push(MenuEntry::command(CMD_LOGS, "Open logs folder"));
    more.push(MenuEntry::Separator);
    more.push(MenuEntry::grey(CMD_BUILD, &state.build));
    more.push(MenuEntry::grey(CMD_STATUS, &state.helper_line));
    if let Some(line) = state.admin_line {
        more.push(MenuEntry::grey(CMD_STATUS, line));
    }
    top.push(MenuEntry::Submenu {
        text: "More".to_owned(),
        entries: more,
    });

    top.push(MenuEntry::Separator);
    top.push(MenuEntry::command(CMD_RESTART, "Restart host"));
    top.push(MenuEntry::command(CMD_QUIT, "Quit"));
    top
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Compact view of a menu for order assertions: ids, "-" for separators, "More[...]".
    fn menu_shape(entries: &[MenuEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                MenuEntry::Item { id, .. } => id.to_string(),
                MenuEntry::Separator => "-".to_owned(),
                MenuEntry::Submenu { text, entries } => {
                    format!("{text}[{}]", menu_shape(entries).join(" "))
                }
            })
            .collect()
    }

    #[test]
    fn tray_icon_follows_system_theme_value() {
        assert_eq!(tray_icon_resource(Some(1)), TRAY_ICON_LIGHT);
        assert_eq!(tray_icon_resource(Some(0)), TRAY_ICON_DARK);
        assert_eq!(tray_icon_resource(None), TRAY_ICON_DARK);
        assert_eq!(tray_icon_resource(Some(7)), TRAY_ICON_LIGHT);
    }

    #[test]
    fn click_debounce_lets_one_click_through_per_window() {
        let window = Duration::from_millis(500);
        let start = Instant::now();
        let mut debounce = ClickDebounce::default();
        assert!(debounce.accept(start, window));
        // The second button-up of a double-click.
        assert!(!debounce.accept(start + Duration::from_millis(200), window));
        assert!(debounce.accept(start + Duration::from_millis(700), window));
        assert!(!debounce.accept(start + Duration::from_millis(1100), window));
        assert!(debounce.accept(start + Duration::from_millis(1300), window));
    }

    #[test]
    fn menu_layout_minimal_order() {
        let state = MenuState {
            status: "Host running".into(),
            build: "Build 1".into(),
            helper_line: "Helper ok".into(),
            admin_enabled: true,
            ..MenuState::default()
        };
        let menu = menu_layout(&state);
        assert_eq!(
            menu_shape(&menu),
            [
                "15",
                "1",
                "-",
                "2",
                "11",
                "More[3 12 - 13 14 - 16 5 - 8 1]",
                "-",
                "4",
                "6"
            ]
        );
        let MenuEntry::Item {
            text,
            default,
            enabled,
            ..
        } = &menu[0]
        else {
            panic!("first entry is an item");
        };
        assert_eq!(text, "Open BubblePlay");
        assert!(*default && *enabled);
        let defaults = menu
            .iter()
            .filter(|entry| matches!(entry, MenuEntry::Item { default: true, .. }))
            .count();
        assert_eq!(defaults, 1);
    }

    #[test]
    fn report_a_problem_is_always_enabled_under_more() {
        let busy = MenuState {
            admin: true,
            admin_enabled: false,
            key_busy: true,
            ..MenuState::default()
        };
        for state in [MenuState::default(), busy] {
            let menu = menu_layout(&state);
            let more = menu
                .iter()
                .find_map(|entry| match entry {
                    MenuEntry::Submenu { entries, .. } => Some(entries),
                    _ => None,
                })
                .expect("More submenu");
            let item = more
                .iter()
                .find(|entry| matches!(entry, MenuEntry::Item { id, .. } if *id == CMD_FEEDBACK))
                .expect("Report a problem item");
            assert_eq!(
                item,
                &MenuEntry::Item {
                    id: CMD_FEEDBACK,
                    text: "Report a problem".to_owned(),
                    enabled: true,
                    checked: false,
                    default: false,
                }
            );
        }
    }

    #[test]
    fn menu_layout_full_order() {
        let state = MenuState {
            status: "Host stopped".into(),
            error: Some("Host error: boom".into()),
            build: "Build 1".into(),
            admin: true,
            admin_enabled: false,
            admin_line: Some("Admin agent not responding"),
            agent_update: Some((true, AGENT_UPDATE_LABEL)),
            helper_line: "Helper ok".into(),
            helper_update: Some((false, SECURE_HELPER_UPDATE_LABEL)),
            login: true,
            audio: true,
            cpu_mode: true,
            key_busy: true,
            can_roll_back: true,
        };
        let menu = menu_layout(&state);
        assert_eq!(
            menu_shape(&menu),
            [
                "15",
                "1",
                "8",
                "-",
                "2",
                "1",
                "9",
                "10",
                "11",
                "More[3 12 - 13 14 - 7 16 5 - 8 1 1]",
                "-",
                "4",
                "6"
            ]
        );
        // Open BubblePlay stays enabled when the host is down; key items grey while busy.
        assert!(matches!(&menu[0], MenuEntry::Item { enabled: true, .. }));
        assert!(matches!(
            &menu[4],
            MenuEntry::Item {
                enabled: false,
                checked: true,
                ..
            }
        ));
        let MenuEntry::Submenu { entries, .. } = &menu[9] else {
            panic!("More is a submenu");
        };
        assert!(matches!(
            &entries[3],
            MenuEntry::Item {
                id: CMD_EXPORT_KEY,
                enabled: false,
                ..
            }
        ));
    }

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
    fn child_lines_map_to_levels() {
        use tracing::Level;
        let level = |text: &str| child_line_level(text);
        // Tracing lines are already in the child's own JSONL log: never echoed.
        assert_eq!(level("2026-11-02T10:00:00Z  WARN bubbleplay_host: x"), None);
        assert_eq!(level("2026-11-02T10:00:00Z ERROR bubbleplay_host: x"), None);
        assert_eq!(level("2026-11-02T10:00:00Z  INFO bubbleplay_host: x"), None);
        assert_eq!(level("2026-11-02T10:00:00Z DEBUG a: b"), None);
        assert_eq!(
            level("thread 'main' panicked at src/main.rs:1:1:"),
            Some(Level::ERROR)
        );
        assert_eq!(
            level("   0: std::backtrace::Backtrace::capture"),
            Some(Level::WARN)
        );
        assert_eq!(
            level("The GPU driver reported: error 0x887a0005"),
            Some(Level::ERROR)
        );
        assert_eq!(
            level(
                "   
"
            ),
            None
        );
        assert_eq!(level(""), None);
    }

    #[test]
    fn admin_menu_disallows_enabling_during_viewer_session() {
        assert!(admin_menu_enabled(false, false));
        assert!(!admin_menu_enabled(false, true));
        assert!(admin_menu_enabled(true, false));
        assert!(admin_menu_enabled(true, true));
    }

    #[test]
    fn legacy_tray_json_admin_mode_is_ignored_and_never_written() {
        for legacy in ["true", "false", "null", "123", "\"invalid\""] {
            let settings: TraySettings = serde_json::from_str(&format!(
                "{{\"adminMode\":{legacy},\"startAtLogin\":false}}"
            ))
            .unwrap();
            assert!(!settings.start_at_login);
            assert_eq!(
                serde_json::to_value(settings).unwrap(),
                serde_json::json!({"startAtLogin":false,"audio":true,"cpuMode":false})
            );
        }
    }

    #[test]
    fn settings_default_roundtrip_and_tolerate_garbage() {
        let dir = temp_dir("settings");
        let path = dir.join("tray.json");
        assert_eq!(TraySettings::load(&path), TraySettings::default());
        assert!(TraySettings::default().start_at_login);
        let settings = TraySettings {
            start_at_login: false,
            audio: true,
            cpu_mode: false,
        };
        settings.save(&path).unwrap();
        assert_eq!(TraySettings::load(&path), settings);
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("adminMode"));
        fs::write(&path, b"{\"adminMode\": true}").unwrap();
        assert_eq!(
            TraySettings::load(&path),
            TraySettings {
                start_at_login: true,
                audio: true,
                cpu_mode: false,
            }
        );
        fs::write(
            &path,
            b"{\"adminMode\": \"ignored\", \"startAtLogin\": false}",
        )
        .unwrap();
        assert!(!TraySettings::load(&path).start_at_login);
        fs::write(&path, b"not json").unwrap();
        assert_eq!(TraySettings::load(&path), TraySettings::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn audio_setting_defaults_on_and_roundtrips_explicitly() {
        let dir = temp_dir("audio-setting");
        let path = dir.join("tray.json");
        // Missing field in an existing file: audio stays on.
        fs::write(&path, b"{\"startAtLogin\": false}").unwrap();
        assert_eq!(
            TraySettings::load(&path),
            TraySettings {
                start_at_login: false,
                audio: true,
                cpu_mode: false,
            }
        );
        // Explicit off survives a save and load.
        let off = TraySettings {
            start_at_login: true,
            audio: false,
            cpu_mode: false,
        };
        off.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"audio\": false"));
        assert_eq!(TraySettings::load(&path), off);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cpu_mode_setting_defaults_off_and_roundtrips_explicitly() {
        let dir = temp_dir("cpu-mode-setting");
        let path = dir.join("tray.json");
        // Missing field in an existing file: CPU mode stays off.
        fs::write(&path, b"{\"startAtLogin\": false, \"audio\": true}").unwrap();
        assert_eq!(
            TraySettings::load(&path),
            TraySettings {
                start_at_login: false,
                audio: true,
                cpu_mode: false,
            }
        );
        // Explicit on survives a save and load, and other fields are kept.
        let on = TraySettings {
            start_at_login: true,
            audio: true,
            cpu_mode: true,
        };
        on.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"cpuMode\": true"));
        assert_eq!(TraySettings::load(&path), on);
        // Unreadable file: CPU mode falls back to off.
        fs::write(&path, b"not json").unwrap();
        assert!(!TraySettings::load(&path).cpu_mode);
        let _ = fs::remove_dir_all(dir);
    }

    fn config() -> StoredHostConfig {
        StoredHostConfig {
            worker: "wss://play.example/host-ws".into(),
            token_dpapi_hex: "01AB".into(),
            monitor: None,
        }
    }

    #[test]
    fn host_args_never_include_secrets() {
        let args = build_host_args(&config(), true, false);
        assert_eq!(
            args,
            ["--mode", "serve", "--worker", "wss://play.example/host-ws"]
                .map(OsString::from)
                .to_vec()
        );
        assert!(!args.iter().any(|a| a == "--token"));
        assert!(!args.iter().any(|a| a.to_string_lossy().contains("01AB")));
        let mut with_monitor = config();
        with_monitor.monitor = Some("primary".into());
        let args = build_host_args(&with_monitor, true, false);
        assert_eq!(
            &args[4..],
            &[OsString::from("--monitor"), OsString::from("primary")]
        );
        with_monitor.monitor = Some(String::new());
        assert_eq!(build_host_args(&with_monitor, true, false).len(), 4);
    }

    #[test]
    fn host_args_pass_no_audio_only_when_audio_is_off() {
        let on = build_host_args(&config(), true, false);
        assert!(!on.iter().any(|a| a == "--no-audio"));
        let off = build_host_args(&config(), false, false);
        assert_eq!(
            off,
            [
                "--mode",
                "serve",
                "--worker",
                "wss://play.example/host-ws",
                "--no-audio",
            ]
            .map(OsString::from)
            .to_vec()
        );
        // With a monitor, --no-audio follows it.
        let mut with_monitor = config();
        with_monitor.monitor = Some("primary".into());
        assert_eq!(
            &build_host_args(&with_monitor, false, false)[4..],
            &[
                OsString::from("--monitor"),
                OsString::from("primary"),
                OsString::from("--no-audio"),
            ]
        );
    }

    #[test]
    fn host_args_pass_cpu_mode_only_when_cpu_mode_is_on() {
        let off = build_host_args(&config(), true, false);
        assert!(!off.iter().any(|a| a == "--cpu-mode"));
        let on = build_host_args(&config(), true, true);
        assert_eq!(
            on,
            [
                "--mode",
                "serve",
                "--worker",
                "wss://play.example/host-ws",
                "--cpu-mode",
            ]
            .map(OsString::from)
            .to_vec()
        );
        // With a monitor and audio off, --cpu-mode comes last.
        let mut with_monitor = config();
        with_monitor.monitor = Some("primary".into());
        assert_eq!(
            &build_host_args(&with_monitor, false, true)[4..],
            &[
                OsString::from("--monitor"),
                OsString::from("primary"),
                OsString::from("--no-audio"),
                OsString::from("--cpu-mode"),
            ]
        );
    }

    #[test]
    fn host_config_parses_service_shape_and_validates() {
        let dir = temp_dir("config");
        let path = dir.join("host.json");
        // A host.json from an older setup may still carry the removed host-gate field; it is ignored.
        fs::write(
            &path,
            br#"{"worker":"wss://w.example/host-ws","tokenDpapiHex":"0102","hostGateTokenDpapiHex":"CD"}"#,
        )
        .unwrap();
        let parsed = StoredHostConfig::load(&path).unwrap();
        assert_eq!(parsed.token_dpapi_hex, "0102");
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
    fn missed_hello_deadline_shows_not_responding_not_an_update() {
        let silent = running_probe_status(true, "the admin agent is not responding".into());
        assert_eq!(
            silent,
            AgentStatus::NotResponding {
                reason: "the admin agent is not responding".into()
            }
        );
        assert!(!silent.needs_update());
        assert_eq!(
            admin_mode_line(&silent),
            Some("Admin mode: agent not responding")
        );
        assert_eq!(
            status_line(&HostState::Running { pid: 1 }, &silent),
            "BubblePlay: Running (user input, admin agent not responding)"
        );
        // A version or protocol problem, or any other failure, still asks for the update.
        let stale = running_probe_status(false, "speaks protocol 1-1".into());
        assert_eq!(
            stale,
            AgentStatus::NeedsUpdate {
                reason: "speaks protocol 1-1".into()
            }
        );
        assert!(stale.needs_update());
    }

    #[test]
    fn probe_in_flight_means_no_second_probe() {
        let t0 = Instant::now();
        let mut gate = ProbeGate::default();
        let first = gate.try_start(t0, true).expect("first probe starts");
        // Neither a forced probe nor a due one starts while the first is out.
        assert_eq!(gate.try_start(t0, true), None);
        assert_eq!(gate.try_start(t0 + Duration::from_secs(10), false), None);
        gate.finish(first);
        // After it reported, the throttle still applies to unforced probes.
        assert_eq!(gate.try_start(t0 + Duration::from_secs(1), false), None);
        assert!(gate.try_start(t0 + Duration::from_secs(1), true).is_some());
        // A reset (agent started or stopped) lets an unforced probe run at once.
        let mut fresh = ProbeGate::default();
        let ticket = fresh.try_start(t0, true).expect("probe starts");
        fresh.finish(ticket);
        fresh.reset_throttle();
        assert!(fresh.try_start(t0, false).is_some());
    }

    #[test]
    fn lost_probe_is_replaced_and_its_late_report_is_ignored() {
        let t0 = Instant::now();
        let mut gate = ProbeGate::default();
        let lost = gate.try_start(t0, true).expect("probe starts");
        let later = t0 + PROBE_LOST_AFTER + Duration::from_secs(1);
        let fresh = gate
            .try_start(later, false)
            .expect("a lost probe must not block forever");
        // The lost probe reports late: the newer one is still in flight.
        gate.finish(lost);
        assert_eq!(gate.try_start(later, true), None);
        gate.finish(fresh);
        assert!(gate.try_start(later, true).is_some());
    }

    #[test]
    fn viewer_snapshot_recovers_without_an_edge_and_idle_does_not_extend_grace() {
        let now = Instant::now();
        let mut gate = ViewerGate::default();
        gate.observe_active(true, now);
        assert!(gate.viewer_connected());
        gate.observe_active(false, now + Duration::from_secs(1));
        assert!(!gate.viewer_connected());
        assert!(gate.wants_agent(now + Duration::from_secs(29)));
        gate.observe_active(false, now + Duration::from_secs(30));
        assert!(!gate.wants_agent(now + Duration::from_secs(31)));
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
            assert!(admin_menu_enabled(admin_mode, false));
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
