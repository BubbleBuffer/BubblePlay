//! Shared Windows Service Control Manager runtime for BubblePlay's privileged roles.
//!
//! The broker and transport binaries include this module directly because the existing host is a
//! binary-only crate. Keep Windows API work at this boundary so the lifecycle and restart policy
//! remain testable without registering a service.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tracing::{info, warn};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL, WAIT_TIMEOUT};
use windows::Win32::Security::Cryptography::{
    CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId,
    TOKEN_ALL_ACCESS,
};
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, ResumeThread, TerminateProcess,
    WaitForSingleObject, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
    STARTUPINFOW,
};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};

#[path = "vdd.rs"]
mod vdd;

pub const START_WAIT_HINT: Duration = Duration::from_secs(10);
const STOP_WAIT_HINT: Duration = Duration::from_secs(5);
const SUPERVISOR_TICK: Duration = Duration::from_millis(500);
const NO_ACTIVE_SESSION: u32 = u32::MAX;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
const MAX_SUPERVISED_LOG_BYTES: u64 = 8 * 1024 * 1024;
const LIVE_RETAINED_LOG_BYTES: u64 = 7 * 1024 * 1024;
const MAX_CONSECUTIVE_START_FAILURES: usize = 5;

fn agent_creation_flags() -> PROCESS_CREATION_FLAGS {
    CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED | CREATE_NO_WINDOW
}

#[allow(dead_code)] // Each binary constructs one role while sharing this directly included module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceRole {
    Broker,
    Transport,
}

impl ServiceRole {
    fn accepts_session_changes(self) -> bool {
        matches!(self, Self::Broker)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServicePhase {
    StartPending,
    Running,
    StopPending,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeEvent {
    Started,
    StopRequested,
    WorkerStopped,
    SessionChanged(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeDecision {
    pub phase: ServicePhase,
    pub reconcile_session: Option<u32>,
}

#[derive(Debug)]
pub struct ServiceLifecycle {
    phase: ServicePhase,
}

impl ServiceLifecycle {
    pub fn new() -> Self {
        Self {
            phase: ServicePhase::StartPending,
        }
    }

    pub fn apply(&mut self, event: RuntimeEvent) -> RuntimeDecision {
        let reconcile_session = match (self.phase, event) {
            (ServicePhase::StartPending, RuntimeEvent::Started) => {
                self.phase = ServicePhase::Running;
                None
            }
            (ServicePhase::Running, RuntimeEvent::StopRequested) => {
                self.phase = ServicePhase::StopPending;
                None
            }
            (ServicePhase::StopPending, RuntimeEvent::WorkerStopped) => {
                self.phase = ServicePhase::Stopped;
                None
            }
            (ServicePhase::Running, RuntimeEvent::SessionChanged(session_id)) => Some(session_id),
            _ => None,
        };
        RuntimeDecision {
            phase: self.phase,
            reconcile_session,
        }
    }
}

#[derive(Debug)]
enum ControlSignal {
    Stop,
    SessionChanged(u32),
}

/// Run a real SCM service. The generated service dispatcher remains in each binary.
pub fn run_service(
    service_name: &'static str,
    role: ServiceRole,
    _args: Vec<OsString>,
) -> Result<()> {
    // SCM services have no console. Keep the backing file alive for the whole
    // dispatch lifetime so tracing/stdout/stderr are durable rather than lost.
    let _service_log = redirect_service_stdio(role)?;
    let (control_tx, control_rx) = mpsc::channel();
    let event_handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
                let _ = control_tx.send(ControlSignal::Stop);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::SessionChange(change) if role.accepts_session_changes() => {
                let _ = control_tx.send(ControlSignal::SessionChanged(
                    change.notification.session_id,
                ));
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(service_name, event_handler)
        .context("registering Windows service control handler")?;
    report_status(
        &status_handle,
        ServicePhase::StartPending,
        role,
        START_WAIT_HINT,
        1,
    )?;
    match run_loop(role, control_rx, Some(&status_handle), true) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = report_failed_start(&status_handle);
            Err(error)
        }
    }
}

/// Safe development entry point. Worker launch is opt-in so inspecting service state cannot
/// accidentally start a second streaming host.
pub fn run_console(role: ServiceRole, run_worker: bool) -> Result<()> {
    let (control_tx, control_rx) = mpsc::channel();
    spawn_console_stop_reader(control_tx);
    info!(
        ?role,
        run_worker, "BubblePlay service console mode; press Enter to stop"
    );
    run_loop(role, control_rx, None, run_worker)
}

fn spawn_console_stop_reader(control_tx: Sender<ControlSignal>) {
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
        let _ = control_tx.send(ControlSignal::Stop);
    });
}

fn run_loop(
    role: ServiceRole,
    control_rx: Receiver<ControlSignal>,
    status_handle: Option<&ServiceStatusHandle>,
    run_worker: bool,
) -> Result<()> {
    let mut lifecycle = ServiceLifecycle::new();
    let mut supervisor = run_worker.then(|| Supervisor::new(role)).transpose()?;

    let running = lifecycle.apply(RuntimeEvent::Started);
    if let Some(handle) = status_handle {
        report_status(handle, running.phase, role, Duration::ZERO, 0)?;
    }

    let mut desired_session = active_console_session_id();
    if let Some(supervisor) = supervisor.as_mut() {
        let result = supervisor.tick(desired_session);
        stop_on_failure(result, || supervisor.stop(), &bubbleplay_data_dir())?;
    }

    loop {
        match control_rx.recv_timeout(SUPERVISOR_TICK) {
            Ok(ControlSignal::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(ControlSignal::SessionChanged(session_id)) => {
                let decision = lifecycle.apply(RuntimeEvent::SessionChanged(session_id));
                if decision.reconcile_session.is_some() {
                    desired_session = active_console_session_id().or(Some(session_id));
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if role.accepts_session_changes() {
                    desired_session = active_console_session_id();
                }
            }
        }
        if let Some(supervisor) = supervisor.as_mut() {
            let result = supervisor.tick(desired_session);
            stop_on_failure(result, || supervisor.stop(), &bubbleplay_data_dir())?;
        }
    }

    let stopping = lifecycle.apply(RuntimeEvent::StopRequested);
    if let Some(handle) = status_handle {
        report_status(handle, stopping.phase, role, STOP_WAIT_HINT, 1)?;
    }
    if let Some(supervisor) = supervisor.as_mut() {
        supervisor.stop();
    }
    let stopped = lifecycle.apply(RuntimeEvent::WorkerStopped);
    if let Some(handle) = status_handle {
        report_status(handle, stopped.phase, role, Duration::ZERO, 0)?;
    }
    Ok(())
}

fn report_failed_start(status_handle: &ServiceStatusHandle) -> Result<()> {
    status_handle
        .set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::ServiceSpecific(1),
            checkpoint: 0,
            wait_hint: Duration::ZERO,
            process_id: None,
        })
        .context("reporting failed Windows service startup")
}

fn report_status(
    status_handle: &ServiceStatusHandle,
    phase: ServicePhase,
    role: ServiceRole,
    wait_hint: Duration,
    checkpoint: u32,
) -> Result<()> {
    let current_state = match phase {
        ServicePhase::StartPending => ServiceState::StartPending,
        ServicePhase::Running => ServiceState::Running,
        ServicePhase::StopPending => ServiceState::StopPending,
        ServicePhase::Stopped => ServiceState::Stopped,
    };
    let controls_accepted = if phase == ServicePhase::Running {
        let mut controls = ServiceControlAccept::STOP | ServiceControlAccept::PRESHUTDOWN;
        if role.accepts_session_changes() {
            controls |= ServiceControlAccept::SESSION_CHANGE;
        }
        controls
    } else {
        ServiceControlAccept::empty()
    };
    status_handle
        .set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint,
            wait_hint,
            process_id: None,
        })
        .context("reporting Windows service status")
}

