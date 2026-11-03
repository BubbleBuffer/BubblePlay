//! Host / tray side of the admin input agent pipe.
//!
//! `AdminAgentLink` is used by the host's `InputSink`: every `SendInput`
//! batch goes to the elevated agent when one is connected and speaks a
//! compatible protocol version; otherwise the caller chooses whether local
//! injection is permitted for the foreground target. Connection attempts are
//! lazy (on input) and spaced out, so a host
//! without Admin mode pays one failed `CreateFile` every few seconds at most.

use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_PIPE_BUSY, HANDLE};
use windows::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};

use crate::deadline_pipe::BoundedPipe;

use super::protocol::{
    read_frame, write_client, AgentMessage, ClientMessage, Record, RefuseCode, Role,
    ENV_ALLOW_DEV_AGENT, ENV_PIPE, HEADER_LEN, MAGIC, MAX_PAYLOAD, MAX_RECORDS_PER_FRAME,
    PROTOCOL_MAX, PROTOCOL_MIN,
};
use super::trust;

/// Agent binary name; the host only talks to a server running this image.
pub const AGENT_EXE: &str = "bubbleplay_admin_agent.exe";
/// Host log line once a viewer's media transport is up, used for tray
/// lifecycle logging. Admin startup uses current demand independently of
/// this line and [`VIEWER_ENDED_MARKER`].
pub const VIEWER_STARTED_MARKER: &str = "viewer session started";
/// Host log line when that viewer session ends (peer left, error or exit).
pub const VIEWER_ENDED_MARKER: &str = "viewer session ended";
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Client steps slower than this are logged, so a stall shows which side is slow.
const SLOW_STEP: Duration = Duration::from_millis(500);
const INJECT_TIMEOUT: Duration = Duration::from_millis(500);
/// Error code reported for a batch the agent refused under its rate limit.
pub const ERROR_AGENT_RATE_LIMITED: i32 = 0x2000_0001;
/// Error code reported when the agent stopped answering mid-batch.
pub const ERROR_AGENT_LOST: i32 = 0x2000_0002;

/// Read + write data only: GENERIC_WRITE would include
/// FILE_CREATE_PIPE_INSTANCE, which the agent's pipe DACL denies to users.
pub const PIPE_CLIENT_ACCESS: u32 = 0x0012_0089 | 0x0002;

// CreateFile flags: identification-level impersonation only, so the agent
// can never act as this process's user through the pipe.
const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// No agent listening (Admin mode off, agent not started yet).
    Unavailable(String),
    /// The pipe server is not an agent we trust.
    Untrusted(String),
    /// The agent speaks no version we speak.
    Incompatible { agent_min: u16, agent_max: u16 },
    /// I/O or protocol failure.
    Failed(String),
    /// The agent did not answer the hello before its deadline (slow or stalled).
    TimedOut(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::Unavailable(why) => write!(f, "admin agent unavailable: {why}"),
            LinkError::Untrusted(why) => write!(f, "admin agent not trusted: {why}"),
            LinkError::Incompatible {
                agent_min,
                agent_max,
            } => write!(
                f,
                "admin agent speaks protocol {agent_min}..{agent_max}, host needs {PROTOCOL_MIN}..{PROTOCOL_MAX}"
            ),
            LinkError::Failed(why) => write!(f, "admin agent failed: {why}"),
            LinkError::TimedOut(why) => write!(f, "admin agent not responding: {why}"),
        }
    }
}

/// Delay before the next connection attempt after a failure.
pub fn retry_delay(error: &LinkError) -> Duration {
    match error {
        LinkError::Unavailable(_) | LinkError::Failed(_) | LinkError::TimedOut(_) => {
            Duration::from_secs(2)
        }
        // Neither heals without an agent update; the tray reports these.
        LinkError::Untrusted(_) | LinkError::Incompatible { .. } => Duration::from_secs(60),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handshake {
    pub version: u16,
    pub agent_pid: u32,
    pub elevated: bool,
}

pub struct Connection {
    pipe: BoundedPipe,
    pub handshake: Handshake,
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn open_pipe(pipe: &str) -> Result<BoundedPipe, LinkError> {
    let name = wide(pipe);
    let mut waited = false;
    loop {
        let opened = BoundedPipe::open(
            pipe,
            PIPE_CLIENT_ACCESS,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
        );
        match opened {
            Ok(pipe) => return Ok(pipe),
            // The agent is between accepting one client and listening again.
            Err(error) if !waited && error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                waited = true;
                // SAFETY: NUL-terminated name.
                let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 1_000) };
            }
            Err(error) => return Err(LinkError::Unavailable(error.to_string())),
        }
    }
}

