//! Host / tray side of the admin input agent pipe.
//!
//! `AdminAgentLink` is used by the host's `InputSink`: every `SendInput`
//! batch goes to the elevated agent when one is connected and speaks a
//! compatible protocol version; otherwise the caller injects locally at user
//! level. Connection attempts are lazy (on input) and spaced out, so a host
//! without Admin mode pays one failed `CreateFile` every few seconds at most.

use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_PIPE_BUSY, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows::Win32::System::Pipes::{GetNamedPipeServerProcessId, PeekNamedPipe, WaitNamedPipeW};

use super::protocol::{
    read_frame, write_client, AgentMessage, ClientMessage, Record, RefuseCode, Role, ENV_ALLOW_DEV_AGENT,
    ENV_PIPE, HEADER_LEN, MAX_RECORDS_PER_FRAME, PROTOCOL_MAX, PROTOCOL_MIN,
};
use super::trust;

/// Agent binary name; the host only talks to a server running this image.
pub const AGENT_EXE: &str = "bubbleplay_admin_agent.exe";
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
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
        }
    }
}

/// Delay before the next connection attempt after a failure.
pub fn retry_delay(error: &LinkError) -> Duration {
    match error {
        LinkError::Unavailable(_) | LinkError::Failed(_) => Duration::from_secs(2),
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
    file: File,
    pub handshake: Handshake,
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn open_pipe(pipe: &str) -> Result<File, LinkError> {
    let name = wide(pipe);
    let mut waited = false;
    loop {
        // SAFETY: name is NUL-terminated; the handle is owned by the File below.
        let opened = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                PIPE_CLIENT_ACCESS,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION),
                None,
            )
        };
        match opened {
            // SAFETY: CreateFileW returned a valid, owned handle.
            Ok(handle) => return Ok(unsafe { File::from_raw_handle(handle.0 as _) }),
            // The agent is between accepting one client and listening again.
            Err(error) if !waited && error.code() == ERROR_PIPE_BUSY.to_hresult() => {
                waited = true;
                // SAFETY: NUL-terminated name.
                let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 1_000) };
            }
            Err(error) => return Err(LinkError::Unavailable(error.message())),
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

fn available(file: &File) -> io::Result<u32> {
    let mut available = 0u32;
    // SAFETY: valid pipe handle; only the byte count is requested.
    unsafe { PeekNamedPipe(handle(file), None, 0, None, Some(&mut available), None) }
        .map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xFFFF))?;
    Ok(available)
}

/// Read one agent message, failing at `deadline` instead of blocking.
fn read_reply(file: &mut File, deadline: Instant) -> io::Result<AgentMessage> {
    let mut spins = 0u32;
    loop {
        let bytes = available(file)? as usize;
        if bytes >= HEADER_LEN {
            let mut header = [0u8; HEADER_LEN];
            let mut peeked = 0u32;
            // SAFETY: header is writable for its length.
            unsafe {
                PeekNamedPipe(
                    handle(file),
                    Some(header.as_mut_ptr().cast()),
                    HEADER_LEN as u32,
                    Some(&mut peeked),
                    None,
                    None,
                )
            }
            .map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xFFFF))?;
            let len = u16::from_le_bytes([header[6], header[7]]) as usize;
            if bytes >= HEADER_LEN + len {
                let (kind, payload) = read_frame(file)?;
                return AgentMessage::decode(kind, &payload);
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "admin agent reply timed out",
            ));
        }
        spins += 1;
        if spins < 200 {
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Connection {
    /// Connect, verify the server and perform the versioned hello.
    pub fn open(pipe: &str, allow_dev: bool, role: Role) -> Result<Self, LinkError> {
        let mut file = open_pipe(pipe)?;
        verify_server(&file, allow_dev)?;
        write_client(
            &mut file,
            &ClientMessage::Hello {
                min: PROTOCOL_MIN,
                max: PROTOCOL_MAX,
                role,
            },
        )
        .map_err(|error| LinkError::Failed(format!("hello: {error}")))?;
        match read_reply(&mut file, Instant::now() + HELLO_TIMEOUT) {
            Ok(AgentMessage::Welcome {
                version,
                agent_pid,
                elevated,
                ..
            }) if (PROTOCOL_MIN..=PROTOCOL_MAX).contains(&version) => Ok(Self {
                file,
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
            Ok(other) => Err(LinkError::Failed(format!("unexpected hello reply {other:?}"))),
            Err(error) => Err(LinkError::Failed(format!("hello reply: {error}"))),
        }
    }

    fn send_frame(&mut self, records: &[Record]) -> io::Result<()> {
        write_client(&mut self.file, &ClientMessage::Inject(records.to_vec()))
    }

    fn receipt(&mut self) -> io::Result<AgentMessage> {
        read_reply(&mut self.file, Instant::now() + INJECT_TIMEOUT)
    }
}

/// Result of the tray's availability/version check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    Ready(Handshake),
    Incompatible { agent_min: u16, agent_max: u16 },
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
        Err(error) => ProbeResult::Unavailable(error.to_string()),
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

    fn fail(&mut self, error: LinkError, now: Instant) {
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
                    tracing::debug!(%error, "using user-level input")
                }
                LinkError::Incompatible { .. } | LinkError::Untrusted(_) => {
                    tracing::warn!(%error, "using user-level input")
                }
                _ => tracing::info!(%error, "using user-level input"),
            }
        }
        self.connection = None;
        self.retry_at = Some(now + retry_delay(&error));
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
                self.fail(error, now);
                false
            }
        }
    }

    /// Inject through the agent. `None` means nothing was sent and the
    /// caller must inject locally; `Some((applied, error))` is the agent's
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
        for (index, chunk) in records.chunks(MAX_RECORDS_PER_FRAME).enumerate() {
            let connection = self.connection.as_mut()?;
            if let Err(error) = connection.send_frame(chunk) {
                // The agent went away before receiving this frame: nothing of
                // the first frame was applied, so the caller injects locally.
                self.fail(LinkError::Unavailable(error.to_string()), now);
                return (index > 0).then_some((applied_total, ERROR_AGENT_LOST));
            }
            let reply = connection.receipt();
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
                    self.fail(LinkError::Failed(format!("unexpected reply {other:?}")), now);
                    return Some((applied_total, ERROR_AGENT_LOST));
                }
                Err(error) => {
                    self.fail(LinkError::Failed(error.to_string()), now);
                    return Some((applied_total, ERROR_AGENT_LOST));
                }
            }
        }
        Some((applied_total, 0))
    }
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
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        use windows::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
            PIPE_WAIT,
        };

        pub struct Server(HANDLE);

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
