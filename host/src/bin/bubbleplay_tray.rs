//! BubblePlay notification-area app. It always runs
//! `bubbleplay-host.exe --mode serve` as the signed-in user (local DXGI
//! capture + local SendInput) and restarts it with backoff. "Admin mode"
//! additionally starts the small elevated admin input agent through the
//! on-demand `\BubblePlay\AdminAgent` scheduled task; the host then sends its
//! input records to the agent over a local named pipe so input reaches
//! elevated windows, and falls back to user-level SendInput whenever the
//! agent is missing or speaks another protocol version.
//!
//! Modes:
//!   bubbleplay_tray.exe          tray (single instance)
//!   bubbleplay_tray.exe --quit   ask a running tray to exit
//!   bubbleplay_tray.exe --send-files <paths...>
//!                                hand files to the running host as a file
//!                                offer (Explorer "Send to > BubblePlay")
#![windows_subsystem = "windows"]

#[path = "../tray/logic.rs"]
mod logic;
#[path = "../tray/platform.rs"]
mod platform;
#[path = "../tray/supervisor.rs"]
mod supervisor;
#[path = "../tray/update.rs"]
mod update;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, NOTIFY_ICON_INFOTIP_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, GetSystemMetrics,
    LoadImageW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SetProcessDPIAware, SetTimer, TrackPopupMenu, TranslateMessage, HICON,
    IMAGE_ICON, LR_DEFAULTCOLOR, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, SM_CXSMICON,
    SM_CYSMICON, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_DESTROY,
    WM_ENDSESSION, WM_LBUTTONUP, WM_NULL, WM_QUERYENDSESSION, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
    WS_OVERLAPPED,
};

use bubbleplay_host::admin_agent::client::{probe, ProbeResult, AGENT_EXE};
use bubbleplay_host::admin_agent::demand::{active_from_env, publish_failure_in, StartRetry};
use bubbleplay_host::admin_agent::protocol::{
    pipe_name, scoped_name, ENV_ALLOW_DEV_AGENT, ENV_PIPE, PROTOCOL_MAX, PROTOCOL_MIN,
};
use bubbleplay_host::admin_agent::trust::current_session_id;
use bubbleplay_host::secure_helper::client::{probe as probe_secure_helper, LinkError};
use bubbleplay_host::secure_helper::protocol::{
    PROTOCOL_MAX as SECURE_PROTOCOL_MAX, PROTOCOL_MIN as SECURE_PROTOCOL_MIN,
};
use logic::{
    admin_menu_enabled, admin_mode_line, run_key_command, secure_helper_should_launch,
    start_secure_helper_for_viewer, status_line, unix_ms_now, AgentStatus, HostState,
    SecureHelperStatus, SharedLog, TraySettings, ViewerGate, AGENT_UPDATE_LABEL,
    SECURE_HELPER_UPDATE_LABEL,
};
use platform::{
    acquire_single_instance, create_manual_event, data_dir, end_agent_task, ensure_agent_task,
    host_exe_beside, named_mutex_exists, open_folder, read_run_value, run_admin_mode_off_task,
    run_agent_task, run_elevated_and_wait, run_secure_helper_task, secure_helper_task_exists,
    set_event, set_run_value, sha256_file, wide, Handle, AGENT_MUTEX, AGENT_STOP_EVENT,
    SECURE_HELPER_TASK, TRAY_MUTEX,
};
use supervisor::{HostEvent, SupervisorConfig, SupervisorThread};
use update::{
    plan_rollback, short_hash, Probation, RollbackPlan, UpdateState, UpdateStatus, Verdict,
};

const WINDOW_CLASS: &str = "BubblePlayTrayWindow";
const WM_TRAY: u32 = WM_APP + 1;
const WM_QUIT_REQUEST: u32 = WM_APP + 2;
/// Posted when the elevated agent update finished; WPARAM = exit code.
const WM_AGENT_UPDATED: u32 = WM_APP + 3;
const WM_ADMIN_CHANGED: u32 = WM_APP + 4;
const TIMER_ID: usize = 1;

const CMD_STATUS: usize = 1;
const CMD_ADMIN: usize = 2;
const CMD_LOGIN: usize = 3;
const CMD_RESTART: usize = 4;
const CMD_LOGS: usize = 5;
const CMD_QUIT: usize = 6;
const CMD_ROLLBACK: usize = 7;
const CMD_BUILD: usize = 8;
const CMD_AGENT_UPDATE: usize = 9;
const CMD_SECURE_HELPER_UPDATE: usize = 10;

/// Time the secure helper gets to answer after its task was started.
const AGENT_START_GRACE: Duration = Duration::from_secs(15);
/// How often the secure helper is re-probed (version + trust).
const AGENT_PROBE_INTERVAL: Duration = Duration::from_secs(30);
/// Left behind by the removed elevated-host runner.
const STALE_HEARTBEAT_FILE: &str = "elevated-runner.json";

fn main() {
    let mut args = std::env::args().skip(1);
    let code = match args.next().as_deref() {
        None => run_tray(),
        Some("--quit") => quit_running_tray(),
        Some("--send-files") => send_files(args.map(PathBuf::from).collect()),
        Some(_) => Ok(2),
    };
    std::process::exit(code.unwrap_or(1));
}

/// Named-object / window-class name for this instance. With
/// `BUBBLEPLAY_TRAY_DATA_DIR` set (test mode) names are suffixed by the data
/// dir, so a test tray never collides with (or `--quit`s) the real one.
fn scoped(base: &str) -> String {
    scoped_name(
        base,
        std::env::var_os("BUBBLEPLAY_TRAY_DATA_DIR").as_deref(),
    )
}

/// The admin agent's pipe for this session (scoped like the other names).
fn agent_pipe() -> String {
    pipe_name(
        current_session_id(),
        std::env::var_os("BUBBLEPLAY_TRAY_DATA_DIR").as_deref(),
    )
}

