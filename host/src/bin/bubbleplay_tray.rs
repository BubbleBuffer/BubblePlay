//! BubblePlay notification-area app. Replaces the Windows services: it runs
//! `bubbleplay-host.exe --mode serve` as the signed-in user (local DXGI
//! capture + local SendInput), restarts it with backoff, and can hand off to
//! the on-demand `\BubblePlay\ElevatedHost` scheduled task ("Admin mode") so
//! input reaches elevated windows without a UAC prompt each time.
//!
//! Modes:
//!   bubbleplay_tray.exe                      tray (single instance)
//!   bubbleplay_tray.exe --run-elevated-host  runner used by the scheduled task
//!   bubbleplay_tray.exe --quit               ask a running tray to exit
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
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, HGDIOBJ};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, NOTIFY_ICON_INFOTIP_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW,
    PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    SetTimer, TrackPopupMenu, TranslateMessage, HICON, ICONINFO, MF_CHECKED, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, MSG, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE,
    WM_APP, WM_DESTROY, WM_ENDSESSION, WM_LBUTTONUP, WM_NULL, WM_QUERYENDSESSION, WM_RBUTTONUP,
    WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};

use logic::{
    admin_menu_label, is_outdated, run_key_command, status_line, unix_ms_now, write_atomic,
    Heartbeat, HostState, SharedLog, TraySettings,
};
use platform::{
    acquire_single_instance, create_manual_event, current_build_id, data_dir, elevated_task_exists,
    end_elevated_task, host_exe_beside, is_process_elevated, is_signaled, is_under_program_files,
    named_mutex_exists, open_event, open_folder, read_run_value, run_elevated_task, set_event,
    set_run_value, sha256_file, wide, Handle, ELEVATED_STOP_EVENT, TRAY_MUTEX,
};
use supervisor::{HostEvent, SupervisorConfig, SupervisorThread};
use update::{
    plan_rollback, short_hash, Probation, RollbackPlan, UpdateState, UpdateStatus, Verdict,
};

const WINDOW_CLASS: &str = "BubblePlayTrayWindow";
const WM_TRAY: u32 = WM_APP + 1;
const WM_QUIT_REQUEST: u32 = WM_APP + 2;
const TIMER_ID: usize = 1;

const CMD_STATUS: usize = 1;
const CMD_ADMIN: usize = 2;
const CMD_LOGIN: usize = 3;
const CMD_RESTART: usize = 4;
const CMD_LOGS: usize = 5;
const CMD_QUIT: usize = 6;
const CMD_ROLLBACK: usize = 7;
const CMD_BUILD: usize = 8;

/// Admin-mode grace period before a missing heartbeat means "not running".
const ADMIN_START_GRACE: Duration = Duration::from_secs(20);
/// Minimum spacing between automatic re-runs of the elevated task.
const ADMIN_RERUN_INTERVAL: Duration = Duration::from_secs(30);

const ELEVATED_RUNNER_MUTEX: &str = r"Local\BubblePlayTray.ElevatedRunner";
const HEARTBEAT_FILE: &str = "elevated-runner.json";

fn main() {
    let mut args = std::env::args().skip(1);
    let code = match args.next().as_deref() {
        None => run_tray(),
        Some("--run-elevated-host") => run_elevated_host(),
        Some("--quit") => quit_running_tray(),
        Some(_) => Ok(2),
    };
    std::process::exit(match code {
        Ok(code) => code,
        Err(_) => 1,
    });
}

/// Named-object / window-class name for this instance. With
/// `BUBBLEPLAY_TRAY_DATA_DIR` set (test mode) names are suffixed by the data
/// dir, so a test tray never collides with (or `--quit`s) the real one.
fn scoped(base: &str) -> String {
    match std::env::var_os("BUBBLEPLAY_TRAY_DATA_DIR") {
        Some(dir) => {
            let key = dir.to_string_lossy().to_lowercase();
            format!("{base}.test-{:016x}", logic::fnv1a64(key.as_bytes()))
        }
        None => base.to_string(),
    }
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
) -> Result<SupervisorConfig> {
    let exe = std::env::current_exe().context("current_exe")?;
    Ok(SupervisorConfig {
        host_exe: host_exe_beside(&exe),
        host_config: data.join("host.json"),
        host_log,
        events,
    })
}

