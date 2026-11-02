//! Host child supervision for the tray: load config, decrypt secrets into
//! env vars, spawn
//! `bubbleplay-host.exe` in a kill-on-close job, pipe its output into a
//! rotating log, restart with backoff.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bubbleplay_host::devices_ui_api::OPEN_DEVICES_LINE;
use windows::Win32::Foundation::HANDLE;

use zeroize::Zeroizing;

use super::device_key_file;
use super::logic::{
    build_host_args, log_child_line, Backoff, HostState, StoredHostConfig, TraySettings,
};
use super::platform::{dpapi_unprotect_hex, KillOnCloseJob};

/// Stdin line that asks the host to tell connected pipe clients to show the
/// Devices app. The host side defines the same literal.
pub const SHOW_DEVICES_APP_LINE: &[u8] = b"show-devices-app\n";

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const POLL: Duration = Duration::from_millis(250);
const NOT_CONFIGURED_RECHECK: Duration = Duration::from_secs(30);

pub struct SupervisorConfig {
    pub host_exe: PathBuf,
    pub host_config: PathBuf,
    /// Optional lifecycle events for update probation.
    pub events: Option<Sender<HostEvent>>,
    /// Extra environment for the host (the admin agent's pipe name).
    pub extra_env: Vec<(String, String)>,
}

/// Host lifecycle events observed by the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEvent {
    Started,
    /// The host logged its signaling registration line.
    Registered,
    /// The host logged that a viewer's media transport is up.
    ViewerStarted,
    /// The host logged that the viewer session ended.
    ViewerEnded,
    Exited,
}

/// The event a host output line signals, if any.
pub fn line_event(line: &[u8]) -> Option<HostEvent> {
    use bubbleplay_host::admin_agent::client::{VIEWER_ENDED_MARKER, VIEWER_STARTED_MARKER};
    [
        (super::update::REGISTERED_MARKER, HostEvent::Registered),
        (VIEWER_STARTED_MARKER, HostEvent::ViewerStarted),
        (VIEWER_ENDED_MARKER, HostEvent::ViewerEnded),
    ]
    .into_iter()
    .find(|(marker, _)| contains(line, marker.as_bytes()))
    .map(|(_, event)| event)
}

fn emit(events: &Option<Sender<HostEvent>>, event: HostEvent) {
    if let Some(events) = events {
        let _ = events.send(event);
    }
}

pub type SharedState = Arc<Mutex<HostState>>;

/// Write end of the stdin of the host child that is running now. The
/// supervisor fills it at spawn and empties it before anything else once the
/// child is gone, so a request can only reach the child that is running.
#[derive(Clone, Default)]
pub struct HostStdin(Arc<Mutex<Option<ChildStdin>>>);

impl HostStdin {
    fn set(&self, stdin: Option<ChildStdin>) {
        match self.0.lock() {
            Ok(mut guard) => *guard = stdin,
            Err(poisoned) => *poisoned.into_inner() = stdin,
        }
    }

    /// Writes the fixed `open-devices` line to the current child. Fails with
    /// `NotConnected` when no child is running.
    pub fn request_open_devices(&self) -> io::Result<()> {
        self.write_line(OPEN_DEVICES_LINE)
    }

    /// Writes the fixed `show-devices-app` line to the current child. Fails
    /// with `NotConnected` when no child is running.
    pub fn request_show_devices_app(&self) -> io::Result<()> {
        self.write_line(SHOW_DEVICES_APP_LINE)
    }

    fn write_line(&self, line: &[u8]) -> io::Result<()> {
        let mut guard = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(stdin) = guard.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "host is not running",
            ));
        };
        stdin.write_all(line)?;
        stdin.flush()
    }
}

fn publish(state: &SharedState, value: HostState) {
    match state.lock() {
        Ok(mut guard) => *guard = value,
        Err(poisoned) => *poisoned.into_inner() = value,
    }
}

/// Wait up to `duration`; true means stop was requested (or the sender vanished).
fn wait_for_stop(stop: &Receiver<()>, duration: Duration) -> bool {
    match stop.recv_timeout(duration) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
        Err(RecvTimeoutError::Timeout) => false,
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Read a host pipe line by line: lifecycle markers become events and warn/error output goes
/// into the tray log (the host's own detailed log is `host-YYYY-MM-DD.jsonl`).
fn pump<R: Read + Send + 'static>(
    reader: R,
    stream: &'static str,
    events: Option<Sender<HostEvent>>,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = Vec::with_capacity(512);
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                // Keep draining even if logging fails, so the host never blocks
                // on a full pipe.
                Ok(0) => break,
                Err(error) => {
                    tracing::warn!(%error, stream, "reading host output failed; output no longer captured");
                    break;
                }
                Ok(_) => {
                    log_child_line("host", stream, &line);
                    if let Some(event) = line_event(&line) {
                        emit(&events, event);
                    }
                }
            }
        }
    });
}