#[derive(Debug)]
enum Supervisor {
    Broker(BrokerSupervisor),
    Transport(TransportSupervisor),
}

impl Supervisor {
    fn new(role: ServiceRole) -> Result<Self> {
        recover_stale_child_logs_in(&bubbleplay_data_dir(), role)?;
        let install_dir = current_install_dir()?;
        Ok(match role {
            ServiceRole::Broker => Self::Broker(BrokerSupervisor::new(
                install_dir.join("bubbleplay_agent.exe"),
            )),
            ServiceRole::Transport => Self::Transport(TransportSupervisor::new(
                install_dir.join("bubbleplay-host.exe"),
            )),
        })
    }

    fn tick(&mut self, desired_session: Option<u32>) -> Result<()> {
        let role = match self {
            Self::Broker(_) => ServiceRole::Broker,
            Self::Transport(_) => ServiceRole::Transport,
        };
        enforce_supervised_log_budget_in(&bubbleplay_data_dir(), role)?;
        match self {
            Self::Broker(supervisor) => supervisor.tick(desired_session),
            Self::Transport(supervisor) => supervisor.tick(),
        }
    }

    fn stop(&mut self) {
        match self {
            Self::Broker(supervisor) => supervisor.stop(),
            Self::Transport(supervisor) => supervisor.stop(),
        }
    }
}

fn current_install_dir() -> Result<PathBuf> {
    std::env::current_exe()
        .context("resolving service executable path")?
        .parent()
        .map(Path::to_path_buf)
        .context("service executable has no parent directory")
}

fn active_console_session_id() -> Option<u32> {
    // SAFETY: WTSGetActiveConsoleSessionId has no parameters or ownership requirements.
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    (session_id != NO_ACTIVE_SESSION).then_some(session_id)
}

#[derive(Debug)]
struct RestartPolicy {
    backoff_step: usize,
    consecutive_start_failures: usize,
    next_attempt: Instant,
}

impl RestartPolicy {
    fn new(now: Instant) -> Self {
        Self {
            backoff_step: 0,
            consecutive_start_failures: 0,
            next_attempt: now,
        }
    }

    fn ready(&self, now: Instant) -> bool {
        now >= self.next_attempt
    }

    fn failed(&mut self, now: Instant) -> Duration {
        const DELAYS: [Duration; 5] = [
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(5),
            Duration::from_secs(10),
            Duration::from_secs(30),
        ];
        let delay = DELAYS[self.backoff_step.min(DELAYS.len() - 1)];
        self.backoff_step = self.backoff_step.saturating_add(1);
        self.next_attempt = now + delay;
        delay
    }

    fn startup_failed(&mut self, now: Instant) -> Duration {
        let delay = self.failed(now);
        self.consecutive_start_failures = self.consecutive_start_failures.saturating_add(1);
        delay
    }

    fn runtime_exited(&mut self, now: Instant) -> Duration {
        self.consecutive_start_failures = 0;
        self.failed(now)
    }

    fn healthy(&mut self, now: Instant) {
        self.backoff_step = 0;
        self.consecutive_start_failures = 0;
        self.next_attempt = now;
    }

    fn circuit_open(&self) -> bool {
        self.consecutive_start_failures >= MAX_CONSECUTIVE_START_FAILURES
    }
}

fn record_restart_failure(
    restart: &mut RestartPolicy,
    now: Instant,
    component: &'static str,
) -> Result<Duration> {
    let delay = restart.startup_failed(now);
    if restart.circuit_open() {
        bail!(
            "{component} failed {} consecutive times; supervised restart circuit is open",
            restart.consecutive_start_failures
        );
    }
    Ok(delay)
}

fn record_runtime_exit(restart: &mut RestartPolicy, now: Instant) -> Duration {
    restart.runtime_exited(now)
}

fn stop_on_failure(result: Result<()>, stop: impl FnOnce(), data_dir: &Path) -> Result<()> {
    let Err(error) = result else {
        return Ok(());
    };
    let log_budget_role = error
        .downcast_ref::<SupervisedLogBudgetViolation>()
        .map(|violation| violation.role);

    // Close the supervised writer before changing its active file. Service stdio may remain open,
    // so the in-place truncator deliberately preserves append-handle behavior.
    stop();
    if let Some(role) = log_budget_role {
        if let Err(retention_error) = retain_logs_after_live_budget_violation(data_dir, role) {
            let context = format!(
                "{error}; failed to retain bounded logs after the live budget violation: {retention_error:#}"
            );
            return Err(error.context(context));
        }
    }
    Err(error)
}