fn handle(file: &File) -> HANDLE {
    HANDLE(file.as_raw_handle() as _)
}

/// The pipe server must be the agent binary, in our session, and (unless
/// dev agents are allowed in test mode) installed under Program Files, so
/// a same-user process squatting the pipe name cannot receive input.
fn verify_server(file: &File, allow_dev: bool) -> Result<(), LinkError> {
    let mut pid = 0u32;
    // SAFETY: valid pipe handle and out-pointer.
    unsafe { GetNamedPipeServerProcessId(handle(file), &mut pid) }
        .map_err(|error| LinkError::Untrusted(format!("server pid: {}", error.message())))?;
    let image = trust::process_image_path(pid)
        .ok_or_else(|| LinkError::Untrusted(format!("cannot query server pid {pid}")))?;
    let name_ok = image
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(AGENT_EXE));
    if !name_ok {
        return Err(LinkError::Untrusted(format!(
            "server is {}, not {AGENT_EXE}",
            image.display()
        )));
    }
    if trust::process_session_id(pid) != Some(trust::current_session_id()) {
        return Err(LinkError::Untrusted(format!(
            "server pid {pid} is in another session"
        )));
    }
    if !allow_dev && !trust::is_under_program_files(&image) {
        return Err(LinkError::Untrusted(format!(
            "server {} is not under Program Files",
            image.display()
        )));
    }
    Ok(())
}

/// Read a whole frame under the same deadline as its corresponding write.
fn read_reply(pipe: &mut BoundedPipe, deadline: Instant) -> io::Result<AgentMessage> {
    let mut header = [0; HEADER_LEN];
    pipe.read_exact_until(&mut header, deadline)?;
    if header[..4] != MAGIC || header[5] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid admin agent frame header",
        ));
    }
    let len = u16::from_le_bytes([header[6], header[7]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "admin agent payload exceeds limit",
        ));
    }
    let mut frame = Vec::from(header);
    frame.resize(HEADER_LEN + len, 0);
    pipe.read_exact_until(&mut frame[HEADER_LEN..], deadline)?;
    let (kind, payload) = read_frame(&mut frame.as_slice())?;
    AgentMessage::decode(kind, &payload)
}

fn send_message(
    pipe: &mut BoundedPipe,
    message: &ClientMessage,
    deadline: Instant,
) -> io::Result<()> {
    let mut frame = Vec::new();
    write_client(&mut frame, message)?;
    pipe.write_all_until(&frame, deadline)
}
impl Connection {
    /// Connect, verify the server and perform the versioned hello. A hello
    /// that misses its deadline is retried once on a fresh connection.
    pub fn open(pipe: &str, allow_dev: bool, role: Role) -> Result<Self, LinkError> {
        retry_once_on_timeout(|| Self::open_once(pipe, allow_dev, role))
    }

    fn open_once(pipe: &str, allow_dev: bool, role: Role) -> Result<Self, LinkError> {
        let started = Instant::now();
        let mut pipe = open_pipe(pipe)?;
        verify_server(
            pipe.file()
                .map_err(|error| LinkError::Failed(error.to_string()))?,
            allow_dev,
        )?;
        log_slow_step("connect and verify_server", started);
        let deadline = Instant::now() + HELLO_TIMEOUT;
        let written = Instant::now();
        send_message(
            &mut pipe,
            &ClientMessage::Hello {
                min: PROTOCOL_MIN,
                max: PROTOCOL_MAX,
                role,
            },
            deadline,
        )
        .map_err(|error| hello_failure("hello", error))?;
        log_slow_step("hello write", written);
        let replied = Instant::now();
        let reply = read_reply(&mut pipe, deadline);
        log_slow_step("hello reply", replied);
        match reply {
            Ok(AgentMessage::Welcome {
                version,
                agent_pid,
                elevated,
                ..
            }) if (PROTOCOL_MIN..=PROTOCOL_MAX).contains(&version) => Ok(Self {
                pipe,
                handshake: Handshake {
                    version,
                    agent_pid,
                    elevated,
                },
            }),
            Ok(AgentMessage::Refused {
                code: RefuseCode::Version,
                agent_min,
                agent_max,
            }) => Err(LinkError::Incompatible {
                agent_min,
                agent_max,
            }),
            Ok(other) => Err(LinkError::Failed(format!(
                "unexpected hello reply {other:?}"
            ))),
            Err(error) => Err(hello_failure("hello reply", error)),
        }
    }