struct Secrets {
    token: String,
}

fn load(config: &SupervisorConfig) -> Result<(StoredHostConfig, Secrets)> {
    let stored = StoredHostConfig::load(&config.host_config)?;
    let token = String::from_utf8(dpapi_unprotect_hex(&stored.token_dpapi_hex)?)
        .context("stored token is not UTF-8")?;
    bubbleplay_host::logging::redact::register_secret(&token);
    Ok((stored, Secrets { token }))
}

/// `tray.json` sits beside `host.json` in the per-user data directory.
fn tray_settings_path(host_config: &Path) -> PathBuf {
    host_config.with_file_name("tray.json")
}

/// The PC device key for this host start. An unusable key logs a safe line and returns `None`,
/// so the host starts without a device ID. The key file is never replaced.
fn device_key_handoff(config: &SupervisorConfig) -> Option<Zeroizing<String>> {
    let path = device_key_file::path_beside(&config.host_config);
    match device_key_file::handoff(
        &path,
        &device_key_file::default_label(),
        &device_key_file::now_rfc3339(),
    ) {
        Ok(key) => Some(key),
        Err(_) => {
            // The key error is deliberately not logged: it can come from reading the key file.
            tracing::warn!("PC device key unavailable; starting without a device ID");
            None
        }
    }
}

fn spawn(config: &SupervisorConfig, job: &KillOnCloseJob, stdin: &HostStdin) -> Result<Child> {
    if !config.host_exe.is_file() {
        anyhow::bail!("host executable missing: {}", config.host_exe.display());
    }
    let (stored, secrets) = load(config)?;
    // Read at every spawn, so a changed setting applies on the next host start.
    let settings = TraySettings::load(&tray_settings_path(&config.host_config));
    let device_key = device_key_handoff(config);
    let mut command = Command::new(&config.host_exe);
    command
        .args(build_host_args(&stored, settings.audio, settings.cpu_mode))
        .env("BUBBLEPLAY_TOKEN", &secrets.token)
        .env("RUST_BACKTRACE", "1")
        // Panics keep backtraces; anyhow errors (logged on every retry) do not.
        .env("RUST_LIB_BACKTRACE", "0")
        .env_remove("BUBBLEPLAY_ADMIN_AGENT_PIPE")
        .env_remove("BUBBLEPLAY_ADMIN_AGENT_ALLOW_DEV")
        // Never inherited: only the child command below may carry the device key.
        .env_remove(device_key_file::HANDOFF_ENV)
        .envs(config.extra_env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    if let Some(parent) = config.host_exe.parent() {
        command.current_dir(parent);
    }
    if let Some(key) = &device_key {
        command.env(device_key_file::HANDOFF_ENV, key.as_str());
    }
    let mut child = command.spawn().context("starting bubbleplay-host.exe")?;
    if let Err(error) = job.assign(HANDLE(child.as_raw_handle())) {
        let _ = child.kill();
        let _ = child.wait();
        bubbleplay_host::input::restore_headless_cursor_after_exit(child.id());
        return Err(error);
    }
    stdin.set(child.stdin.take());
    if let Some(out) = child.stdout.take() {
        pump(out, "stdout", config.events.clone());
    }
    if let Some(err) = child.stderr.take() {
        pump(err, "stderr", config.events.clone());
    }
    Ok(child)
}

fn is_not_configured(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    }) && error.to_string().contains("host.json")
}