// ---------------------------------------------------------------------------
// Elevated runner (launched by the \BubblePlay\ElevatedHost scheduled task)
// ---------------------------------------------------------------------------

fn write_heartbeat(path: &Path, host: HostState, build: &str) {
    let beat = Heartbeat {
        runner_pid: std::process::id(),
        updated_unix_ms: unix_ms_now(),
        host,
        build: build.to_string(),
    };
    if let Ok(bytes) = serde_json::to_vec(&beat) {
        let _ = write_atomic(path, &bytes);
    }
}

fn run_elevated_host() -> Result<i32> {
    let data = data_dir();
    let log = SharedLog::new(logs_dir(&data).join("host-admin.log"));
    init_tracing(log.clone());
    // An elevated runner must only ever execute admin-owned binaries: it runs
    // the host beside its own exe, so that exe must live under Program Files.
    let exe = std::env::current_exe().context("current_exe")?;
    if is_process_elevated() && !is_under_program_files(&exe) {
        tracing::error!(
            exe = %exe.display(),
            "refusing to run elevated from a non-Program Files location"
        );
        return Ok(1);
    }
    let Some(_instance) = acquire_single_instance(&scoped(ELEVATED_RUNNER_MUTEX))? else {
        tracing::info!("elevated runner already active; exiting");
        return Ok(0);
    };
    // The tray creates this event before running the task; without it the
    // tray is gone and an elevated host must not linger.
    let Some(stop_event) = open_event(&scoped(ELEVATED_STOP_EVENT)) else {
        tracing::warn!("tray is not running; elevated runner exiting");
        return Ok(0);
    };
    tracing::info!(pid = std::process::id(), "elevated runner started");
    let heartbeat = data.join(HEARTBEAT_FILE);
    let build = current_build_id();
    let mut supervisor = SupervisorThread::start(supervisor_config(&data, log, None)?);
    let mut tray_missing = 0u32;
    loop {
        write_heartbeat(&heartbeat, supervisor.state(), &build);
        if is_signaled(&stop_event) {
            tracing::info!("tray requested stop");
            break;
        }
        if named_mutex_exists(&scoped(TRAY_MUTEX)) {
            tray_missing = 0;
        } else {
            tray_missing += 1;
            if tray_missing >= 3 {
                tracing::warn!("tray disappeared; stopping elevated host");
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    supervisor.stop();
    write_heartbeat(&heartbeat, HostState::Stopped, &build);
    tracing::info!("elevated runner exited");
    Ok(0)
}

// ---------------------------------------------------------------------------
// --quit
// ---------------------------------------------------------------------------

fn quit_running_tray() -> Result<i32> {
    let class = wide(&scoped(WINDOW_CLASS));
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum IconKind {
    Running,
    Busy,
    Stopped,
}

fn icon_kind(state: &HostState) -> IconKind {
    match state {
        HostState::Running { .. } => IconKind::Running,
        HostState::Starting | HostState::Restarting { .. } => IconKind::Busy,
        HostState::NotConfigured { .. } | HostState::Stopped => IconKind::Stopped,
    }
}

/// Draw a 32x32 status disc with a white "play" glyph.
fn make_icon(kind: IconKind) -> HICON {
    const SIZE: i32 = 32;
    let (r, g, b) = match kind {
        IconKind::Running => (0x2e, 0xa0, 0x5a),
        IconKind::Busy => (0xe0, 0x9a, 0x1f),
        IconKind::Stopped => (0x80, 0x80, 0x88),
    };
    let mut pixels = vec![0u32; (SIZE * SIZE) as usize];
    let center = (SIZE as f32 - 1.0) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let distance = (dx * dx + dy * dy).sqrt();
            let coverage = (15.5 - distance).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            // Play triangle: x in [11, 23], |dy| <= (23 - x) / 2 * ...
            let fx = x as f32;
            let in_triangle = (11.0..=23.0).contains(&fx) && dy.abs() <= (23.0 - fx) * 0.6;
            let (pr, pg, pb) = if in_triangle {
                (255, 255, 255)
            } else {
                (r, g, b)
            };
            let alpha = (coverage * 255.0) as u32;
            // Premultiplied BGRA.
            let premultiply = |c: u32| c * alpha / 255;
            pixels[(y * SIZE + x) as usize] =
                (alpha << 24) | (premultiply(pr) << 16) | (premultiply(pg) << 8) | premultiply(pb);
        }
    }
    let mask = vec![0u8; (SIZE * SIZE / 8) as usize];
    // SAFETY: buffers match the declared bitmap dimensions and live across the calls.
    unsafe {
        let color = CreateBitmap(SIZE, SIZE, 1, 32, Some(pixels.as_ptr().cast()));
        let mask_bitmap = CreateBitmap(SIZE, SIZE, 1, 1, Some(mask.as_ptr().cast()));
        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bitmap,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask_bitmap.0));
        icon
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
    test_mode: bool,
    host_log: SharedLog,
    user: Option<SupervisorThread>,
    admin_active: bool,
    admin_started: Option<Instant>,
    stop_event: Handle,
    icons: [HICON; 3],
    shown: Option<(IconKind, String)>,
    taskbar_created: u32,
    shut_down: bool,
    build: String,
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
    fn icon(&self, kind: IconKind) -> HICON {
        self.icons[kind as usize]
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
        let state = self.state();
        let kind = icon_kind(&state);
        let tip = status_line(&state, self.admin_active);
        let mut data = self.notify_data();
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = self.icon(kind);
        copy_wide(&mut data.szTip, &tip);
        // SAFETY: data is fully initialized.
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool();
        tracing::info!(added, "notification icon registered");
        self.shown = Some((kind, tip));
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

    fn start_admin(&mut self) -> bool {
        if !elevated_task_exists() {
            return false;
        }
        set_event(&self.stop_event, false);
        match run_elevated_task() {
            Ok(()) => {
                tracing::info!("elevated host task started");
                self.admin_active = true;
                self.admin_started = Some(Instant::now());
                true
            }
            Err(error) => {
                tracing::warn!(?error, "running elevated task failed");
                false
            }
        }
    }

    fn stop_admin(&mut self) {
        if !self.admin_active {
            return;
        }
        tracing::info!("stopping elevated host task");
        set_event(&self.stop_event, true);
        // Give the runner a moment to stop its child cleanly, then make sure.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && named_mutex_exists(&scoped(ELEVATED_RUNNER_MUTEX)) {
            std::thread::sleep(Duration::from_millis(100));
        }
        end_elevated_task();
        self.admin_active = false;
        self.admin_started = None;
    }

    fn admin_heartbeat(&self) -> Option<Heartbeat> {
        let bytes = std::fs::read(self.data.join(HEARTBEAT_FILE)).ok()?;
        let beat: Heartbeat = serde_json::from_slice(&bytes).ok()?;
        beat.is_fresh(unix_ms_now()).then_some(beat)
    }

    fn state(&self) -> HostState {
        if self.admin_active {
            if let Some(beat) = self.admin_heartbeat() {
                return beat.host;
            }
            if self
                .admin_started
                .is_some_and(|started| started.elapsed() < ADMIN_START_GRACE)
            {
                return HostState::Starting;
            }
            return HostState::Stopped;
        }
        match &self.user {
            Some(user) => user.state(),
            None => HostState::Stopped,
        }
    }

    /// A live elevated runner built from different binaries than this tray.
    fn admin_outdated(&self) -> bool {
        self.admin_active
            && self
                .admin_heartbeat()
                .is_some_and(|beat| is_outdated(&self.build, &beat.build))
    }

    fn missing_task_fallback(&mut self) {
        self.settings.admin_mode = false;
        self.save_settings();
        self.balloon(
            "BubblePlay",
            "Admin mode needs one-time setup (run setup-tray.ps1 as administrator).",
            NIIF_WARNING,
        );
    }

    fn apply_mode(&mut self) {
        if self.settings.admin_mode {
            self.stop_user();
            if !self.admin_active && !self.start_admin() {
                self.missing_task_fallback();
                self.start_user();
            }
        } else {
            self.stop_admin();
            self.start_user();
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
                HostEvent::Exited => {
                    if let Some(probation) = self.probation.as_mut() {
                        probation.on_exit(now);
                    }
                    self.exits.push(now);
                }
            }
        }
        self.exits
            .retain(|&at| now.saturating_sub(at) < update::CRASH_LOOP_WINDOW_MS);
        if self.admin_active || self.user.is_none() {
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
        self.watch_update();
        // Re-run the elevated task if it died (e.g. ended externally).
        if self.admin_active && self.admin_heartbeat().is_none() {
            let due = self
                .admin_started
                .is_none_or(|started| started.elapsed() >= ADMIN_RERUN_INTERVAL);
            if due && !named_mutex_exists(&scoped(ELEVATED_RUNNER_MUTEX)) {
                tracing::warn!("elevated host is not reporting; re-running task");
                self.admin_active = false;
                if !self.start_admin() {
                    self.missing_task_fallback();
                    self.start_user();
                }
            }
        }
        let state = self.state();
        let kind = icon_kind(&state);
        let tip = status_line(&state, self.admin_active);
        if self.shown.as_ref() != Some(&(kind, tip.clone())) {
            let mut data = self.notify_data();
            data.uFlags = NIF_ICON | NIF_TIP;
            data.hIcon = self.icon(kind);
            copy_wide(&mut data.szTip, &tip);
            // SAFETY: data is fully initialized.
            let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
            self.shown = Some((kind, tip));
        }
    }

    fn restart_host(&mut self) {
        if self.admin_active {
            self.stop_admin();
            if !self.start_admin() {
                self.missing_task_fallback();
                self.start_user();
            }
        } else {
            self.stop_user();
            self.start_user();
        }
    }

    fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;
        tracing::info!("tray shutting down");
        self.stop_user();
        self.stop_admin();
        self.remove_icon();
    }

    fn handle_command(&mut self, command: usize) {
        match command {
            CMD_ADMIN => {
                self.settings.admin_mode = !self.settings.admin_mode;
                self.save_settings();
                self.apply_mode();
                if self.settings.admin_mode && self.admin_active {
                    self.balloon(
                        "BubblePlay",
                        "Admin mode on: input can reach elevated windows. The secure desktop (UAC prompts, lock screen) is still not captured.",
                        NIIF_INFO,
                    );
                }
            }
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
    let Some((status, admin, login, outdated, build, error, can_roll_back)) = with_app(|app| {
        (
            status_line(&app.state(), app.admin_active),
            app.settings.admin_mode,
            app.settings.start_at_login,
            app.admin_outdated(),
            app.build_line(),
            app.host_error.clone(),
            !app.admin_active && app.can_roll_back(),
        )
    }) else {
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
            if admin {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            },
            CMD_ADMIN,
            admin_menu_label(outdated),
        );
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
    let data = data_dir();
    let test_mode = std::env::var_os("BUBBLEPLAY_TRAY_DATA_DIR").is_some();
    let logs = logs_dir(&data);
    init_tracing(SharedLog::new(logs.join("tray.log")));
    let Some(_instance) = acquire_single_instance(&scoped(TRAY_MUTEX))? else {
        return Ok(0);
    };
    tracing::info!(pid = std::process::id(), data = %data.display(), "tray starting");
    let stop_event = create_manual_event(&scoped(ELEVATED_STOP_EVENT))?;

    // SAFETY: standard window-class registration and hidden window creation.
    let hwnd = unsafe {
        let instance = GetModuleHandleW(PCWSTR::null()).context("GetModuleHandleW")?;
        let class = wide(&scoped(WINDOW_CLASS));
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
        test_mode,
        host_log: SharedLog::new(logs.join("host.log")),
        user: None,
        admin_active: false,
        admin_started: None,
        stop_event,
        icons: [
            make_icon(IconKind::Running),
            make_icon(IconKind::Busy),
            make_icon(IconKind::Stopped),
        ],
        shown: None,
        taskbar_created,
        shut_down: false,
        build: current_build_id(),
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
        for icon in app.icons {
            // SAFETY: icons were created by make_icon.
            let _ = unsafe { DestroyIcon(icon) };
        }
    }
    tracing::info!("tray exited");
    Ok(0)
}