#[derive(Debug)]
struct BrokerSupervisor {
    agent_path: PathBuf,
    agent: Option<AgentProcess>,
    agent_started: Option<Instant>,
    desired_session: Option<u32>,
    restart: RestartPolicy,
    // The Broker already lives for the lifetime of BubblePlay. Owning the VDD
    // here keeps Windows' display topology stable across Stream Lab campaigns.
    persistent_vdd: Option<vdd::VddKeepalive>,
}

impl BrokerSupervisor {
    fn new(agent_path: PathBuf) -> Self {
        let persistent_vdd = match vdd::VddKeepalive::start() {
            Ok(keepalive) => {
                info!("BubblePlay Broker is holding the persistent virtual display");
                Some(keepalive)
            }
            Err(error) => {
                warn!(?error, "persistent virtual display is unavailable");
                None
            }
        };
        Self {
            agent_path,
            agent: None,
            agent_started: None,
            desired_session: None,
            restart: RestartPolicy::new(Instant::now()),
            persistent_vdd,
        }
    }

    fn tick(&mut self, desired_session: Option<u32>) -> Result<()> {
        let now = Instant::now();
        let Some(session_id) = desired_session else {
            self.stop_agent();
            if self.desired_session.take().is_some() {
                self.restart.healthy(now);
            }
            return Ok(());
        };

        if self.desired_session != Some(session_id) {
            self.stop_agent();
            self.desired_session = Some(session_id);
            self.restart.healthy(now);
        }
        let had_agent = self.agent.is_some();
        let keep_current = self
            .agent
            .as_ref()
            .is_some_and(|agent| agent.session_id == session_id && agent.is_running());
        if keep_current {
            if self
                .agent_started
                .is_some_and(|started| now.duration_since(started) >= Duration::from_secs(30))
            {
                self.restart.healthy(now);
            }
            return Ok(());
        }
        self.stop_agent();
        if had_agent {
            record_runtime_exit(&mut self.restart, now);
        }
        if !self.restart.ready(now) {
            return Ok(());
        }
        if validate_protected_path(&self.agent_path).is_err() || !self.agent_path.is_file() {
            let delay = record_restart_failure(&mut self.restart, now, "BubblePlay Agent startup")?;
            warn!(
                path = %self.agent_path.display(),
                ?delay,
                "BubblePlay Agent is not installed yet"
            );
            return Ok(());
        }

        match AgentProcess::launch(&self.agent_path, session_id) {
            Ok(agent) => {
                info!(
                    session_id,
                    pid = agent.process_id,
                    "launched BubblePlay Agent"
                );
                self.agent = Some(agent);
                self.agent_started = Some(now);
            }
            Err(error) => {
                let delay =
                    record_restart_failure(&mut self.restart, now, "BubblePlay Agent startup")?;
                warn!(
                    ?error,
                    session_id,
                    ?delay,
                    "failed to launch BubblePlay Agent"
                );
            }
        }
        Ok(())
    }

    fn stop_agent(&mut self) {
        if let Some(agent) = self.agent.take() {
            agent.stop();
        }
        self.agent_started = None;
    }

    fn stop(&mut self) {
        self.stop_agent();
        self.persistent_vdd.take();
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredTransportConfig {
    worker: String,
    token_dpapi_hex: String,
    host_gate_token_dpapi_hex: Option<String>,
}

struct TransportConfig {
    worker: OsString,
    token: OsString,
    host_gate_token: Option<OsString>,
}

impl TransportConfig {
    fn load() -> Result<Self> {
        let path = transport_config_path();
        validate_protected_path(&path)?;
        let stored: StoredTransportConfig = serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        )
        .with_context(|| format!("parsing {}", path.display()))?;
        let token = String::from_utf8(dpapi_unprotect_hex(&stored.token_dpapi_hex)?)
            .context("stored BubblePlay token is not UTF-8")?;
        let host_gate_token = stored
            .host_gate_token_dpapi_hex
            .as_deref()
            .map(dpapi_unprotect_hex)
            .transpose()?
            .map(String::from_utf8)
            .transpose()
            .context("stored BubblePlay host-gate token is not UTF-8")?;
        Ok(Self {
            worker: OsString::from(stored.worker),
            token: OsString::from(token),
            host_gate_token: host_gate_token.map(OsString::from),
        })
    }

    fn host_args(&self) -> Vec<OsString> {
        vec![
            OsString::from("--mode"),
            OsString::from("serve"),
            OsString::from("--privileged-agent"),
            OsString::from("--worker"),
            self.worker.clone(),
        ]
    }
}

fn configure_transport_host_command(command: &mut Command, config: &TransportConfig) {
    command
        .args(config.host_args())
        .env("BUBBLEPLAY_TOKEN", &config.token)
        .env("RUST_BACKTRACE", "1");
    if let Some(token) = &config.host_gate_token {
        command.env("BUBBLEPLAY_HOST_GATE_TOKEN", token);
    }
}

#[derive(Debug)]
struct TransportSupervisor {
    host_path: PathBuf,
    child: Option<JobChild>,
    child_started: Option<Instant>,
    restart: RestartPolicy,
}

impl TransportSupervisor {
    fn new(host_path: PathBuf) -> Self {
        Self {
            host_path,
            child: None,
            child_started: None,
            restart: RestartPolicy::new(Instant::now()),
        }
    }

    fn tick(&mut self) -> Result<()> {
        let now = Instant::now();
        if let Some(child) = self.child.as_mut() {
            match child.child.try_wait() {
                Ok(None) => {
                    if self.child_started.is_some_and(|started| {
                        now.saturating_duration_since(started) >= Duration::from_secs(30)
                    }) {
                        self.restart.healthy(now);
                    }
                    return Ok(());
                }
                Ok(Some(status)) => warn!(?status, "BubblePlay transport host exited"),
                Err(error) => warn!(?error, "failed to query BubblePlay transport host"),
            }
            self.child = None;
            self.child_started = None;
            record_runtime_exit(&mut self.restart, now);
        }
        if !self.restart.ready(now) {
            return Ok(());
        }
        match self.start_host() {
            Ok(child) => {
                info!(
                    pid = child.child.id(),
                    "started BubblePlay LocalService transport host"
                );
                self.child = Some(child);
                self.child_started = Some(now);
            }
            Err(error) => {
                let delay = record_restart_failure(
                    &mut self.restart,
                    now,
                    "BubblePlay transport host startup",
                )?;
                warn!(?error, ?delay, "failed to start BubblePlay transport host");
            }
        }
        Ok(())
    }