    fn send_frame(&mut self, records: &[Record], deadline: Instant) -> io::Result<()> {
        send_message(
            &mut self.pipe,
            &ClientMessage::Inject(records.to_vec()),
            deadline,
        )
    }

    fn receipt(&mut self, deadline: Instant) -> io::Result<AgentMessage> {
        read_reply(&mut self.pipe, deadline)
    }
}

/// Result of the tray's availability/version check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    Ready(Handshake),
    Incompatible {
        agent_min: u16,
        agent_max: u16,
    },
    /// The agent did not answer in time; it is not known to be out of date.
    NotResponding(String),
    Unavailable(String),
}

pub fn probe(pipe: &str, allow_dev: bool) -> ProbeResult {
    match Connection::open(pipe, allow_dev, Role::Probe) {
        Ok(connection) => ProbeResult::Ready(connection.handshake),
        Err(LinkError::Incompatible {
            agent_min,
            agent_max,
        }) => ProbeResult::Incompatible {
            agent_min,
            agent_max,
        },
        Err(LinkError::TimedOut(why)) => ProbeResult::NotResponding(why),
        Err(error) => ProbeResult::Unavailable(error.to_string()),
    }
}

/// A missed hello deadline means a slow or stalled agent; any other I/O
/// error is a failure.
fn hello_failure(step: &str, error: io::Error) -> LinkError {
    if error.kind() == io::ErrorKind::TimedOut {
        LinkError::TimedOut(format!("{step}: {error}"))
    } else {
        LinkError::Failed(format!("{step}: {error}"))
    }
}

/// Runs the attempt again once if the agent did not answer in time. Every
/// other failure is final, so an untrusted or incompatible agent is not retried.
fn retry_once_on_timeout<T>(
    mut attempt: impl FnMut() -> Result<T, LinkError>,
) -> Result<T, LinkError> {
    match attempt() {
        Err(LinkError::TimedOut(why)) => {
            tracing::info!(%why, "admin agent did not answer in time; retrying once");
            attempt()
        }
        other => other,
    }
}

fn log_slow_step(step: &'static str, started: Instant) {
    let elapsed = started.elapsed();
    if elapsed >= SLOW_STEP {
        tracing::warn!(
            step,
            elapsed_ms = elapsed.as_millis() as u64,
            "admin agent client step slow"
        );
    }
}

/// What the link is doing, for logs and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    Idle,
    Connected(Handshake),
    Fallback(LinkError),
}

pub struct AdminAgentLink {
    pipe: String,
    allow_dev: bool,
    connection: Option<Connection>,
    retry_at: Option<Instant>,
    state: LinkState,
    attempts: u64,
}

impl AdminAgentLink {
    pub fn new(pipe: impl Into<String>, allow_dev: bool) -> Self {
        Self {
            pipe: pipe.into(),
            allow_dev,
            connection: None,
            retry_at: None,
            state: LinkState::Idle,
            attempts: 0,
        }
    }

    /// Configured by the tray through `BUBBLEPLAY_ADMIN_AGENT_PIPE`.
    pub fn from_env() -> Option<Self> {
        let pipe = std::env::var(ENV_PIPE).ok().filter(|p| !p.is_empty())?;
        let allow_dev = std::env::var_os(ENV_ALLOW_DEV_AGENT).is_some_and(|v| v == "1");
        Some(Self::new(pipe, allow_dev))
    }

    pub fn state(&self) -> &LinkState {
        &self.state
    }

    pub fn attempts(&self) -> u64 {
        self.attempts
    }

    pub(crate) fn allows_development_agent(&self) -> bool {
        self.allow_dev
    }

    fn fail(&mut self, error: LinkError) {
        let was_connected = matches!(self.state, LinkState::Connected(_));
        let changed = match &self.state {
            LinkState::Fallback(previous) => {
                std::mem::discriminant(previous) != std::mem::discriminant(&error)
            }
            _ => true,
        };
        if was_connected || changed {
            match &error {
                LinkError::Unavailable(_) if !was_connected => {
                    tracing::debug!(%error, "admin agent unavailable")
                }
                LinkError::Incompatible { .. } | LinkError::Untrusted(_) => {
                    tracing::warn!(%error, "admin agent unavailable")
                }
                _ => tracing::info!(%error, "admin agent unavailable"),
            }
        }
        self.connection = None;
        // A slow failed hello must still leave a full backoff interval.
        self.retry_at = Some(Instant::now() + retry_delay(&error));
        self.state = LinkState::Fallback(error);
    }