fn logs_dir(data: &Path) -> PathBuf {
    data.join("logs")
}

fn init_tracing(log: SharedLog) {
    let _ = tracing_subscriber::fmt()
        .with_writer(move || log.clone())
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}

fn supervisor_config(
    data: &Path,
    host_log: SharedLog,
    events: Option<std::sync::mpsc::Sender<HostEvent>>,
    test_mode: bool,
) -> Result<SupervisorConfig> {
    let exe = std::env::current_exe().context("current_exe")?;
    let mut extra_env = vec![
        (ENV_PIPE.to_string(), agent_pipe()),
        (
            "BUBBLEPLAY_SENDTO_DIR".to_string(),
            sendto_dir().to_string_lossy().into_owned(),
        ),
    ];
    if test_mode {
        // Isolated test instance: its agent is an unelevated --dev build.
        extra_env.push((ENV_ALLOW_DEV_AGENT.to_string(), "1".to_string()));
    }
    Ok(SupervisorConfig {
        host_exe: host_exe_beside(&exe),
        host_config: data.join("host.json"),
        host_log,
        events,
        extra_env,
    })
}

// ---------------------------------------------------------------------------
// --quit
// ---------------------------------------------------------------------------

/// Where `--send-files` spools requests for the host (`file_service`).
fn sendto_dir() -> PathBuf {
    data_dir().join("sendto")
}

/// Explorer "Send to > BubblePlay": write a spool request the running host
/// turns into a file offer. Nothing is sent until the client downloads.
fn send_files(paths: Vec<PathBuf>) -> Result<i32> {
    let paths: Vec<PathBuf> = paths
        .into_iter()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| std::path::absolute(&path).unwrap_or(path))
        .collect();
    if paths.is_empty() {
        return Ok(2);
    }
    bubbleplay_host::file_service::write_spool_request(&sendto_dir(), &paths)?;
    Ok(0)
}

fn quit_running_tray() -> Result<i32> {
    let class = wide(scoped(WINDOW_CLASS));
    // SAFETY: class is NUL-terminated.
    let Ok(hwnd) = (unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) }) else {
        return Ok(0);
    };
    // SAFETY: posting to a window owned by another process of the same user.
    let _ = unsafe { PostMessageW(Some(hwnd), WM_QUIT_REQUEST, WPARAM(0), LPARAM(0)) };
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if !named_mutex_exists(&scoped(TRAY_MUTEX)) {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(1)
}

// ---------------------------------------------------------------------------
// Tray
// ---------------------------------------------------------------------------

/// Owned copy of the embedded BubblePlay mark at the system's small-icon size.
/// Resource ID 1 is linked into this executable by build.rs.
struct TrayIcon {
    handle: HICON,
    size: (i32, i32),
}

impl TrayIcon {
    fn small_size() -> (i32, i32) {
        // SAFETY: these metrics need no pointers. run_tray enables system DPI awareness.
        unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) }
    }

    fn load(size: (i32, i32)) -> Result<Self> {
        // SAFETY: module is our executable; PCWSTR(1) is MAKEINTRESOURCEW(1).
        // Omitting LR_SHARED gives this object ownership of the loaded icon.
        let handle = unsafe {
            let module = GetModuleHandleW(PCWSTR::null()).context("GetModuleHandleW")?;
            LoadImageW(
                Some(module.into()),
                // MAKEINTRESOURCE(1): the icon resource ID, not a pointer. (`dangling()` is 2.)
                PCWSTR(std::ptr::without_provenance::<u16>(1)),
                IMAGE_ICON,
                size.0,
                size.1,
                LR_DEFAULTCOLOR,
            )
            .context("loading embedded BubblePlay tray icon")?
        };
        Ok(Self {
            handle: HICON(handle.0),
            size,
        })
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        // SAFETY: LoadImageW created this non-shared icon, destroyed exactly once.
        let _ = unsafe { DestroyIcon(self.handle) };
    }
}

fn copy_wide(dst: &mut [u16], text: &str) {
    let encoded: Vec<u16> = text.encode_utf16().take(dst.len() - 1).collect();
    dst[..encoded.len()].copy_from_slice(&encoded);
    dst[encoded.len()] = 0;
}

struct App {
    hwnd: HWND,
    data: PathBuf,
    settings_path: PathBuf,
    settings: TraySettings,
    admin_on: bool,
    admin_change_running: bool,
    test_mode: bool,
    host_log: SharedLog,
    user: Option<SupervisorThread>,
    agent: AgentStatus,
    secure_helper: SecureHelperStatus,
    helper_started: Option<Instant>,
    helper_last_probe: Option<Instant>,
    /// Viewer state reconciled with the host demand signal on every tick.
    viewers: ViewerGate,
    /// When the agent task was started or an existing agent was adopted.
    agent_started: Option<Instant>,
    agent_retry: StartRetry,
    last_probe: Option<Instant>,
    /// Test mode only: the unelevated `--dev` agent the tray spawned.
    agent_child: Option<std::process::Child>,
    agent_update_running: bool,
    /// Show the "Admin mode on" balloon once the agent is ready.
    announce_ready: bool,
    agent_pipe: String,
    stop_event: Handle,
    icon: TrayIcon,
    shown: Option<String>,
    taskbar_created: u32,
    shut_down: bool,
    events_tx: std::sync::mpsc::Sender<HostEvent>,
    events_rx: std::sync::mpsc::Receiver<HostEvent>,
    /// Active while update-state.json says the installed host is pending.
    probation: Option<Probation>,
    /// Exit times of the user host, to flag a failing rolled-back build.
    exits: Vec<u64>,
    host_hash: Option<String>,
    host_error: Option<String>,
}

impl App {
    fn refresh_icon(&mut self) {
        let size = TrayIcon::small_size();
        if size != self.icon.size {
            match TrayIcon::load(size) {
                Ok(icon) => {
                    self.icon = icon;
                    self.shown = None;
                }
                Err(error) => tracing::warn!(?error, "reloading tray icon failed"),
            }
        }
    }