    fn start_host(&self) -> Result<JobChild> {
        validate_protected_path(&self.host_path)?;
        if !self.host_path.is_file() {
            bail!("transport host is missing: {}", self.host_path.display());
        }
        let config = TransportConfig::load()?;
        let stdout = open_supervised_log("transport-host", "stdout")?;
        let stderr = open_supervised_log("transport-host", "stderr")?;
        let mut command = Command::new(&self.host_path);
        configure_transport_host_command(&mut command, &config);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let child = command
            .spawn()
            .context("starting BubblePlay transport host")?;
        JobChild::new(child).context("placing BubblePlay transport host in kill-on-close job")
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Dropping the job handle terminates the entire supervised process
            // tree even if the direct child created helpers.
            drop(child.job);
            let _ = child.child.wait();
        }
        self.child_started = None;
    }
}

fn transport_config_path() -> PathBuf {
    std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("BubblePlay")
        .join("transport.json")
}

fn bubbleplay_data_dir() -> PathBuf {
    std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("BubblePlay")
}

fn open_supervised_log(component: &str, stream: &str) -> Result<File> {
    let path = supervised_log_path_in(&bubbleplay_data_dir(), component, stream);
    let directory = path.parent().context("supervised log has no parent")?;
    validate_protected_path(&directory)?;
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating protected log directory {}", directory.display()))?;
    validate_protected_path(&directory)?;
    validate_protected_path(&path)?;
    let previous = directory.join(format!("{component}.{stream}.previous.log"));
    validate_protected_path(&previous)?;
    rotate_supervised_log(&path, &previous, MAX_SUPERVISED_LOG_BYTES)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening supervised log {}", path.display()))
}

fn supervised_log_budget_exceeded(length: u64) -> bool {
    length > MAX_SUPERVISED_LOG_BYTES
}

#[derive(Debug)]
struct SupervisedLogBudgetViolation {
    role: ServiceRole,
    path: PathBuf,
    observed_bytes: u64,
}

impl fmt::Display for SupervisedLogBudgetViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "supervised log {} exceeded the {} byte live budget ({} bytes)",
            self.path.display(),
            MAX_SUPERVISED_LOG_BYTES,
            self.observed_bytes
        )
    }
}

impl std::error::Error for SupervisedLogBudgetViolation {}