    fn ensure_connected(&mut self, now: Instant) -> bool {
        if self.connection.is_some() {
            return true;
        }
        if self.retry_at.is_some_and(|at| now < at) {
            return false;
        }
        self.attempts += 1;
        match Connection::open(&self.pipe, self.allow_dev, Role::Host) {
            Ok(connection) => {
                let handshake = connection.handshake;
                tracing::info!(
                    version = handshake.version,
                    agent_pid = handshake.agent_pid,
                    elevated = handshake.elevated,
                    "admin agent connected; input goes through the agent"
                );
                self.state = LinkState::Connected(handshake);
                self.connection = Some(connection);
                self.retry_at = None;
                true
            }
            Err(error) => {
                self.fail(error);
                false
            }
        }
    }

    /// Inject through the agent. `None` means nothing was sent and the
    /// caller may inject locally if the foreground target permits it;
    /// `Some((applied, error))` is the agent's
    /// receipt (possibly partial, like `SendInput`).
    pub fn inject(&mut self, records: &[Record]) -> Option<(u32, i32)> {
        if records.is_empty() || records.iter().any(|record| record.validate().is_err()) {
            return None;
        }
        let now = Instant::now();
        if !self.ensure_connected(now) {
            return None;
        }
        let mut applied_total = 0u32;
        let mut retried = false;
        let deadline = Instant::now() + INJECT_TIMEOUT;
        for (index, chunk) in records.chunks(MAX_RECORDS_PER_FRAME).enumerate() {
            let connection = loop {
                let connection = self.connection.as_mut()?;
                match connection.send_frame(chunk, deadline) {
                    Ok(()) => break connection,
                    Err(error) => {
                        // The tray stops the agent after the last viewer leaves, while the host
                        // keeps its pipe. A write that failed because the pipe is closed delivered
                        // nothing, so the first batch can safely go again on a fresh connection.
                        if !retried && index == 0 && write_never_delivered(&error) {
                            retried = true;
                            tracing::info!(
                                %error,
                                "admin agent pipe was closed before the batch was delivered; reconnecting once"
                            );
                            self.fail(LinkError::Failed(error.to_string()));
                            self.retry_at = None;
                            if !self.ensure_connected(Instant::now()) {
                                // Nothing was delivered: the caller may inject locally.
                                return None;
                            }
                            continue;
                        }
                        // A failed/cancelled write may already have delivered bytes.
                        // The receipt is unknown: never repeat this input locally.
                        self.fail(LinkError::Failed(error.to_string()));
                        return Some((applied_total, ERROR_AGENT_LOST));
                    }
                }
            };
            let reply = connection.receipt(deadline);
            match reply {
                Ok(AgentMessage::Injected {
                    requested,
                    applied,
                    error,
                }) if usize::from(requested) == chunk.len() && applied <= requested => {
                    applied_total += u32::from(applied);
                    if applied < requested {
                        return Some((applied_total, error));
                    }
                }
                Ok(AgentMessage::Refused {
                    code: RefuseCode::RateLimited,
                    ..
                }) => return Some((applied_total, ERROR_AGENT_RATE_LIMITED)),
                // The frame may have been applied: never repeat it locally.
                Ok(other) => {
                    self.fail(LinkError::Failed(format!("unexpected reply {other:?}")));
                    return Some((applied_total, ERROR_AGENT_LOST));
                }
                Err(error) => {
                    self.fail(LinkError::Failed(error.to_string()));
                    return Some((applied_total, ERROR_AGENT_LOST));
                }
            }
        }
        Some((applied_total, 0))
    }
}

/// A write that the OS refused because the pipe's other end is gone (ERROR_BROKEN_PIPE 109 or
/// ERROR_NO_DATA 232): the bytes were never accepted, so nothing reached the agent. Other errors,
/// including timeouts and our own "closed after incomplete I/O" marker, may have delivered data.
fn write_never_delivered(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(109 | 232))
}