    fn notify_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        }
    }

    fn add_icon(&mut self) {
        self.refresh_icon();
        let state = self.state();
        let tip = status_line(&state, &self.agent);
        let mut data = self.notify_data();
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = self.icon.handle;
        copy_wide(&mut data.szTip, &tip);
        // SAFETY: data is fully initialized.
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool();
        tracing::info!(added, "notification icon registered");
        self.shown = added.then_some(tip);
    }

    fn remove_icon(&self) {
        let data = self.notify_data();
        // SAFETY: data identifies our icon.
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
    }

    fn balloon(&self, title: &str, text: &str, flags: NOTIFY_ICON_INFOTIP_FLAGS) {
        let mut data = self.notify_data();
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = flags;
        copy_wide(&mut data.szInfoTitle, title);
        copy_wide(&mut data.szInfo, text);
        // SAFETY: data is fully initialized.
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
    }

    fn save_settings(&self) {
        if let Err(error) = self.settings.save(&self.settings_path) {
            tracing::warn!(?error, "saving tray.json failed");
        }
    }

    fn sync_run_key(&self) {
        if self.test_mode {
            return;
        }
        let result = if self.settings.start_at_login {
            match std::env::current_exe() {
                Ok(exe) => {
                    let command = run_key_command(&exe);
                    if read_run_value().as_deref() == Some(command.as_str()) {
                        Ok(())
                    } else {
                        set_run_value(Some(&command))
                    }
                }
                Err(error) => Err(error.into()),
            }
        } else {
            set_run_value(None)
        };
        if let Err(error) = result {
            tracing::warn!(?error, "updating HKCU Run entry failed");
        }
    }

    fn start_user(&mut self) {
        if self.user.is_some() {
            return;
        }
        match supervisor_config(
            &self.data,
            self.host_log.clone(),
            Some(self.events_tx.clone()),
            self.test_mode,
        ) {
            Ok(config) => {
                tracing::info!(exe = %config.host_exe.display(), "starting user-level host");
                self.host_hash = sha256_file(&config.host_exe);
                self.begin_probation_if_pending();
                self.user = Some(SupervisorThread::start(config));
            }
            Err(error) => tracing::error!(?error, "cannot start host supervisor"),
        }
    }

    fn stop_user(&mut self) {
        if let Some(mut user) = self.user.take() {
            tracing::info!("stopping user-level host");
            user.stop();
        }
    }

    fn state(&self) -> HostState {
        match &self.user {
            Some(user) => user.state(),
            None => HostState::Stopped,
        }
    }

    fn start_secure_helper(&mut self, trigger: &str) {
        if self.test_mode {
            tracing::debug!(trigger, "test mode: secure helper task launch skipped");
            return;
        }
        // Do not pre-probe or gate on the previous helper status: each new
        // viewer must attempt the installed task even after a prior failure.
        let result = start_secure_helper_for_viewer(
            &mut self.secure_helper,
            self.agent_update_running,
            || {
                run_secure_helper_task()
                    .map_err(|error| format!("secure helper did not launch: {error:#}"))
            },
        );
        match result {
            Ok(true) => {
                self.helper_started = Some(Instant::now());
                self.helper_last_probe = None;
                tracing::info!(
                    trigger,
                    task = SECURE_HELPER_TASK,
                    admin_mode = self.admin_on,
                    "secure helper task started"
                );
            }
            Ok(false) => tracing::info!(
                trigger,
                "secure helper start deferred during privileged helper update"
            ),
            Err(reason) => {
                self.helper_started = None;
                self.helper_last_probe = None;
                tracing::warn!(trigger, %reason, "secure helper task start failed");
            }
        }
    }

    /// Probe never claims the host role or changes the helper lifetime. All
    /// probes verify the protected image, SYSTEM token and session identity.
    fn check_secure_helper(&mut self, force_probe: bool) {
        if self.test_mode || self.agent_update_running {
            return;
        }
        if !self.viewers.viewer_connected() {
            if !self.secure_helper.needs_update() {
                self.secure_helper = SecureHelperStatus::Armed;
            }
            self.helper_started = None;
            self.helper_last_probe = None;
            return;
        }
        if secure_helper_should_launch(true, &self.secure_helper, false) {
            self.start_secure_helper("active viewer retry");
        }
        if self.secure_helper.needs_update() {
            return;
        }
        let due = force_probe
            || self.secure_helper == SecureHelperStatus::Starting
            || self
                .helper_last_probe
                .is_none_or(|at| at.elapsed() >= AGENT_PROBE_INTERVAL);
        if !due {
            return;
        }
        let first_probe = self.helper_last_probe.is_none();
        self.helper_last_probe = Some(Instant::now());
        match probe_secure_helper() {
            Ok(()) => {
                if self.secure_helper != SecureHelperStatus::Ready || force_probe {
                    tracing::info!("secure helper probe ready (version and identity verified)");
                } else {
                    tracing::debug!("secure helper probe ready");
                }
                self.secure_helper = SecureHelperStatus::Ready;
            }
            Err(LinkError::Unavailable)
                if self
                    .helper_started
                    .is_some_and(|at| at.elapsed() < AGENT_START_GRACE) =>
            {
                if first_probe {
                    tracing::info!("secure helper probe unavailable; waiting for task startup");
                } else {
                    tracing::debug!("secure helper probe unavailable during startup grace");
                }
            }
            Err(LinkError::Unavailable) if self.secure_helper == SecureHelperStatus::Ready => {
                // Re-arm once; the next tick retries launch. Failure is then
                // reported instead of endlessly restarting a SYSTEM process.
                tracing::warn!(
                    "secure helper probe unavailable; will retry task for active viewer"
                );
                self.secure_helper = SecureHelperStatus::Armed;
            }
            Err(error) => {
                let reason = Self::helper_error(error);
                tracing::warn!(%reason, "secure helper probe failed; helper needs update");
                self.secure_helper = SecureHelperStatus::NeedsUpdate { reason };
            }
        }
    }

    fn helper_error(error: LinkError) -> String {
        match error {
            LinkError::Busy => "the secure helper already has a host client".into(),
            LinkError::Unavailable => "the secure helper did not answer".into(),
            LinkError::Incompatible { helper_min, helper_max } => format!("secure helper protocol {helper_min}-{helper_max}; host needs {SECURE_PROTOCOL_MIN}-{SECURE_PROTOCOL_MAX}"),
            LinkError::Untrusted => "secure helper identity verification failed".into(),
            LinkError::Protocol => "secure helper protocol verification failed".into(),
        }
    }

    fn agent_running(&self) -> bool {
        named_mutex_exists(&scoped(AGENT_MUTEX))
    }

    /// Start the agent: the scheduled task normally, or (test mode) an
    /// unelevated `--dev` agent beside the tray.
    fn launch_agent(&mut self) -> std::result::Result<(), String> {
        set_event(&self.stop_event, false);
        if self.test_mode {
            if let Some(child) = self.agent_child.as_mut() {
                if matches!(child.try_wait(), Ok(None)) {
                    return Ok(());
                }
            }
            let exe = std::env::current_exe().map_err(|error| error.to_string())?;
            let agent = exe.with_file_name(AGENT_EXE);
            let child = std::process::Command::new(&agent)
                .arg("--dev")
                .spawn()
                .map_err(|error| format!("starting {}: {error}", agent.display()))?;
            self.agent_child = Some(child);
            return Ok(());
        }
        ensure_agent_task().map_err(|error| format!("{error:#}"))?;
        run_agent_task().map_err(|error| format!("{error:#}"))
    }

    fn set_agent(&mut self, status: AgentStatus) {
        if self.agent == status {
            return;
        }
        match &status {
            AgentStatus::NeedsUpdate { reason } => {
                tracing::warn!(%reason, "admin agent needs update; host input stays at user level");
                self.balloon(
                    "BubblePlay",
                    &format!(
                        "Admin mode: {reason}. Input stays at user level until you choose \"{AGENT_UPDATE_LABEL}\"."
                    ),
                    NIIF_WARNING,
                );
            }
            AgentStatus::Ready if self.announce_ready => {
                self.announce_ready = false;
                self.balloon(
                    "BubblePlay",
                    "Admin mode on: input can reach elevated windows. An installed secure-desktop helper also allows input on UAC prompts and the lock screen.",
                    NIIF_INFO,
                );
            }
            _ => {}
        }
        self.agent = status;
    }

    fn start_agent(&mut self) {
        tracing::warn!(task = platform::AGENT_TASK, pipe = %self.agent_pipe, "admin agent start attempt");
        match self.launch_agent() {
            Ok(()) => {
                tracing::warn!(
                    task = platform::AGENT_TASK,
                    "admin agent start attempt accepted"
                );
                self.agent_started = Some(Instant::now());
                self.last_probe = None;
                self.set_agent(AgentStatus::Starting);
            }
            Err(reason) => {
                tracing::warn!(%reason, "admin agent start attempt failed");
                self.report_agent_failure(&reason);
                self.agent_started = None;
                self.set_agent(AgentStatus::NeedsUpdate { reason });
            }
        }
        self.agent_retry.attempt_finished(Instant::now());
    }

    fn report_agent_failure(&self, reason: &str) {
        if let Err(error) = publish_failure_in(&self.data, Some(reason)) {
            tracing::warn!(%error, "cannot publish admin agent input diagnostic");
        }
    }

    /// Stop a running agent; the host falls back to user-level input.
    fn stop_agent_process(&mut self) {
        set_event(&self.stop_event, true);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && self.agent_running() {
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Some(mut child) = self.agent_child.take() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.kill();
            }
            let _ = child.wait();
        } else if !self.test_mode && self.agent_started.is_some() {
            end_agent_task();
        }
        self.agent_started = None;
        self.last_probe = None;
    }

    fn stop_agent(&mut self) {
        if self.agent_started.is_some() || self.agent_child.is_some() {
            tracing::info!("stopping admin agent");
            self.stop_agent_process();
        }
        self.set_agent(AgentStatus::Off);
    }

    /// Admin mode is on and a viewer session is active (or ended less than
    /// the idle grace ago).
    fn agent_wanted(&self) -> bool {
        self.admin_on && self.viewers.wants_agent(Instant::now())
    }

    /// Keep an already running agent during idle grace, but only start or
    /// retry while the host currently reports an active viewer.
    fn gate_agent(&mut self) {
        self.viewers
            .observe_active(active_from_env(), Instant::now());
        if !self.admin_on {
            self.stop_agent();
            let _ = publish_failure_in(&self.data, None);
            return;
        }
        if self.agent_update_running || self.agent_wanted() {
            return;
        }
        if self.agent_started.is_some() || self.agent_child.is_some() {
            tracing::info!("no viewer session: stopping admin agent");
            self.stop_agent_process();
        }
        let _ = publish_failure_in(&self.data, None);
        if !self.agent.needs_update() {
            self.set_agent(AgentStatus::Armed);
        }
    }

    /// Current demand, not a log edge or prior status, drives recovery. A
    /// running agent is never launched twice, even if its hello is silent.
    fn check_agent(&mut self, force_probe: bool) {
        let needed = self.admin_on && active_from_env();
        if !needed || self.agent_update_running {
            return;
        }
        let now = Instant::now();
        let running = self.agent_running();
        let due = force_probe
            || self
                .last_probe
                .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(5));
        if !due {
            return;
        }
        self.last_probe = Some(now);
        match probe(&self.agent_pipe, self.test_mode) {
            ProbeResult::Ready(handshake) => {
                if self.agent != AgentStatus::Ready {
                    tracing::warn!(
                        version = handshake.version,
                        agent_pid = handshake.agent_pid,
                        elevated = handshake.elevated,
                        "admin agent ready"
                    );
                }
                let _ = publish_failure_in(&self.data, None);
                self.agent_started.get_or_insert(now);
                self.set_agent(AgentStatus::Ready);
            }
            ProbeResult::Incompatible {
                agent_min,
                agent_max,
            } => {
                let reason = format!("the admin agent speaks protocol {agent_min}-{agent_max}, the host needs {PROTOCOL_MIN}-{PROTOCOL_MAX}");
                self.report_agent_failure(&reason);
                self.set_agent(AgentStatus::NeedsUpdate { reason });
            }
            ProbeResult::Unavailable(error) => {
                let retry = self
                    .agent_retry
                    .should_start(Instant::now(), needed, running);
                if !retry && !running {
                    return;
                }
                let result = if self.test_mode || (self.agent_started.is_none() && !running) {
                    String::new()
                } else {
                    platform::agent_task_result()
                };
                let reason = format!("the admin agent does not answer ({error}){result}");
                self.report_agent_failure(&reason);
                // Keep transient startup failure in diagnostics without a new balloon.
                if self.agent != AgentStatus::Starting {
                    tracing::warn!(%reason, "admin agent probe failed");
                }
                if retry {
                    tracing::warn!(%reason, "admin agent needed; scheduling start");
                    self.start_agent();
                } else if running {
                    self.set_agent(AgentStatus::NeedsUpdate { reason });
                }
            }
        }
    }

    /// Install/update the agent with one elevated setup run (one UAC).
    fn update_agent(&mut self) {
        if self.agent_update_running {
            return;
        }
        self.stop_agent_process();
        let hwnd = self.hwnd.0 as isize;
        let post = move |code: u32| {
            // SAFETY: posting to our own window (it outlives the process's
            // message loop; a failed post only loses the notification).
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(hwnd as *mut _)),
                    WM_AGENT_UPDATED,
                    WPARAM(code as usize),
                    LPARAM(0),
                )
            };
        };
        if self.test_mode {
            tracing::info!("test mode: skipping the elevated agent update");
            post(0);
            return;
        }
        let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        else {
            return;
        };
        let script = dir.join("setup-tray.ps1");
        if !script.is_file() {
            self.set_agent(AgentStatus::NeedsUpdate {
                reason: format!("{} is missing; run setup-tray.ps1 again", script.display()),
            });
            return;
        }
        let system = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let powershell =
            PathBuf::from(system).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let params = format!(
            "-NoProfile -ExecutionPolicy Bypass -File \"{}\" -AgentOnly -SourceDir \"{}\"",
            script.display(),
            dir.display()
        );
        tracing::info!(%params, "running the elevated admin agent update");
        self.agent_update_running = true;
        let spawned = std::thread::Builder::new()
            .name("agent-update".into())
            .spawn(move || {
                let code = match run_elevated_and_wait(&powershell, &params, &dir) {
                    Ok(code) => code,
                    Err(error) => {
                        tracing::warn!(?error, "elevated agent update did not run");
                        u32::MAX
                    }
                };
                post(code);
            });
        if spawned.is_err() {
            self.agent_update_running = false;
        }
    }

    fn agent_updated(&mut self, code: u32) {
        self.agent_update_running = false;
        self.refresh_admin_mode();
        if code == 0 {
            tracing::info!("privileged helper update finished");
            self.secure_helper = if self.test_mode || secure_helper_task_exists() {
                SecureHelperStatus::Armed
            } else {
                SecureHelperStatus::NeedsUpdate {
                    reason: "the secure helper is not installed".into(),
                }
            };
            self.helper_started = None;
            self.helper_last_probe = None;
            if self.agent_wanted() {
                self.announce_ready = true;
                self.check_agent(true);
            } else if self.admin_on {
                self.announce_ready = true;
                self.set_agent(AgentStatus::Armed);
            } else {
                self.set_agent(AgentStatus::Off);
            }
        } else {
            let reason = if code == u32::MAX {
                "the admin prompt was declined or failed".to_string()
            } else {
                format!("the agent update failed (exit {code}; see receipts\\setup-tray-machine-*.json)")
            };
            self.secure_helper = SecureHelperStatus::NeedsUpdate {
                reason: reason.clone(),
            };
            self.set_agent(AgentStatus::NeedsUpdate { reason });
        }
        self.tick();
    }

    fn apply_mode(&mut self) {
        self.refresh_admin_mode();
        self.start_user();
        self.gate_agent();
    }

    fn refresh_admin_mode(&mut self) {
        let enabled = bubbleplay_host::admin_mode::effective_for_console();
        if self.admin_on != enabled {
            self.admin_on = enabled;
            self.announce_ready = enabled;
            self.gate_agent();
        }
    }

    fn change_admin_mode(&mut self) {
        self.refresh_admin_mode();
        if self.admin_change_running
            || !admin_menu_enabled(self.admin_on, self.viewers.viewer_connected())
            || self.test_mode
        {
            return;
        }
        let turn_on = !self.admin_on;
        let Some(helper) = bubbleplay_host::secure_helper::trust::installed_image() else {
            tracing::warn!("admin mode helper path unavailable");
            return;
        };
        // The elevated command must always use the protected installed image.
        if turn_on {
            if let Err(error) = bubbleplay_host::secure_helper::trust::protected_image(&helper) {
                tracing::warn!(%error, "admin mode helper unavailable");
                return;
            }
        }
        self.admin_change_running = true;
        let window = self.hwnd.0 as usize;
        let spawned = std::thread::Builder::new()
            .name("admin-mode".into())
            .spawn(move || {
                let result = if turn_on {
                    run_elevated_and_wait(&helper, "--set-admin-mode on", helper.parent().unwrap())
                        .and_then(|code| {
                            anyhow::ensure!(code == 0, "admin mode setter exit {code}");
                            Ok(())
                        })
                } else {
                    run_admin_mode_off_task().and_then(|()| {
                        // schtasks returns after dispatch. Wait for the protected
                        // policy to reflect the task's write before refreshing.
                        let deadline = Instant::now() + Duration::from_secs(10);
                        while bubbleplay_host::admin_mode::effective_for_console() {
                            anyhow::ensure!(
                                Instant::now() < deadline,
                                "AdminModeOff task timed out"
                            );
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        Ok(())
                    })
                };
                if let Err(error) = result {
                    tracing::info!(%error, "admin mode change declined or failed");
                }
                let _ = unsafe {
                    PostMessageW(
                        Some(HWND(window as _)),
                        WM_ADMIN_CHANGED,
                        WPARAM(0),
                        LPARAM(0),
                    )
                };
            });
        if spawned.is_err() {
            self.admin_change_running = false;
        }
    }

    fn update_state_path(&self) -> PathBuf {
        self.data.join("update-state.json")
    }

    fn host_paths(&self) -> Option<(PathBuf, PathBuf, PathBuf)> {
        let exe = std::env::current_exe().ok()?;
        let host = host_exe_beside(&exe);
        let previous_dir = host.parent()?.join("previous");
        Some((
            host,
            previous_dir.join("bubbleplay-host.exe"),
            previous_dir.join("bubbleplay-host.failed.exe"),
        ))
    }

    fn previous_hash(&self) -> Option<String> {
        let (_, previous, _) = self.host_paths()?;
        sha256_file(&previous)
    }

    /// A previous host exists and differs from the installed one.
    fn can_roll_back(&self) -> bool {
        match (self.previous_hash(), self.host_hash.as_deref()) {
            (Some(previous), Some(current)) => !previous.eq_ignore_ascii_case(current),
            (Some(_), None) => true,
            _ => false,
        }
    }

    fn begin_probation_if_pending(&mut self) {
        if self.probation.is_some() {
            return;
        }
        let Some(hash) = self.host_hash.as_deref() else {
            return;
        };
        if let Some(state) = UpdateState::load(&self.update_state_path()) {
            if state.needs_probation(hash) {
                tracing::info!(
                    build = short_hash(hash),
                    "host update pending: probation started"
                );
                self.probation = Some(Probation::new(unix_ms_now()));
            }
        }
    }

    fn build_line(&self) -> String {
        let build = self
            .host_hash
            .as_deref()
            .map(short_hash)
            .unwrap_or("unknown");
        let status = match UpdateState::load(&self.update_state_path()) {
            Some(state) => match state.status {
                UpdateStatus::Pending if self.probation.is_some() => "pending (probation)",
                UpdateStatus::Pending => "pending",
                UpdateStatus::Good => "good",
                UpdateStatus::RolledBack => "rolled back",
                UpdateStatus::Failed => "update failed",
            },
            None => "no update record",
        };
        format!("Host build {build}: {status}")
    }

    fn save_update_state(&self, state: &UpdateState) {
        if let Err(error) = state.save(&self.update_state_path()) {
            tracing::warn!(?error, "writing update-state.json failed");
        }
    }

    /// Swap the installed host with `previous\bubbleplay-host.exe`; the
    /// replaced binary is kept as `previous\bubbleplay-host.failed.exe`.
    fn restore_previous_host(&mut self) -> Result<()> {
        let (host, previous, failed) = self.host_paths().context("host paths")?;
        if !previous.is_file() {
            anyhow::bail!("{} is missing", previous.display());
        }
        self.stop_user();
        let _ = std::fs::remove_file(&failed);
        std::fs::rename(&host, &failed)
            .with_context(|| format!("moving {} aside", host.display()))?;
        if let Err(error) = std::fs::copy(&previous, &host) {
            let _ = std::fs::rename(&failed, &host);
            return Err(error).context("restoring previous host");
        }
        self.host_hash = sha256_file(&host);
        Ok(())
    }

    fn roll_back(&mut self, reason: &str, automatic: bool) {
        let now = unix_ms_now();
        let path = self.update_state_path();
        let state = UpdateState::load(&path).unwrap_or_else(|| UpdateState {
            status: UpdateStatus::Pending,
            new_hash: self.host_hash.clone().unwrap_or_default(),
            previous_hash: None,
            updated_unix_ms: now,
            reason: None,
        });
        self.probation = None;
        if automatic {
            if let RollbackPlan::GiveUp(why) =
                plan_rollback(&state, self.previous_hash().as_deref())
            {
                let message = format!("{reason}; {why}");
                tracing::error!(%message, "host update failed and cannot be rolled back");
                if state.status == UpdateStatus::Pending {
                    self.save_update_state(&state.with_status(
                        UpdateStatus::Failed,
                        Some(message.clone()),
                        now,
                    ));
                }
                self.host_error = Some(format!("Host update failed: {message}"));
                self.balloon(
                    "BubblePlay",
                    &format!("Host update failed: {message}"),
                    NIIF_WARNING,
                );
                return;
            }
        }
        tracing::warn!(%reason, automatic, "rolling back host");
        match self.restore_previous_host() {
            Ok(()) => {
                let reason = if automatic {
                    reason.to_string()
                } else {
                    format!("manual: {reason}")
                };
                self.save_update_state(&state.with_status(
                    UpdateStatus::RolledBack,
                    Some(reason.clone()),
                    now,
                ));
                self.host_error = None;
                self.exits.clear();
                self.balloon(
                    "BubblePlay",
                    &format!("Host update rolled back: {reason}"),
                    NIIF_WARNING,
                );
            }
            Err(error) => {
                let message = format!("{reason}; rollback failed: {error:#}");
                tracing::error!(%message, "host rollback failed");
                self.save_update_state(&state.with_status(
                    UpdateStatus::Failed,
                    Some(message.clone()),
                    now,
                ));
                self.host_error = Some(message.clone());
                self.balloon(
                    "BubblePlay",
                    &format!("Host rollback failed: {message}"),
                    NIIF_WARNING,
                );
            }
        }
        self.start_user();
    }

    /// Feed host events into probation and act on its verdict.
    fn watch_update(&mut self) {
        let now = unix_ms_now();
        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                HostEvent::Started => {}
                HostEvent::Registered => {
                    if let Some(probation) = self.probation.as_mut() {
                        if !probation.registered() {
                            tracing::info!("pending host registered with signaling");
                        }
                        probation.on_registered(now);
                    }
                    self.host_error = None;
                }
                HostEvent::ViewerStarted => {
                    tracing::info!("viewer session started");
                    self.viewers.on_started();
                    self.start_secure_helper("viewer session started");
                }
                HostEvent::ViewerEnded => {
                    self.viewers.on_ended(Instant::now());
                    if self.viewers.viewer_connected() {
                        tracing::info!("viewer session ended; another is still active");
                    } else {
                        tracing::info!(
                            grace_s = logic::AGENT_IDLE_GRACE.as_secs(),
                            "viewer session ended; admin agent stops after the grace"
                        );
                    }
                }
                HostEvent::Exited => {
                    self.viewers.on_host_exited(Instant::now());
                    if let Some(probation) = self.probation.as_mut() {
                        probation.on_exit(now);
                    }
                    self.exits.push(now);
                }
            }
        }
        self.exits
            .retain(|&at| now.saturating_sub(at) < update::CRASH_LOOP_WINDOW_MS);
        if self.user.is_none() {
            return;
        }
        if let Some(probation) = self.probation.as_mut() {
            match probation.evaluate(now) {
                Verdict::Wait => {}
                Verdict::Good => {
                    self.probation = None;
                    if let Some(state) = UpdateState::load(&self.update_state_path()) {
                        self.save_update_state(&state.with_status(UpdateStatus::Good, None, now));
                    }
                    tracing::info!("host update marked good");
                }
                Verdict::RollBack(reason) => self.roll_back(&reason, true),
            }
        } else if self.exits.len() >= update::CRASH_LOOP_EXITS && self.host_error.is_none() {
            let rolled_back = UpdateState::load(&self.update_state_path())
                .is_some_and(|state| state.status == UpdateStatus::RolledBack);
            let message = if rolled_back {
                "Rolled-back host keeps exiting; retrying with backoff (see host.log)"
            } else {
                "Host keeps exiting; retrying with backoff (see host.log)"
            };
            tracing::error!(message);
            self.host_error = Some(message.to_string());
        }
    }

    fn tick(&mut self) {
        self.refresh_admin_mode();
        self.watch_update();
        self.gate_agent();
        self.check_agent(false);
        self.check_secure_helper(false);
        self.refresh_icon();
        let state = self.state();
        let tip = status_line(&state, &self.agent);
        if self.shown.as_ref() != Some(&tip) {
            let mut data = self.notify_data();
            data.uFlags = NIF_ICON | NIF_TIP;
            data.hIcon = self.icon.handle;
            copy_wide(&mut data.szTip, &tip);
            // SAFETY: data is fully initialized.
            if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
                self.shown = Some(tip);
            }
        }
    }

    fn restart_host(&mut self) {
        self.stop_user();
        self.start_user();
    }

    fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;
        tracing::info!("tray shutting down");
        self.stop_user();
        self.stop_agent();
        self.remove_icon();
    }

    fn handle_command(&mut self, command: usize) {
        match command {
            CMD_ADMIN => {
                self.change_admin_mode();
            }
            CMD_AGENT_UPDATE | CMD_SECURE_HELPER_UPDATE => self.update_agent(),
            CMD_LOGIN => {
                self.settings.start_at_login = !self.settings.start_at_login;
                self.save_settings();
                self.sync_run_key();
            }
            CMD_RESTART => self.restart_host(),
            CMD_ROLLBACK => self.roll_back("requested from tray menu", false),
            CMD_LOGS => open_folder(&logs_dir(&self.data)),
            CMD_QUIT => {
                self.shutdown();
                // SAFETY: our own window.
                let _ = unsafe { DestroyWindow(self.hwnd) };
                return;
            }
            _ => {}
        }
        self.tick();
    }
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => guard.as_mut().map(f),
        Err(_) => None,
    })
}