fn truncate_log_to_tail_in_place(path: &Path, max_bytes: u64) -> Result<()> {
    let length = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting log {}", path.display()))
        }
    };
    if length <= max_bytes {
        return Ok(());
    }

    let mut temp_name = path.as_os_str().to_os_string();
    temp_name.push(format!(".{}.tail.tmp", std::process::id()));
    let temp = PathBuf::from(temp_name);
    if temp.exists() {
        fs::remove_file(&temp)
            .with_context(|| format!("removing stale bounded tail {}", temp.display()))?;
    }

    let result = (|| -> Result<()> {
        let mut source = File::open(path)
            .with_context(|| format!("opening oversized log {}", path.display()))?;
        source
            .seek(SeekFrom::Start(length.saturating_sub(max_bytes)))
            .with_context(|| format!("seeking oversized log tail {}", path.display()))?;
        let mut tail = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .with_context(|| format!("creating bounded log tail {}", temp.display()))?;
        io::copy(&mut (&mut source).take(max_bytes), &mut tail)
            .with_context(|| format!("copying bounded log tail {}", path.display()))?;
        tail.sync_all()
            .with_context(|| format!("flushing bounded log tail {}", temp.display()))?;
        drop(tail);
        drop(source);

        let mut tail = File::open(&temp)
            .with_context(|| format!("reopening bounded log tail {}", temp.display()))?;
        let mut destination = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("truncating oversized log {}", path.display()))?;
        io::copy(&mut tail, &mut destination)
            .with_context(|| format!("restoring bounded log tail {}", path.display()))?;
        destination
            .sync_all()
            .with_context(|| format!("flushing bounded active log {}", path.display()))?;
        drop(destination);
        drop(tail);
        fs::remove_file(&temp)
            .with_context(|| format!("removing bounded log tail {}", temp.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn rotate_supervised_log(path: &Path, previous: &Path, max_bytes: u64) -> Result<()> {
    if fs::metadata(previous).is_ok_and(|metadata| metadata.len() > max_bytes) {
        fs::remove_file(previous)
            .with_context(|| format!("removing oversized rotated log {}", previous.display()))?;
    }
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() >= max_bytes) {
        let mut temp_name = previous.as_os_str().to_os_string();
        temp_name.push(format!(".{}.tmp", std::process::id()));
        let temp = PathBuf::from(temp_name);
        if temp.exists() {
            fs::remove_file(&temp)
                .with_context(|| format!("removing stale rotated log {}", temp.display()))?;
        }

        let result = (|| -> Result<()> {
            let mut source = File::open(path)
                .with_context(|| format!("opening oversized log {}", path.display()))?;
            let length = source
                .metadata()
                .with_context(|| format!("inspecting oversized log {}", path.display()))?
                .len();
            source
                .seek(SeekFrom::Start(length.saturating_sub(max_bytes)))
                .with_context(|| format!("seeking oversized log tail {}", path.display()))?;
            let mut destination = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp)
                .with_context(|| format!("creating bounded rotated log {}", temp.display()))?;
            io::copy(&mut source.take(max_bytes), &mut destination)
                .with_context(|| format!("copying bounded log tail {}", path.display()))?;
            destination
                .sync_all()
                .with_context(|| format!("flushing bounded rotated log {}", temp.display()))?;
            atomic_replace_file(&temp, previous)
                .with_context(|| format!("publishing rotated log {}", previous.display()))?;
            fs::remove_file(path)
                .with_context(|| format!("removing oversized active log {}", path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
    }
    Ok(())
}

fn atomic_replace_file(source: &Path, destination: &Path) -> Result<()> {
    let source = wide_null(source.as_os_str());
    let destination = wide_null(destination.as_os_str());
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .context("atomically replacing file")
}

fn supervised_log_name(component: &str, stream: &str) -> String {
    format!("{component}.{stream}.log")
}

fn supervised_log_specs(role: ServiceRole) -> &'static [(&'static str, &'static str)] {
    match role {
        ServiceRole::Broker => &[
            ("broker-service", "combined"),
            ("agent", "stdout"),
            ("agent", "stderr"),
        ],
        ServiceRole::Transport => &[
            ("transport-service", "combined"),
            ("transport-host", "stdout"),
            ("transport-host", "stderr"),
        ],
    }
}

fn supervised_log_path_in(data_dir: &Path, component: &str, stream: &str) -> PathBuf {
    let trust_partition = match component {
        "transport-host" | "transport-service" => "transport",
        _ => "system",
    };
    data_dir
        .join("logs")
        .join(trust_partition)
        .join(supervised_log_name(component, stream))
}

fn recover_stale_child_logs_in(data_dir: &Path, role: ServiceRole) -> Result<()> {
    for &(component, stream) in &supervised_log_specs(role)[1..] {
        let path = supervised_log_path_in(data_dir, component, stream);
        let directory = path.parent().context("supervised log has no parent")?;
        let previous = directory.join(format!("{component}.{stream}.previous.log"));
        validate_protected_path(&path)?;
        validate_protected_path(&previous)?;
        rotate_supervised_log(&path, &previous, MAX_SUPERVISED_LOG_BYTES)?;
    }
    Ok(())
}

fn retain_logs_after_live_budget_violation(data_dir: &Path, role: ServiceRole) -> Result<()> {
    for &(component, stream) in supervised_log_specs(role) {
        let path = supervised_log_path_in(data_dir, component, stream);
        let directory = path.parent().context("supervised log has no parent")?;
        let previous = directory.join(format!("{component}.{stream}.previous.log"));
        validate_protected_path(&path)?;
        validate_protected_path(&previous)?;
        truncate_log_to_tail_in_place(&path, LIVE_RETAINED_LOG_BYTES)?;
        truncate_log_to_tail_in_place(&previous, MAX_SUPERVISED_LOG_BYTES)?;
    }
    Ok(())
}

fn enforce_supervised_log_budget_in(data_dir: &Path, role: ServiceRole) -> Result<()> {
    for &(component, stream) in supervised_log_specs(role) {
        let path = supervised_log_path_in(data_dir, component, stream);
        validate_protected_path(&path)?;
        match fs::metadata(&path) {
            Ok(metadata) if supervised_log_budget_exceeded(metadata.len()) => {
                return Err(SupervisedLogBudgetViolation {
                    role,
                    path,
                    observed_bytes: metadata.len(),
                }
                .into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting supervised log {}", path.display()));
            }
        }
    }
    Ok(())
}

fn redirect_service_stdio(role: ServiceRole) -> Result<File> {
    let component = match role {
        ServiceRole::Broker => "broker-service",
        ServiceRole::Transport => "transport-service",
    };
    let file = open_supervised_log(component, "combined")?;
    let handle = HANDLE(file.as_raw_handle().cast());
    if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, handle) } == 0 {
        return Err(io::Error::last_os_error()).context("redirecting service stdout");
    }
    if unsafe { SetStdHandle(STD_ERROR_HANDLE, handle) } == 0 {
        return Err(io::Error::last_os_error()).context("redirecting service stderr");
    }
    Ok(file)
}

fn validate_protected_path(path: &Path) -> Result<()> {
    let mut current = Some(path);
    while let Some(component) = current {
        match fs::symlink_metadata(component) {
            Ok(metadata) => {
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    bail!("refusing reparse-point path: {}", component.display());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting protected path {}", component.display()));
            }
        }
        current = component.parent();
    }
    Ok(())
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    if value.len() % 2 != 0 {
        bail!("DPAPI ciphertext has an odd hexadecimal length");
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("hexadecimal pair is valid UTF-8");
            u8::from_str_radix(pair, 16).context("DPAPI ciphertext contains non-hex data")
        })
        .collect()
}

fn dpapi_unprotect_hex(value: &str) -> Result<Vec<u8>> {
    let mut encrypted = decode_hex(value)?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: encrypted
            .len()
            .try_into()
            .context("DPAPI ciphertext is too large")?,
        pbData: encrypted.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .context("decrypting BubblePlay configuration with machine DPAPI")?;
    let plaintext = if output.pbData.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec()
    };
    if !output.pbData.is_null() {
        let _ = unsafe { LocalFree(Some(HLOCAL(output.pbData.cast()))) };
    }
    encrypted.fill(0);
    Ok(plaintext)
}

#[derive(Debug)]
struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> Result<Self> {
        if handle.is_invalid() {
            bail!("Windows returned an invalid handle");
        }
        Ok(Self(handle))
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: OwnedHandle is the unique owner and closes a valid Win32 handle exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[derive(Debug)]
struct KillOnCloseJob {
    handle: OwnedHandle,
}

impl KillOnCloseJob {
    fn new() -> Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        let handle = OwnedHandle::new(handle).context("creating child supervision Job Object")?;
        let mut limits = JobObjectExtendedLimitInformation::default();
        limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle.0,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                (&limits as *const JobObjectExtendedLimitInformation).cast(),
                std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error()).context("configuring kill-on-close Job Object");
        }
        Ok(Self { handle })
    }

    fn assign(&self, process: HANDLE) -> Result<()> {
        if unsafe { AssignProcessToJobObject(self.handle.0, process) } == 0 {
            return Err(io::Error::last_os_error())
                .context("assigning supervised process to Job Object");
        }
        Ok(())
    }
}

#[derive(Debug)]
struct JobChild {
    // Field order is intentional: drop the job before waiting on the child in
    // explicit stop paths, while normal struct drop also closes both handles.
    job: KillOnCloseJob,
    child: Child,
}

impl JobChild {
    fn new(mut child: Child) -> Result<Self> {
        let job = KillOnCloseJob::new()?;
        let process = HANDLE(child.as_raw_handle().cast());
        if let Err(error) = job.assign(process) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(Self { job, child })
    }
}

#[derive(Debug)]
struct AgentProcess {
    job: KillOnCloseJob,
    process: OwnedHandle,
    process_id: u32,
    session_id: u32,
}