/// Where the agent must be installed (for scripts and diagnostics).
pub fn installed_agent_path() -> Option<std::path::PathBuf> {
    trust::program_files().map(|root| root.join("BubblePlay").join("agent").join(AGENT_EXE))
}

pub fn is_agent_image(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(AGENT_EXE))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_connection(pipe: BoundedPipe) -> Connection {
        Connection {
            pipe,
            handshake: Handshake {
                version: PROTOCOL_MAX,
                agent_pid: std::process::id(),
                elevated: false,
            },
        }
    }

    fn move_one() -> Record {
        Record::Mouse {
            dx: 1,
            dy: 0,
            data: 0,
            flags: super::super::protocol::MOUSEEVENTF_MOVE,
        }
    }

    #[test]
    #[ignore = "negative control waits for the legacy path's finite peer watchdog"]
    fn legacy_synchronous_client_write_exceeds_the_deadline() {
        use std::os::windows::io::FromRawHandle;
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
        };
        let name = format!(
            r"\\.\pipe\BubblePlay.AdminAgent.legacy-control-{}",
            std::process::id()
        );
        let server = test_server::listen(&name);
        let wide_name = wide(&name);
        // SAFETY: isolated local test fixture, reproducing the old synchronous flags.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide_name.as_ptr()),
                PIPE_CLIENT_ACCESS,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION),
                None,
            )
        }
        .unwrap();
        // SAFETY: newly-created uniquely-owned client handle.
        let mut file = unsafe { File::from_raw_handle(handle.0 as _) };
        let peer = test_server::hold(server, &[]);
        let started = Instant::now();
        let _ = write_client(
            &mut file,
            &ClientMessage::Inject(vec![move_one(); MAX_RECORDS_PER_FRAME]),
        );
        let elapsed = started.elapsed();
        drop(peer);
        assert!(
            elapsed >= Duration::from_millis(900),
            "negative control did not stall: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "watchdog cleanup was not finite: {elapsed:?}"
        );
    }

    #[test]
    fn only_a_closed_pipe_write_counts_as_never_delivered() {
        assert!(write_never_delivered(&io::Error::from_raw_os_error(232)));
        assert!(write_never_delivered(&io::Error::from_raw_os_error(109)));
        assert!(!write_never_delivered(&io::Error::from_raw_os_error(5)));
        assert!(!write_never_delivered(&io::Error::new(
            io::ErrorKind::TimedOut,
            "deadline"
        )));
        assert!(!write_never_delivered(&io::Error::new(
            io::ErrorKind::BrokenPipe,
            "named pipe connection closed after incomplete I/O"
        )));
    }

    #[test]
    fn stalled_agent_write_has_a_deadline_even_when_the_pipe_is_full() {
        let (pipe, peer) = test_server::peer("full", &[]);
        let mut connection = test_connection(pipe);
        let started = Instant::now();
        let error = connection
            .send_frame(
                &vec![move_one(); MAX_RECORDS_PER_FRAME],
                started + Duration::from_millis(100),
            )
            .unwrap_err();
        let elapsed = started.elapsed();
        drop(peer); // releases the peer within a finite watchdog even on failure
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            elapsed < Duration::from_millis(500),
            "blocked for {elapsed:?}"
        );
        assert!(
            connection.pipe.file().is_err(),
            "timed out pipe must be unusable"
        );
    }

    #[test]
    fn stalled_agent_reply_has_a_deadline_without_flushing_the_write() {
        let (mut pipe, peer) = test_server::peer("silent", &[]);
        let started = Instant::now();
        let deadline = started + Duration::from_millis(100);
        send_message(
            &mut pipe,
            &ClientMessage::Hello {
                min: PROTOCOL_MIN,
                max: PROTOCOL_MAX,
                role: Role::Host,
            },
            deadline,
        )
        .unwrap();
        let error = read_reply(&mut pipe, deadline).unwrap_err();
        let elapsed = started.elapsed();
        drop(peer);
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            elapsed < Duration::from_millis(500),
            "blocked for {elapsed:?}"
        );
    }

    #[test]
    fn partial_reply_uses_the_original_deadline() {
        let mut reply = Vec::new();
        super::super::protocol::write_agent(
            &mut reply,
            &AgentMessage::Injected {
                requested: 1,
                applied: 1,
                error: 0,
            },
        )
        .unwrap();
        let (mut pipe, peer) = test_server::peer("partial", &reply[..HEADER_LEN + 1]);
        let started = Instant::now();
        let error = read_reply(&mut pipe, started + Duration::from_millis(100)).unwrap_err();
        let elapsed = started.elapsed();
        drop(peer);
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            elapsed < Duration::from_millis(500),
            "blocked for {elapsed:?}"
        );
    }

    #[test]
    fn ambiguous_first_write_never_falls_back_to_local_input() {
        let (pipe, peer) = test_server::peer("no-retry", &[]);
        let mut link = AdminAgentLink::new("unused", true);
        link.connection = Some(test_connection(pipe));
        link.state = LinkState::Connected(link.connection.as_ref().unwrap().handshake);
        let started = Instant::now();
        let result = link.inject(&vec![move_one(); MAX_RECORDS_PER_FRAME]);
        let elapsed = started.elapsed();
        drop(peer);
        assert_eq!(result, Some((0, ERROR_AGENT_LOST)));
        assert!(
            elapsed < Duration::from_millis(800),
            "blocked for {elapsed:?}"
        );
        assert!(matches!(
            link.state(),
            LinkState::Fallback(LinkError::Failed(_))
        ));
        assert_eq!(link.inject(&[move_one()]), None);
        assert_eq!(link.attempts(), 0, "failed connection must back off");
    }

    #[test]
    fn missing_agent_falls_back_and_backs_off() {
        let pipe = format!(
            r"\\.\pipe\BubblePlay.AdminAgent.unit-missing-{}",
            std::process::id()
        );
        let mut link = AdminAgentLink::new(pipe, true);
        let move_one = Record::Mouse {
            dx: 1,
            dy: 0,
            data: 0,
            flags: super::super::protocol::MOUSEEVENTF_MOVE,
        };
        assert_eq!(link.inject(&[move_one]), None);
        assert!(matches!(
            link.state(),
            LinkState::Fallback(LinkError::Unavailable(_))
        ));
        assert_eq!(link.attempts(), 1);
        // Within the retry delay no further connection is attempted.
        assert_eq!(link.inject(&[move_one]), None);
        assert_eq!(link.attempts(), 1);
    }

    #[test]
    fn slow_failed_attempt_gets_a_full_backoff_after_completion() {
        let pipe = format!(
            r"\\.\pipe\BubblePlay.AdminAgent.unit-slow-missing-{}",
            std::process::id()
        );
        let mut link = AdminAgentLink::new(pipe, true);
        let completed_after = Instant::now();
        // Advance the attempt's start time without a multi-second sleep.
        let started = completed_after - Duration::from_secs(3);
        assert!(!link.ensure_connected(started));
        assert!(link.retry_at.unwrap() >= completed_after + Duration::from_secs(2));
        assert!(!link.ensure_connected(Instant::now()));
        assert_eq!(
            link.attempts(),
            1,
            "queued input must respect the new backoff"
        );
    }

    #[test]
    fn invalid_records_never_use_the_agent() {
        let mut link = AdminAgentLink::new(r"\\.\pipe\unused", true);
        let bad = Record::Key {
            vk: 0x1FF,
            scan: 0,
            flags: 0,
        };
        assert_eq!(link.inject(&[bad]), None);
        assert_eq!(link.attempts(), 0);
        assert_eq!(link.inject(&[]), None);
    }

    #[test]
    fn retry_policy_waits_longer_for_problems_needing_an_update() {
        assert_eq!(
            retry_delay(&LinkError::Unavailable(String::new())),
            Duration::from_secs(2)
        );
        assert_eq!(
            retry_delay(&LinkError::Incompatible {
                agent_min: 2,
                agent_max: 2
            }),
            Duration::from_secs(60)
        );
        assert_eq!(
            retry_delay(&LinkError::Untrusted(String::new())),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn timed_out_hello_is_retried_once_then_reported() {
        let mut calls = 0;
        let result: Result<u8, LinkError> = retry_once_on_timeout(|| {
            calls += 1;
            Err(LinkError::TimedOut("hello reply".into()))
        });
        assert_eq!(calls, 2);
        assert_eq!(result, Err(LinkError::TimedOut("hello reply".into())));
    }

    #[test]
    fn timed_out_hello_that_recovers_on_retry_succeeds() {
        let mut calls = 0;
        let result: Result<u8, LinkError> = retry_once_on_timeout(|| {
            calls += 1;
            if calls == 1 {
                Err(LinkError::TimedOut("slow".into()))
            } else {
                Ok(7)
            }
        });
        assert_eq!(calls, 2);
        assert_eq!(result, Ok(7));
    }

    #[test]
    fn other_hello_failures_are_not_retried() {
        let mut calls = 0;
        let result: Result<u8, LinkError> = retry_once_on_timeout(|| {
            calls += 1;
            Err(LinkError::Untrusted("not the agent".into()))
        });
        assert_eq!(calls, 1);
        assert_eq!(result, Err(LinkError::Untrusted("not the agent".into())));
    }

    #[test]
    fn missed_hello_deadline_is_a_timeout_not_a_failure() {
        let timed_out = io::Error::new(io::ErrorKind::TimedOut, "named pipe operation timed out");
        assert!(matches!(
            hello_failure("hello reply", timed_out),
            LinkError::TimedOut(_)
        ));
        let broken = io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed");
        assert!(matches!(
            hello_failure("hello reply", broken),
            LinkError::Failed(_)
        ));
    }

    #[test]
    fn untrusted_pipe_server_is_rejected() {
        // This test process is not the agent image, so even a dev-allowed
        // link must refuse to talk to it.
        let pipe = format!(
            r"\\.\pipe\BubblePlay.AdminAgent.unit-squat-{}",
            std::process::id()
        );
        let server = test_server::listen(&pipe);
        let mut link = AdminAgentLink::new(pipe, true);
        let move_one = Record::Mouse {
            dx: 1,
            dy: 0,
            data: 0,
            flags: super::super::protocol::MOUSEEVENTF_MOVE,
        };
        assert_eq!(link.inject(&[move_one]), None);
        assert!(
            matches!(link.state(), LinkState::Fallback(LinkError::Untrusted(_))),
            "{:?}",
            link.state()
        );
        drop(server);
    }

    mod test_server {
        use super::*;
        use std::io::Write;
        use std::os::windows::io::FromRawHandle;
        use std::sync::mpsc;
        use std::thread;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        use windows::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
            PIPE_WAIT,
        };

        pub struct Server(HANDLE);

        /// Worker performs no blocking reads/flushes. Its finite watchdog
        /// releases a synchronous pre-fix client instead of hanging the suite.
        pub struct Peer {
            release: mpsc::Sender<()>,
            worker: Option<thread::JoinHandle<()>>,
        }

        impl Drop for Peer {
            fn drop(&mut self) {
                let _ = self.release.send(());
                if let Some(worker) = self.worker.take() {
                    worker.join().unwrap();
                }
            }
        }

        pub fn peer(suffix: &str, reply_prefix: &[u8]) -> (BoundedPipe, Peer) {
            let name = format!(
                r"\\.\pipe\BubblePlay.AdminAgent.bounded-{}-{suffix}",
                std::process::id()
            );
            let server = listen(&name);
            let pipe = open_pipe(&name).unwrap();
            let peer = hold(server, reply_prefix);
            (pipe, peer)
        }

        pub fn hold(server: Server, reply_prefix: &[u8]) -> Peer {
            // SAFETY: server's owned, listening pipe. Client already opened it.
            let connected =
                unsafe { windows::Win32::System::Pipes::ConnectNamedPipe(server.0, None) };
            assert!(
                connected.is_ok()
                    || connected.unwrap_err().code()
                        == windows::Win32::Foundation::ERROR_PIPE_CONNECTED.to_hresult()
            );
            // SAFETY: move unique server handle into File, suppressing Server's Drop.
            let mut file = unsafe { File::from_raw_handle(server.0 .0 as _) };
            std::mem::forget(server);
            if !reply_prefix.is_empty() {
                file.write_all(reply_prefix).unwrap();
            }
            let (release, wait) = mpsc::channel();
            let worker = thread::spawn(move || {
                let _ = wait.recv_timeout(Duration::from_millis(1100));
                drop(file);
            });
            Peer {
                release,
                worker: Some(worker),
            }
        }

        impl Drop for Server {
            fn drop(&mut self) {
                // SAFETY: owned handle.
                let _ = unsafe { CloseHandle(self.0) };
            }
        }

        /// A listening pipe instance owned by the test process itself.
        pub fn listen(name: &str) -> Server {
            let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            // SAFETY: NUL-terminated name; default security.
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    4096,
                    4096,
                    0,
                    None,
                )
            };
            assert!(!handle.is_invalid());
            Server(handle)
        }
    }
}