fn show_menu(hwnd: HWND) {
    let Some((
        status,
        admin,
        admin_enabled,
        admin_line,
        login,
        agent_update,
        helper_line,
        helper_update,
        build,
        error,
        can_roll_back,
    )) = with_app(|app| {
        app.refresh_admin_mode();
        app.check_agent(true);
        app.check_secure_helper(true);
        let agent_update = if app.agent_update_running {
            Some((MF_STRING | MF_GRAYED, "Admin agent update running..."))
        } else if app.admin_on && app.agent.needs_update() {
            Some((MF_STRING, AGENT_UPDATE_LABEL))
        } else {
            None
        };
        (
            status_line(&app.state(), &app.agent),
            app.admin_on,
            admin_menu_enabled(app.admin_on, app.viewers.viewer_connected())
                && !app.admin_change_running,
            admin_mode_line(&app.agent),
            app.settings.start_at_login,
            agent_update,
            app.secure_helper.line(),
            if app.agent_update_running {
                Some((MF_STRING | MF_GRAYED, "Privileged helper update running..."))
            } else if app.secure_helper.needs_update() {
                Some((MF_STRING, SECURE_HELPER_UPDATE_LABEL))
            } else {
                None
            },
            app.build_line(),
            app.host_error.clone(),
            app.can_roll_back(),
        )
    })
    else {
        return;
    };
    // SAFETY: standard popup-menu sequence on our own window.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let item = |flags, id, text: &str| {
            let text = wide(text);
            let _ = AppendMenuW(menu, flags, id, PCWSTR(text.as_ptr()));
        };
        item(MF_STRING | MF_GRAYED, CMD_STATUS, &status);
        item(MF_STRING | MF_GRAYED, CMD_BUILD, &build);
        if let Some(error) = error {
            item(MF_STRING | MF_GRAYED, CMD_BUILD, &error);
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        item(
            MF_STRING
                | if admin {
                    MF_CHECKED
                } else {
                    windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_FLAGS(0)
                }
                | if admin_enabled {
                    windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_FLAGS(0)
                } else {
                    MF_GRAYED
                },
            CMD_ADMIN,
            "Admin mode",
        );
        if let Some(line) = admin_line {
            item(MF_STRING | MF_GRAYED, CMD_STATUS, line);
        }
        if let Some((flags, label)) = agent_update {
            item(flags, CMD_AGENT_UPDATE, label);
        }
        item(MF_STRING | MF_GRAYED, CMD_STATUS, &helper_line);
        if let Some((flags, label)) = helper_update {
            item(flags, CMD_SECURE_HELPER_UPDATE, label);
        }
        item(
            if login {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            },
            CMD_LOGIN,
            "Start at login",
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        item(MF_STRING, CMD_RESTART, "Restart host");
        if can_roll_back {
            item(MF_STRING, CMD_ROLLBACK, "Roll back host");
        }
        item(MF_STRING, CMD_LOGS, "Open logs folder");
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        item(MF_STRING, CMD_QUIT, "Quit");
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        let _ = SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        if command.0 > 0 {
            with_app(|app| app.handle_command(command.0 as usize));
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TRAY => {
            let event = (lparam.0 & 0xffff) as u32;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            with_app(|app| app.tick());
            LRESULT(0)
        }
        WM_ADMIN_CHANGED => {
            with_app(|app| {
                app.admin_change_running = false;
                app.refresh_admin_mode();
                app.apply_mode();
                app.tick();
            });
            LRESULT(0)
        }
        WM_AGENT_UPDATED => {
            with_app(|app| app.agent_updated(wparam.0 as u32));
            LRESULT(0)
        }
        WM_QUIT_REQUEST => {
            with_app(|app| app.handle_command(CMD_QUIT));
            LRESULT(0)
        }
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                with_app(|app| app.shutdown());
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: posts WM_QUIT to this thread's queue.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => {
            let taskbar_created = with_app(|app| app.taskbar_created).unwrap_or(0);
            if taskbar_created != 0 && message == taskbar_created {
                with_app(|app| app.add_icon());
                return LRESULT(0);
            }
            // SAFETY: default handling.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

fn run_tray() -> Result<i32> {
    // SAFETY: opt this tray process into system DPI before creating any windows,
    // so small-icon metrics select the matching embedded frame (16/20/24/32 px).
    let _ = unsafe { SetProcessDPIAware() };
    let icon = TrayIcon::load(TrayIcon::small_size())?;
    let data = data_dir();
    let test_mode = std::env::var_os("BUBBLEPLAY_TRAY_DATA_DIR").is_some();
    let logs = logs_dir(&data);
    init_tracing(SharedLog::new(logs.join("tray.log")));
    let Some(_instance) = acquire_single_instance(&scoped(TRAY_MUTEX))? else {
        return Ok(0);
    };
    tracing::info!(pid = std::process::id(), data = %data.display(), "tray starting");
    // Signaled to ask the admin agent to exit; the agent also exits when
    // this tray's mutex disappears.
    let stop_event = create_manual_event(&scoped(AGENT_STOP_EVENT))?;
    let _ = std::fs::remove_file(data.join(STALE_HEARTBEAT_FILE));

    // SAFETY: standard window-class registration and hidden window creation.
    let hwnd = unsafe {
        let instance = GetModuleHandleW(PCWSTR::null()).context("GetModuleHandleW")?;
        let class = wide(scoped(WINDOW_CLASS));
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            anyhow::bail!("RegisterClassW failed");
        }
        let title = wide("BubblePlay");
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .context("CreateWindowExW")?
    };
    let taskbar_message = wide("TaskbarCreated");
    // SAFETY: NUL-terminated string.
    let taskbar_created = unsafe { RegisterWindowMessageW(PCWSTR(taskbar_message.as_ptr())) };

    let settings_path = data.join("tray.json");
    let settings = TraySettings::load(&settings_path);
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let app = App {
        hwnd,
        data: data.clone(),
        settings_path,
        settings,
        admin_on: bubbleplay_host::admin_mode::effective_for_console(),
        admin_change_running: false,
        test_mode,
        host_log: SharedLog::new(logs.join("host.log")),
        user: None,
        agent: AgentStatus::Off,
        secure_helper: if test_mode || secure_helper_task_exists() {
            SecureHelperStatus::Armed
        } else {
            SecureHelperStatus::NeedsUpdate {
                reason: "the secure helper is not installed".into(),
            }
        },
        helper_started: None,
        helper_last_probe: None,
        viewers: ViewerGate::default(),
        agent_started: None,
        agent_retry: StartRetry::default(),
        last_probe: None,
        agent_child: None,
        agent_update_running: false,
        announce_ready: false,
        agent_pipe: agent_pipe(),
        stop_event,
        icon,
        shown: None,
        taskbar_created,
        shut_down: false,
        events_tx,
        events_rx,
        probation: None,
        exits: Vec::new(),
        host_hash: None,
        host_error: None,
    };
    APP.with(|cell| *cell.borrow_mut() = Some(app));
    with_app(|app| {
        app.save_settings();
        app.sync_run_key();
        app.add_icon();
        app.apply_mode();
        app.tick();
    });
    // SAFETY: timer on our own window.
    unsafe { SetTimer(Some(hwnd), TIMER_ID, 1000, None) };

    let mut message = MSG::default();
    // SAFETY: standard message loop.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    let app = APP.with(|cell| cell.borrow_mut().take());
    if let Some(mut app) = app {
        app.shutdown();
    }
    tracing::info!("tray exited");
    Ok(0)
}
