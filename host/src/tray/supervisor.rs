//! Host child supervision shared by the tray (user mode) and the elevated
//! runner (admin mode): load config, decrypt secrets into env vars, spawn
//! `bubbleplay-host.exe` in a kill-on-close job, pipe its output into a
//! rotating log, restart with backoff.

use std::io::{BufRead, BufReader, Read};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::Foundation::HANDLE;

use super::logic::{build_host_args, Backoff, HostState, SharedLog, StoredHostConfig};
use super::platform::{dpapi_unprotect_hex, KillOnCloseJob};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const POLL: Duration = Duration::from_millis(250);
const NOT_CONFIGURED_RECHECK: Duration = Duration::from_secs(30);

pub struct SupervisorConfig {
    pub host_exe: PathBuf,
    pub host_config: PathBuf,
    /// Host stdout/stderr destination.
    pub host_log: SharedLog,
}

pub type SharedState = Arc<Mutex<HostState>>;

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

fn pump<R: Read + Send + 'static>(reader: R, log: SharedLog) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = Vec::with_capacity(512);
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                // Keep draining even if logging fails, so the host never blocks
                // on a full pipe.
                Ok(0) | Err(_) => break,
                Ok(_) => log.append(&line),
            }
        }
    });
}

struct Secrets {
    token: String,
    host_gate_token: Option<String>,
}

fn load(config: &SupervisorConfig) -> Result<(StoredHostConfig, Secrets)> {
    let stored = StoredHostConfig::load(&config.host_config)?;
    let token = String::from_utf8(dpapi_unprotect_hex(&stored.token_dpapi_hex)?)
        .context("stored token is not UTF-8")?;
    let host_gate_token = match stored.host_gate_token_dpapi_hex.as_deref() {
        Some(hex) if !hex.trim().is_empty() => Some(
            String::from_utf8(dpapi_unprotect_hex(hex)?)
                .context("stored host-gate token is not UTF-8")?,
        ),
        _ => None,
    };
    Ok((
        stored,
        Secrets {
            token,
            host_gate_token,
        },
    ))
}

fn spawn(config: &SupervisorConfig, job: &KillOnCloseJob) -> Result<Child> {
    if !config.host_exe.is_file() {
        anyhow::bail!("host executable missing: {}", config.host_exe.display());
    }
    let (stored, secrets) = load(config)?;
    let mut command = Command::new(&config.host_exe);
    command
        .args(build_host_args(&stored))
        .env("BUBBLEPLAY_TOKEN", &secrets.token)
        .env("RUST_BACKTRACE", "1")
        // Panics keep backtraces; anyhow errors (logged on every retry) do not.
        .env("RUST_LIB_BACKTRACE", "0")
        .env_remove("BUBBLEPLAY_HOST_GATE_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    if let Some(parent) = config.host_exe.parent() {
        command.current_dir(parent);
    }
    if let Some(gate) = &secrets.host_gate_token {
        command.env("BUBBLEPLAY_HOST_GATE_TOKEN", gate);
    }
    let mut child = command.spawn().context("starting bubbleplay-host.exe")?;
    if let Err(error) = job.assign(HANDLE(child.as_raw_handle())) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    if let Some(out) = child.stdout.take() {
        pump(out, config.host_log.clone());
    }
    if let Some(err) = child.stderr.take() {
        pump(err, config.host_log.clone());
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
pub fn supervise(config: SupervisorConfig, state: SharedState, stop: Receiver<()>) {
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
        let mut child = match spawn(&config, &job) {
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
        let Some(status) = status else {
            tracing::info!(pid, "stopping host");
            let _ = child.kill();
            let _ = child.wait();
            break;
        };
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
}

impl SupervisorThread {
    pub fn start(config: SupervisorConfig) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let state: SharedState = Arc::new(Mutex::new(HostState::Starting));
        let thread_state = state.clone();
        let thread = std::thread::Builder::new()
            .name("host-supervisor".into())
            .spawn(move || supervise(config, thread_state, rx))
            .ok();
        Self {
            stop: Some(tx),
            thread,
            state,
        }
    }

    pub fn state(&self) -> HostState {
        match self.state.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
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