/// Supervise until `stop` fires. The child is killed before returning.
pub fn supervise(
    config: SupervisorConfig,
    state: SharedState,
    stop: Receiver<()>,
    stdin: HostStdin,
) {
    let job = match KillOnCloseJob::new() {
        Ok(job) => job,
        Err(error) => {
            tracing::error!(?error, "cannot create job object; refusing to start host");
            publish(
                &state,
                HostState::NotConfigured {
                    reason: format!("{error:#}"),
                },
            );
            let _ = stop.recv();
            publish(&state, HostState::Stopped);
            return;
        }
    };
    let mut backoff = Backoff::default();
    publish(&state, HostState::Starting);
    loop {
        let started = Instant::now();
        let mut child = match spawn(&config, &job, &stdin) {
            Ok(child) => child,
            Err(error) => {
                let reason = format!("{error:#}");
                if is_not_configured(&error) {
                    tracing::warn!(%reason, "host not configured; run setup-tray.ps1");
                    publish(&state, HostState::NotConfigured { reason });
                    if wait_for_stop(&stop, NOT_CONFIGURED_RECHECK) {
                        break;
                    }
                    continue;
                }
                let delay = backoff.on_start_failure();
                tracing::warn!(%reason, ?delay, "failed to start host");
                publish(
                    &state,
                    HostState::Restarting {
                        delay_ms: delay.as_millis() as u64,
                        reason,
                    },
                );
                if wait_for_stop(&stop, delay) {
                    break;
                }
                continue;
            }
        };
        let pid = child.id();
        tracing::info!(pid, exe = %config.host_exe.display(), "host started");
        publish(&state, HostState::Running { pid });
        emit(&config.events, HostEvent::Started);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(?error, "querying host failed");
                }
            }
            if wait_for_stop(&stop, POLL) {
                break None;
            }
        };
        // Close the pipe before anything else: a request after this point
        // must not reach this child, and the next spawn fills the slot again.
        stdin.set(None);
        let Some(status) = status else {
            tracing::info!(pid, "stopping host");
            let _ = child.kill();
            let _ = child.wait();
            bubbleplay_host::input::restore_headless_cursor_after_exit(pid);
            break;
        };
        bubbleplay_host::input::restore_headless_cursor_after_exit(pid);
        emit(&config.events, HostEvent::Exited);
        let ran_for = started.elapsed();
        let delay = backoff.on_exit(ran_for);
        tracing::warn!(pid, ?status, ?ran_for, ?delay, "host exited; restarting");
        publish(
            &state,
            HostState::Restarting {
                delay_ms: delay.as_millis() as u64,
                reason: format!("exited with {status}"),
            },
        );
        if wait_for_stop(&stop, delay) {
            break;
        }
    }
    publish(&state, HostState::Stopped);
    drop(job);
}

/// Handle to a supervisor running on a background thread.
pub struct SupervisorThread {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub state: SharedState,
    stdin: HostStdin,
}

impl SupervisorThread {
    pub fn start(config: SupervisorConfig) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let state: SharedState = Arc::new(Mutex::new(HostState::Starting));
        let stdin = HostStdin::default();
        let thread_state = state.clone();
        let thread_stdin = stdin.clone();
        let thread = std::thread::Builder::new()
            .name("host-supervisor".into())
            .spawn(move || supervise(config, thread_state, rx, thread_stdin))
            .ok();
        Self {
            stop: Some(tx),
            thread,
            state,
            stdin,
        }
    }

    pub fn state(&self) -> HostState {
        match self.state.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Asks the running host to open its Devices window. Fails with
    /// `NotConnected` when no host child is running.
    pub fn request_open_devices(&self) -> io::Result<()> {
        self.stdin.request_open_devices()
    }

    /// Asks the running host to tell the Devices app to show itself. Fails
    /// with `NotConnected` when no host child is running.
    pub fn request_show_devices_app(&self) -> io::Result<()> {
        self.stdin.request_show_devices_app()
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for SupervisorThread {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_settings_sit_beside_host_config() {
        let host = Path::new(r"C:\Users\me\AppData\Local\BubblePlay\host.json");
        assert_eq!(
            tray_settings_path(host),
            PathBuf::from(r"C:\Users\me\AppData\Local\BubblePlay\tray.json")
        );
    }

    #[test]
    fn devices_request_without_a_running_host_is_refused() {
        let stdin = HostStdin::default();
        let error = stdin
            .request_open_devices()
            .expect_err("no child is running");
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
        stdin.set(None);
        assert!(stdin.request_open_devices().is_err());
    }

    #[test]
    fn show_request_without_a_running_host_is_refused() {
        let stdin = HostStdin::default();
        let error = stdin
            .request_show_devices_app()
            .expect_err("no child is running");
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
    }

    #[test]
    fn show_line_is_the_fixed_literal() {
        assert_eq!(SHOW_DEVICES_APP_LINE, b"show-devices-app\n");
    }

    #[test]
    fn show_request_writes_exactly_the_line_to_a_child() {
        let mut child = Command::new("cmd")
            .args(["/C", "more"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn more");
        let stdin = HostStdin::default();
        stdin.set(child.stdin.take());
        stdin.request_show_devices_app().expect("written");
        stdin.set(None); // closes the pipe so `more` finishes
        let output = child.wait_with_output().expect("output");
        let text = String::from_utf8_lossy(&output.stdout);
        assert_eq!(text.trim(), "show-devices-app");
    }

    #[test]
    fn host_lines_map_to_events() {
        let line = |text: &str| line_event(text.as_bytes());
        assert_eq!(
            line("2026-09-30T10:00:00Z  INFO bubbleplay_host::signaling: signaling registered as host\n"),
            Some(HostEvent::Registered)
        );
        assert_eq!(
            line("2026-09-30T10:00:01Z  INFO bubbleplay_host: viewer session started\n"),
            Some(HostEvent::ViewerStarted)
        );
        assert_eq!(
            line("2026-09-30T10:00:02Z  INFO bubbleplay_host: viewer session ended\n"),
            Some(HostEvent::ViewerEnded)
        );
        assert_eq!(line("INFO session ended - waiting for next client\n"), None);
        assert_eq!(line(""), None);
    }
}