impl AgentProcess {
    fn launch(agent_path: &Path, session_id: u32) -> Result<Self> {
        validate_protected_path(agent_path)?;
        let agent_wide = wide_null(agent_path.as_os_str());
        let current_dir = agent_path
            .parent()
            .map(|path| wide_null(path.as_os_str()))
            .context("Agent executable has no parent directory")?;
        let mut desktop = wide_null(OsStr::new("winsta0\\default"));

        let mut source_token = HANDLE::default();
        // SAFETY: Output pointers reference initialized storage and returned handles are owned below.
        unsafe {
            OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut source_token)
                .context("opening Broker process token")?;
        }
        let source_token = OwnedHandle::new(source_token)?;

        let mut session_token = HANDLE::default();
        // SAFETY: source_token remains valid, output points to initialized storage, and the new token
        // is closed by OwnedHandle.
        unsafe {
            DuplicateTokenEx(
                source_token.0,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut session_token,
            )
            .context("duplicating Broker primary token")?;
        }
        let session_token = OwnedHandle::new(session_token)?;

        // SAFETY: TokenSessionId expects a pointer to a u32 session identifier.
        unsafe {
            SetTokenInformation(
                session_token.0,
                TokenSessionId,
                (&session_id as *const u32).cast(),
                std::mem::size_of::<u32>() as u32,
            )
            .context("assigning Agent token to interactive session")?;
        }

        let mut environment = std::ptr::null_mut();
        // SAFETY: The token is valid and environment receives ownership released below.
        unsafe {
            CreateEnvironmentBlock(&mut environment, Some(session_token.0), false)
                .context("creating Agent environment block")?;
        }
        let environment = EnvironmentBlock(environment);

        let stdin = File::open("NUL").context("opening Agent null stdin")?;
        let stdout = open_supervised_log("agent", "stdout")?;
        let stderr = open_supervised_log("agent", "stderr")?;
        for handle in [
            stdin.as_raw_handle(),
            stdout.as_raw_handle(),
            stderr.as_raw_handle(),
        ] {
            if unsafe {
                SetHandleInformation(
                    HANDLE(handle.cast()),
                    HANDLE_FLAG_INHERIT,
                    HANDLE_FLAG_INHERIT,
                )
            } == 0
            {
                return Err(io::Error::last_os_error())
                    .context("marking Agent standard stream inheritable");
            }
        }

        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: HANDLE(stdin.as_raw_handle().cast()),
            hStdOutput: HANDLE(stdout.as_raw_handle().cast()),
            hStdError: HANDLE(stderr.as_raw_handle().cast()),
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        // SAFETY: All UTF-16 buffers are NUL-terminated and live through the call; process receives
        // handles which are closed or transferred before returning.
        unsafe {
            CreateProcessAsUserW(
                Some(session_token.0),
                PCWSTR(agent_wide.as_ptr()),
                None,
                None,
                None,
                true,
                agent_creation_flags(),
                Some(environment.0.cast_const()),
                PCWSTR(current_dir.as_ptr()),
                &startup,
                &mut process,
            )
            .context("creating BubblePlay Agent in active console session")?;
        }
        let process_handle = OwnedHandle::new(process.hProcess)?;
        let thread_handle = OwnedHandle::new(process.hThread)?;
        let terminate_suspended = || {
            let _ = unsafe { TerminateProcess(process_handle.0, 1) };
            let _ = unsafe { WaitForSingleObject(process_handle.0, 2_000) };
        };
        let job = match KillOnCloseJob::new() {
            Ok(job) => job,
            Err(error) => {
                terminate_suspended();
                return Err(error).context("creating Agent supervision Job Object");
            }
        };
        if let Err(error) = job.assign(process_handle.0) {
            terminate_suspended();
            return Err(error).context("assigning suspended Agent to supervision Job Object");
        }
        if unsafe { ResumeThread(thread_handle.0) } == u32::MAX {
            let error = io::Error::last_os_error();
            drop(job);
            let _ = unsafe { WaitForSingleObject(process_handle.0, 2_000) };
            return Err(error).context("resuming supervised Agent process");
        }
        drop(thread_handle);
        Ok(Self {
            job,
            process: process_handle,
            process_id: process.dwProcessId,
            session_id,
        })
    }

    fn is_running(&self) -> bool {
        // SAFETY: process is a valid process handle and a zero timeout does not block.
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_TIMEOUT }
    }

    fn stop(self) {
        if self.is_running() {
            // Closing the job first kills the complete Agent process tree.
            drop(self.job);
            // SAFETY: process remains valid until self is dropped after this method.
            let _ = unsafe { WaitForSingleObject(self.process.0, 2_000) };
        }
    }
}

struct EnvironmentBlock(*mut core::ffi::c_void);

impl Drop for EnvironmentBlock {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: CreateEnvironmentBlock returned this pointer and this is its sole owner.
            let _ = unsafe { DestroyEnvironmentBlock(self.0) };
        }
    }
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[repr(C)]
#[derive(Default)]
struct JobObjectBasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[repr(C)]
#[derive(Default)]
struct JobObjectExtendedLimitInformation {
    basic_limit_information: JobObjectBasicLimitInformation,
    io_info: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: i32 = 9;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
const STD_ERROR_HANDLE: u32 = (-12i32) as u32;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *const core::ffi::c_void, name: *const u16) -> HANDLE;
    fn SetInformationJobObject(
        job: HANDLE,
        information_class: i32,
        information: *const core::ffi::c_void,
        information_length: u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: HANDLE, process: HANDLE) -> i32;
    fn SetHandleInformation(handle: HANDLE, mask: u32, flags: u32) -> i32;
    fn SetStdHandle(std_handle: u32, handle: HANDLE) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_reaches_running_then_stops_idempotently() {
        let mut lifecycle = ServiceLifecycle::new();

        assert_eq!(
            lifecycle.apply(RuntimeEvent::Started).phase,
            ServicePhase::Running
        );
        assert_eq!(
            lifecycle.apply(RuntimeEvent::StopRequested).phase,
            ServicePhase::StopPending
        );
        assert_eq!(
            lifecycle.apply(RuntimeEvent::StopRequested).phase,
            ServicePhase::StopPending
        );
        assert_eq!(
            lifecycle.apply(RuntimeEvent::WorkerStopped).phase,
            ServicePhase::Stopped
        );
    }

    #[test]
    fn only_running_services_reconcile_session_changes() {
        let mut lifecycle = ServiceLifecycle::new();
        assert_eq!(
            lifecycle
                .apply(RuntimeEvent::SessionChanged(7))
                .reconcile_session,
            None
        );

        lifecycle.apply(RuntimeEvent::Started);
        assert_eq!(
            lifecycle
                .apply(RuntimeEvent::SessionChanged(7))
                .reconcile_session,
            Some(7)
        );

        lifecycle.apply(RuntimeEvent::StopRequested);
        assert_eq!(
            lifecycle
                .apply(RuntimeEvent::SessionChanged(8))
                .reconcile_session,
            None
        );
    }

    #[test]
    fn restart_backoff_is_bounded_and_resets_after_health() {
        let start = Instant::now();
        let mut policy = RestartPolicy::new(start);
        let observed: Vec<_> = (0..7).map(|_| policy.failed(start)).collect();
        assert_eq!(
            observed,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(10),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
            ]
        );
        policy.healthy(start);
        assert_eq!(policy.failed(start), Duration::from_secs(1));
    }

    #[test]
    fn restart_circuit_opens_after_five_consecutive_start_failures_and_resets() {
        let start = Instant::now();
        let mut policy = RestartPolicy::new(start);
        for _ in 0..MAX_CONSECUTIVE_START_FAILURES {
            policy.startup_failed(start);
        }
        assert!(policy.circuit_open());
        policy.healthy(start);
        assert!(!policy.circuit_open());
    }

    #[test]
    fn fifth_supervised_start_failure_is_fatal() {
        let start = Instant::now();
        let mut policy = RestartPolicy::new(start);
        for _ in 1..MAX_CONSECUTIVE_START_FAILURES {
            record_restart_failure(&mut policy, start, "test child").unwrap();
        }
        let error = record_restart_failure(&mut policy, start, "test child").unwrap_err();
        assert!(error.to_string().contains("test child"));
        assert!(error
            .to_string()
            .contains(&MAX_CONSECUTIVE_START_FAILURES.to_string()));
    }

    #[test]
    fn repeated_runtime_exits_back_off_without_stopping_the_service() {
        let start = Instant::now();
        let mut policy = RestartPolicy::new(start);
        let observed: Vec<_> = (0..12)
            .map(|_| record_runtime_exit(&mut policy, start))
            .collect();
        assert_eq!(observed[0], Duration::from_secs(1));
        assert_eq!(observed[4], Duration::from_secs(30));
        assert_eq!(observed[11], Duration::from_secs(30));
    }

    #[test]
    fn fatal_supervisor_tick_runs_cleanup_once() {
        let mut stops = 0;
        let result = stop_on_failure(
            Err(anyhow::anyhow!("fatal supervisor tick")),
            || {
                stops += 1;
            },
            Path::new("unused"),
        );
        assert!(result.is_err());
        assert_eq!(stops, 1);
    }

    #[test]
    fn live_supervised_log_budget_rejects_the_first_byte_past_the_cap() {
        assert!(!supervised_log_budget_exceeded(MAX_SUPERVISED_LOG_BYTES));
        assert!(supervised_log_budget_exceeded(MAX_SUPERVISED_LOG_BYTES + 1));
    }

    #[test]
    fn in_place_tail_truncation_preserves_append_handle_and_bounded_tail() {
        use std::io::Write as _;

        struct TempDirectory(PathBuf);
        impl Drop for TempDirectory {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = TempDirectory(std::env::temp_dir().join(format!(
            "bubbleplay-in-place-log-truncation-{}-{unique}",
            std::process::id()
        )));
        fs::create_dir_all(&directory.0).unwrap();
        let active = directory.0.join("active.log");
        let bytes: Vec<u8> = (0..96).collect();
        fs::write(&active, &bytes).unwrap();
        let mut append_handle = OpenOptions::new().append(true).open(&active).unwrap();

        truncate_log_to_tail_in_place(&active, 24).unwrap();
        append_handle.write_all(&[0xfa, 0xfb]).unwrap();
        append_handle.flush().unwrap();

        let retained = fs::read(&active).unwrap();
        assert_eq!(&retained[..24], &bytes[72..]);
        assert_eq!(&retained[24..], &[0xfa, 0xfb]);
    }

    #[test]
    fn oversized_rotation_retains_only_the_bounded_log_tail() {
        struct TempDirectory(PathBuf);
        impl Drop for TempDirectory {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = TempDirectory(std::env::temp_dir().join(format!(
            "bubbleplay-log-rotation-{}-{unique}",
            std::process::id()
        )));
        fs::create_dir_all(&directory.0).unwrap();
        let active = directory.0.join("active.log");
        let previous = directory.0.join("previous.log");
        let bytes: Vec<u8> = (0..96).collect();
        fs::write(&active, &bytes).unwrap();

        rotate_supervised_log(&active, &previous, 32).unwrap();

        assert!(!active.exists());
        assert_eq!(fs::read(&previous).unwrap(), bytes[64..]);
        assert_eq!(fs::metadata(&previous).unwrap().len(), 32);
    }

    #[test]
    fn active_and_previous_logs_retain_at_most_two_bounded_files() {
        struct TempDirectory(PathBuf);
        impl Drop for TempDirectory {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = TempDirectory(std::env::temp_dir().join(format!(
            "bubbleplay-log-retention-{}-{unique}",
            std::process::id()
        )));
        fs::create_dir_all(&directory.0).unwrap();
        let active = directory.0.join("active.log");
        let previous = directory.0.join("previous.log");
        fs::write(&previous, vec![0x11; 96]).unwrap();
        fs::write(&active, vec![0x22; 96]).unwrap();

        rotate_supervised_log(&active, &previous, 32).unwrap();
        fs::write(&active, vec![0x33; 32]).unwrap();

        let retained =
            fs::metadata(&active).unwrap().len() + fs::metadata(&previous).unwrap().len();
        assert_eq!(retained, 64);
    }

    #[test]
    fn every_role_log_is_rejected_when_it_exceeds_the_live_budget() {
        struct TempDirectory(PathBuf);
        impl Drop for TempDirectory {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir = TempDirectory(std::env::temp_dir().join(format!(
            "bubbleplay-live-log-budget-{}-{unique}",
            std::process::id()
        )));
        for role in [ServiceRole::Broker, ServiceRole::Transport] {
            for &(component, stream) in supervised_log_specs(role) {
                let trust_partition = match component {
                    "transport-host" | "transport-service" => "transport",
                    _ => "system",
                };
                let directory = data_dir.0.join("logs").join(trust_partition);
                fs::create_dir_all(&directory).unwrap();
                let path = directory.join(supervised_log_name(component, stream));
                File::create(&path)
                    .unwrap()
                    .set_len(MAX_SUPERVISED_LOG_BYTES + 1)
                    .unwrap();

                let error = enforce_supervised_log_budget_in(&data_dir.0, role).unwrap_err();
                assert!(error.to_string().contains(&path.display().to_string()));
                File::create(path).unwrap().set_len(0).unwrap();
            }
        }
    }

    #[test]
    fn live_budget_violation_stops_before_bounding_active_and_previous_logs() {
        struct TempDirectory(PathBuf);
        impl Drop for TempDirectory {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir = TempDirectory(std::env::temp_dir().join(format!(
            "bubbleplay-live-log-retention-{}-{unique}",
            std::process::id()
        )));
        let directory = data_dir.0.join("logs").join("transport");
        fs::create_dir_all(&directory).unwrap();
        let active = directory.join(supervised_log_name("transport-host", "stderr"));
        let previous = directory.join("transport-host.stderr.previous.log");
        File::create(&active)
            .unwrap()
            .set_len(MAX_SUPERVISED_LOG_BYTES + 4096)
            .unwrap();
        File::create(&previous)
            .unwrap()
            .set_len(MAX_SUPERVISED_LOG_BYTES + 4096)
            .unwrap();

        let result = enforce_supervised_log_budget_in(&data_dir.0, ServiceRole::Transport);
        let mut stop_saw_oversized_log = false;
        let error = stop_on_failure(
            result,
            || {
                stop_saw_oversized_log =
                    fs::metadata(&active).unwrap().len() > MAX_SUPERVISED_LOG_BYTES;
            },
            &data_dir.0,
        )
        .unwrap_err();

        assert!(stop_saw_oversized_log);
        assert!(error.to_string().contains(&active.display().to_string()));
        assert!(fs::metadata(&active).unwrap().len() <= LIVE_RETAINED_LOG_BYTES);
        assert!(fs::metadata(&previous).unwrap().len() <= MAX_SUPERVISED_LOG_BYTES);
        let retained =
            fs::metadata(&active).unwrap().len() + fs::metadata(&previous).unwrap().len();
        assert!(retained <= 2 * MAX_SUPERVISED_LOG_BYTES);
    }

    #[test]
    fn stale_oversized_child_log_can_be_bounded_before_live_monitoring() {
        struct TempDirectory(PathBuf);
        impl Drop for TempDirectory {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir = TempDirectory(std::env::temp_dir().join(format!(
            "bubbleplay-stale-log-recovery-{}-{unique}",
            std::process::id()
        )));
        let directory = data_dir.0.join("logs").join("transport");
        fs::create_dir_all(&directory).unwrap();
        let active = directory.join(supervised_log_name("transport-host", "stderr"));
        let previous = directory.join("transport-host.stderr.previous.log");
        File::create(&active)
            .unwrap()
            .set_len(MAX_SUPERVISED_LOG_BYTES + 1)
            .unwrap();

        assert!(enforce_supervised_log_budget_in(&data_dir.0, ServiceRole::Transport).is_err());
        recover_stale_child_logs_in(&data_dir.0, ServiceRole::Transport).unwrap();
        enforce_supervised_log_budget_in(&data_dir.0, ServiceRole::Transport).unwrap();
        assert!(!active.exists());
        assert_eq!(
            fs::metadata(previous).unwrap().len(),
            MAX_SUPERVISED_LOG_BYTES
        );
    }

    #[test]
    fn transport_always_starts_the_host_in_privileged_agent_mode() {
        let config = TransportConfig {
            worker: OsString::from("wss://example.invalid/ws"),
            token: OsString::from("opaque"),
            host_gate_token: None,
        };
        let args = config.host_args();
        assert!(args.iter().any(|arg| arg == "--privileged-agent"));
        assert!(args.iter().any(|arg| arg == "--mode"));
        assert!(args.iter().any(|arg| arg == "serve"));
        assert!(!args.iter().any(|arg| arg == "opaque"));
        assert!(!args.iter().any(|arg| arg == "--token"));
    }

    #[test]
    fn supervised_transport_host_enables_one_fatal_backtrace() {
        let config = TransportConfig {
            worker: OsString::from("wss://example.invalid/ws"),
            token: OsString::from("opaque"),
            host_gate_token: None,
        };
        let mut command = Command::new("ignored.exe");
        configure_transport_host_command(&mut command, &config);
        let backtrace = command
            .get_envs()
            .find(|(name, _)| *name == "RUST_BACKTRACE")
            .and_then(|(_, value)| value);
        assert_eq!(backtrace, Some(OsStr::new("1")));
    }

    #[test]
    fn supervised_children_use_a_kill_on_close_job() {
        let job = KillOnCloseJob::new().unwrap();
        assert!(!job.handle.0.is_invalid());
    }

    #[test]
    fn interactive_agent_launch_never_allocates_a_console_window() {
        use windows::Win32::System::Threading::CREATE_NO_WINDOW;

        let flags = agent_creation_flags();
        assert_eq!(flags & CREATE_NO_WINDOW, CREATE_NO_WINDOW);
        assert_eq!(flags & CREATE_SUSPENDED, CREATE_SUSPENDED);
        assert_eq!(
            flags & CREATE_UNICODE_ENVIRONMENT,
            CREATE_UNICODE_ENVIRONMENT
        );
    }

    #[test]
    fn supervised_log_names_are_stable_and_contain_no_secret_material() {
        assert_eq!(
            supervised_log_name("transport-host", "stderr"),
            "transport-host.stderr.log"
        );
        assert_eq!(supervised_log_name("agent", "stdout"), "agent.stdout.log");
        assert_eq!(
            supervised_log_name("broker-service", "combined"),
            "broker-service.combined.log"
        );
    }
}
