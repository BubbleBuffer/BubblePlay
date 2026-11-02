//! The Devices UI over a per-user named pipe, for the Flutter app's "This PC"
//! section. Protocol: `docs/app/devices-pipe.md`.
//!
//! Three layers:
//! - wire types ([`ServerMessage`], [`ClientMessage`]) borrowed from the
//!   [`DevicesEvent`]s they carry, so secrets are not copied around;
//! - a pure line codec ([`encode_line`], [`decode_command`], [`LineReader`]);
//! - the pipe server ([`DevicesPipe`]), fed through a [`DevicesPipeHandle`]
//!   that implements [`DevicesUi`].
//!
//! Messages that are not device state: `show` (the tray's Devices item) and
//! `openFeedback` (the tray's "Report a problem" item: the app opens its
//! report sheet). Both are only broadcast to connected clients; the one
//! exception is `openFeedback` sent while no client is connected (the app was
//! just started), which is held for [`FEEDBACK_REPLAY_WINDOW`] and sent to the
//! next client that connects.
//!
//! Pairing codes, QR URIs and comparison numbers travel on this pipe. They
//! never reach a log line or `Debug` output; encoded lines are zeroed when
//! dropped.

use std::fmt;
use std::io::{self, Read};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    LocalFree, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE,
    HLOCAL, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE,
    FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcessToken, SetEvent, WaitForMultipleObjects,
    WaitForSingleObject, INFINITE,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use zeroize::Zeroizing;

use crate::devices_ui_api::{DeviceRow, DevicesCommand, DevicesEvent, DevicesUi, StdinRequest};

/// Protocol version sent in `hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest line either side accepts, without its `\n`.
pub const MAX_LINE: usize = 16 * 1024;

/// Pipe name prefix; the user's string SID follows.
pub const PIPE_NAME_PREFIX: &str = r"\\.\pipe\bubbleplay-devices-v1-";

/// Lines queued per client before it counts as stuck and is dropped.
const CLIENT_QUEUE: usize = 64;

/// Pipe instances (one always listening), so at most `MAX_INSTANCES - 1`
/// clients at once.
const MAX_INSTANCES: u32 = 16;

const PIPE_BUFFER: u32 = 16 * 1024;

/// How long an `openFeedback` sent with no client connected waits for one.
const FEEDBACK_REPLAY_WINDOW: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// One device of a `list` message.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireDevice<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub last_seen: Option<&'a str>,
    pub connected: bool,
}

impl<'a> From<&'a DeviceRow> for WireDevice<'a> {
    fn from(row: &'a DeviceRow) -> Self {
        Self {
            id: &row.id,
            name: &row.name,
            last_seen: row.last_seen.as_deref(),
            connected: row.connected,
        }
    }
}

/// Server to client. Deliberately no `Debug` derive: the manual impl prints
/// only the message type.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage<'a> {
    #[serde(rename_all = "camelCase")]
    Hello {
        protocol: u32,
        pc_name: &'a str,
        pc_id: &'a str,
        relay_origin: &'a str,
        relay_online: bool,
    },
    Relay {
        online: bool,
    },
    List {
        devices: Vec<WireDevice<'a>>,
    },
    #[serde(rename_all = "camelCase")]
    PairWaiting {
        code_display: &'a str,
        qr_uri: &'a str,
        pc_id: &'a str,
        expires_at_ms: u64,
    },
    PairExpired,
    PairStopped,
    PairFailed,
    PairDenied,
    #[serde(rename_all = "camelCase")]
    PairRequest {
        id: &'a str,
        name: &'a str,
        sas_display: &'a str,
    },
    PairDone {
        id: &'a str,
        name: &'a str,
    },
    SaveError {
        message: &'a str,
    },
    Show,
    OpenFeedback,
}

impl ServerMessage<'_> {
    /// The `type` value, safe to log.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::Relay { .. } => "relay",
            Self::List { .. } => "list",
            Self::PairWaiting { .. } => "pairWaiting",
            Self::PairExpired => "pairExpired",
            Self::PairStopped => "pairStopped",
            Self::PairFailed => "pairFailed",
            Self::PairDenied => "pairDenied",
            Self::PairRequest { .. } => "pairRequest",
            Self::PairDone { .. } => "pairDone",
            Self::SaveError { .. } => "saveError",
            Self::Show => "show",
            Self::OpenFeedback => "openFeedback",
        }
    }
}

impl fmt::Debug for ServerMessage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ServerMessage({})", self.kind())
    }
}

fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl<'a> From<&'a DevicesEvent> for ServerMessage<'a> {
    fn from(event: &'a DevicesEvent) -> Self {
        match event {
            DevicesEvent::List(rows) => Self::List {
                devices: rows.iter().map(WireDevice::from).collect(),
            },
            DevicesEvent::PairWaiting {
                code_display,
                qr_uri,
                pc_id,
                expires_at,
            } => Self::PairWaiting {
                code_display,
                qr_uri,
                pc_id,
                expires_at_ms: unix_ms(*expires_at),
            },
            DevicesEvent::PairExpired => Self::PairExpired,
            DevicesEvent::PairStopped => Self::PairStopped,
            DevicesEvent::PairFailed => Self::PairFailed,
            DevicesEvent::PairDenied => Self::PairDenied,
            DevicesEvent::PairRequest {
                id,
                name,
                sas_display,
            } => Self::PairRequest {
                id,
                name,
                sas_display,
            },
            DevicesEvent::PairDone { id, name } => Self::PairDone { id, name },
            DevicesEvent::SaveError(message) => Self::SaveError { message },
        }
    }
}

/// Client to server.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientMessage {
    Open,
    Revoke { id: String },
    StartPairing,
    CancelPairing,
    Allow { id: String },
    Deny { id: String },
}

impl From<ClientMessage> for DevicesCommand {
    fn from(message: ClientMessage) -> Self {
        match message {
            ClientMessage::Open => Self::Open,
            ClientMessage::Revoke { id } => Self::Revoke(id),
            ClientMessage::StartPairing => Self::StartPairing,
            ClientMessage::CancelPairing => Self::CancelPairing,
            ClientMessage::Allow { id } => Self::Allow(id),
            ClientMessage::Deny { id } => Self::Deny(id),
        }
    }
}

const CLIENT_TYPES: [&str; 6] = [
    "open",
    "revoke",
    "startPairing",
    "cancelPairing",
    "allow",
    "deny",
];

// ---------------------------------------------------------------------------
// Line codec
// ---------------------------------------------------------------------------

/// Why a client line ends the connection. `Display` never includes content.
#[derive(Debug)]
pub enum LineError {
    TooLong,
    BadUtf8,
    BadJson,
    Io(io::Error),
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => write!(f, "line longer than {MAX_LINE} bytes"),
            Self::BadUtf8 => f.write_str("line is not UTF-8"),
            Self::BadJson => f.write_str("line is not a JSON object"),
            Self::Io(error) => write!(f, "pipe I/O failed: {error}"),
        }
    }
}

/// A client line that does not end the connection.
#[derive(Debug, PartialEq, Eq)]
pub enum Decoded {
    Command(DevicesCommand),
    /// A `type` this server does not know (or no `type`): ignored.
    Unknown,
    /// A known `type` with missing or wrong fields: ignored.
    Malformed(&'static str),
}

/// An encoded server line, `\n` included, zeroed on drop.
pub type Line = Arc<Zeroizing<Vec<u8>>>;

/// One JSON line for `message`. `None` if it would be longer than
/// [`MAX_LINE`] (the client would drop the connection).
pub fn encode_line(message: &ServerMessage<'_>) -> Option<Zeroizing<Vec<u8>>> {
    // Sized up front so pairing lines are not reallocated (no stray copies).
    let mut line = Zeroizing::new(Vec::with_capacity(1024));
    serde_json::to_writer(&mut *line, message).ok()?;
    if line.len() > MAX_LINE {
        return None;
    }
    line.push(b'\n');
    Some(line)
}

/// One client line (without its `\n`) as a command.
pub fn decode_command(line: &[u8]) -> Result<Decoded, LineError> {
    if line.len() > MAX_LINE {
        return Err(LineError::TooLong);
    }
    let text = std::str::from_utf8(line).map_err(|_| LineError::BadUtf8)?;
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| LineError::BadJson)?;
    let Some(object) = value.as_object() else {
        return Err(LineError::BadJson);
    };
    let Some(kind) = object.get("type").and_then(|kind| kind.as_str()) else {
        return Ok(Decoded::Unknown);
    };
    let Some(kind) = CLIENT_TYPES.iter().copied().find(|known| *known == kind) else {
        return Ok(Decoded::Unknown);
    };
    match serde_json::from_value::<ClientMessage>(value) {
        Ok(message) => Ok(Decoded::Command(message.into())),
        Err(_) => Ok(Decoded::Malformed(kind)),
    }
}

/// Splits a byte stream into lines of at most [`MAX_LINE`] bytes.
pub struct LineReader<R> {
    inner: R,
    buffer: Vec<u8>,
    /// Bytes of `buffer` already searched for `\n`.
    scanned: usize,
}

impl<R: Read> LineReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            scanned: 0,
        }
    }

    /// The next line without its `\n`; `Ok(None)` at end of input (a partial
    /// last line is dropped).
    pub fn next_line(&mut self) -> Result<Option<Vec<u8>>, LineError> {
        loop {
            if let Some(offset) = self.buffer[self.scanned..].iter().position(|&b| b == b'\n') {
                let end = self.scanned + offset;
                if end > MAX_LINE {
                    return Err(LineError::TooLong);
                }
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                self.scanned = 0;
                return Ok(Some(line));
            }
            self.scanned = self.buffer.len();
            if self.buffer.len() > MAX_LINE {
                return Err(LineError::TooLong);
            }
            let mut chunk = [0u8; 4096];
            let read = self.inner.read(&mut chunk).map_err(LineError::Io)?;
            if read == 0 {
                return Ok(None);
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

// ---------------------------------------------------------------------------
// Win32 helpers
// ---------------------------------------------------------------------------

fn win32_code(error: &windows::core::Error) -> u32 {
    (error.code().0 as u32) & 0xffff
}

fn os_error(error: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(win32_code(&error) as i32)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A manual-reset event.
struct Event(OwnedHandle);

impl Event {
    fn new() -> io::Result<Self> {
        // SAFETY: plain unnamed event; the handle is owned below.
        let handle = unsafe { CreateEventW(None, true, false, None) }.map_err(os_error)?;
        // SAFETY: fresh handle owned by nobody else.
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle.0 as _) }))
    }

    fn raw(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle() as _)
    }

    fn set(&self) {
        // SAFETY: valid event handle.
        let _ = unsafe { SetEvent(self.raw()) };
    }

    fn is_set(&self) -> bool {
        // SAFETY: valid event handle; zero timeout.
        let waited = unsafe { WaitForSingleObject(self.raw(), 0) };
        waited == WAIT_OBJECT_0
    }
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "pipe operation cancelled")
}

/// An overlapped pipe handle (server or client end). Reads and writes from
/// different threads do not serialize, unlike a synchronous handle.
struct Pipe(OwnedHandle);

impl Pipe {
    fn raw(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle() as _)
    }

    /// Runs one overlapped operation to completion, or cancels it when
    /// `abort` is set. The OVERLAPPED and buffer stay borrowed until the
    /// operation has completed either way.
    fn overlapped(
        &self,
        abort: &Event,
        start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> io::Result<u32> {
        let done = Event::new()?;
        let mut overlapped = OVERLAPPED {
            hEvent: done.raw(),
            ..Default::default()
        };
        let pointer: *mut OVERLAPPED = &mut overlapped;
        match start(pointer) {
            Ok(()) => {}
            Err(error) if win32_code(&error) == ERROR_IO_PENDING.0 => {
                let handles = [done.raw(), abort.raw()];
                // SAFETY: both handles are valid events.
                let waited = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
                if waited != WAIT_OBJECT_0 {
                    let mut ignored = 0;
                    // SAFETY: the operation was started on this handle with
                    // `pointer`; waiting (bWait = true) keeps the buffer and
                    // OVERLAPPED alive until it has completed.
                    unsafe {
                        let _ = CancelIoEx(self.raw(), Some(pointer));
                        let _ = GetOverlappedResult(self.raw(), pointer, &mut ignored, true);
                    }
                    return Err(cancelled());
                }
            }
            Err(error) => return Err(os_error(error)),
        }
        let mut transferred = 0;
        // SAFETY: the operation has completed (event signalled or sync completion).
        unsafe { GetOverlappedResult(self.raw(), pointer, &mut transferred, false) }
            .map_err(os_error)?;
        Ok(transferred)
    }

    /// Reads some bytes; 0 when the other end closed.
    fn read(&self, buffer: &mut [u8], abort: &Event) -> io::Result<usize> {
        let handle = self.raw();
        let result = self.overlapped(abort, |overlapped| {
            // SAFETY: buffer outlives the operation (see `overlapped`).
            unsafe { ReadFile(handle, Some(buffer), None, Some(overlapped)) }
        });
        match result {
            Ok(read) => Ok(read as usize),
            Err(error) if error.raw_os_error() == Some(ERROR_BROKEN_PIPE.0 as i32) => Ok(0),
            Err(error) => Err(error),
        }
    }

    fn write_all(&self, mut bytes: &[u8], abort: &Event) -> io::Result<()> {
        let handle = self.raw();
        while !bytes.is_empty() {
            let chunk = bytes;
            let written = self.overlapped(abort, |overlapped| {
                // SAFETY: bytes outlive the operation (see `overlapped`).
                unsafe { WriteFile(handle, Some(chunk), None, Some(overlapped)) }
            })? as usize;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            bytes = &bytes[written.min(bytes.len())..];
        }
        Ok(())
    }

    /// Waits for a client. `Ok(false)` when `stop` was set.
    fn accept(&self, stop: &Event) -> io::Result<bool> {
        let done = Event::new()?;
        let mut overlapped = OVERLAPPED {
            hEvent: done.raw(),
            ..Default::default()
        };
        let pointer: *mut OVERLAPPED = &mut overlapped;
        // SAFETY: listening instance; `pointer` lives until completion below.
        match unsafe { ConnectNamedPipe(self.raw(), Some(pointer)) } {
            // A client connected between create and connect.
            Ok(()) => return Ok(true),
            Err(error) if win32_code(&error) == ERROR_PIPE_CONNECTED.0 => return Ok(true),
            Err(error) if win32_code(&error) == ERROR_IO_PENDING.0 => {}
            Err(error) => return Err(os_error(error)),
        }
        let handles = [done.raw(), stop.raw()];
        // SAFETY: both handles are valid events.
        let waited = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
        let mut ignored = 0;
        if waited != WAIT_OBJECT_0 {
            // SAFETY: as in `overlapped`: cancel, then wait for completion.
            unsafe {
                let _ = CancelIoEx(self.raw(), Some(pointer));
                let _ = GetOverlappedResult(self.raw(), pointer, &mut ignored, true);
            }
            return Ok(false);
        }
        // SAFETY: completed operation.
        unsafe { GetOverlappedResult(self.raw(), pointer, &mut ignored, false) }
            .map_err(os_error)?;
        Ok(true)
    }

    fn disconnect(&self) {
        // SAFETY: valid server pipe handle.
        let _ = unsafe { DisconnectNamedPipe(self.raw()) };
    }
}

/// A security descriptor from SDDL, freed on drop.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: immutable after creation; only read by CreateNamedPipeW.
unsafe impl Send for SecurityDescriptor {}
unsafe impl Sync for SecurityDescriptor {}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.0 .0))) };
    }
}

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let text = wide(sddl);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: NUL-terminated SDDL; descriptor freed on drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(os_error)?;
        Ok(Self(descriptor))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0 .0,
            bInheritHandle: false.into(),
        }
    }
}

/// The DACL of the pipe: the current user only, protected (no inherited
/// entries), no SYSTEM or Administrators entry.
pub fn pipe_sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;{sid})")
}

fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = PWSTR::null();
    // SAFETY: valid SID; text freed with LocalFree.
    unsafe { ConvertSidToStringSidW(sid, &mut text) }.map_err(os_error)?;
    // SAFETY: NUL-terminated string from the API.
    let result = unsafe { text.to_string() }
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "SID is not UTF-16"));
    // SAFETY: allocated by ConvertSidToStringSidW.
    let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
    result
}

/// The string SID of the user this process runs as.
pub fn current_user_sid() -> io::Result<String> {
    let mut token = HANDLE::default();
    // SAFETY: out-pointer valid; handle owned below.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.map_err(os_error)?;
    // SAFETY: fresh token handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token.0 as _) };
    // u64 elements keep TOKEN_USER aligned.
    let mut buffer = vec![0u64; 64];
    let mut returned = 0u32;
    // SAFETY: buffer is writable for its byte length.
    unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle() as _),
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            (buffer.len() * 8) as u32,
            &mut returned,
        )
    }
    .map_err(os_error)?;
    // SAFETY: GetTokenInformation filled an aligned TOKEN_USER at the start.
    let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    sid_string(user.User.Sid)
}

/// The production pipe name for a user SID.
pub fn pipe_name_for_sid(sid: &str) -> String {
    format!("{PIPE_NAME_PREFIX}{sid}")
}

/// Spawns a thread that logs through the spawner's tracing dispatcher.
fn spawn_named(name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    let dispatch = tracing::dispatcher::get_default(|dispatch| dispatch.clone());
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || tracing::dispatcher::with_default(&dispatch, body))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// What the pipe server needs to start.
pub struct DevicesPipeConfig {
    /// [`pipe_name_for_sid`] in production; tests pass a unique name.
    pub pipe_name: String,
    /// The user SID the DACL grants access to.
    pub sid: String,
    pub pc_name: String,
    pub pc_id: String,
    pub relay_origin: String,
    /// True while the host holds its relay connection.
    pub relay_online: Arc<AtomicBool>,
    /// How often `relay_online` is polled for `relay` messages.
    pub relay_poll: Duration,
}

/// The open pairing, as encoded lines for replay.
enum Pairing {
    None,
    Waiting(Line),
    Request(Line),
}

struct Client {
    id: u64,
    queue: SyncSender<Line>,
    kill: Arc<Event>,
}

struct State {
    clients: Vec<Client>,
    last_list: Option<Line>,
    pairing: Pairing,
    relay_online: bool,
    /// When an `openFeedback` arrived with nobody connected to receive it.
    feedback_pending: Option<Instant>,
}

struct Shared {
    pipe_name: String,
    security: SecurityDescriptor,
    pc_name: String,
    pc_id: String,
    relay_origin: String,
    relay_flag: Arc<AtomicBool>,
    commands: Mutex<Sender<DevicesCommand>>,
    stop: Event,
    state: Mutex<State>,
    client_threads: Mutex<Vec<JoinHandle<()>>>,
    next_id: AtomicU64,
}

/// The running pipe server. Dropping it stops accepting, disconnects every
/// client and joins its threads.
pub struct DevicesPipe {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

/// Feeds events to the pipe; cheap to clone.
#[derive(Clone)]
pub struct DevicesPipeHandle {
    shared: Arc<Shared>,
}

impl DevicesPipe {
    /// Creates the first pipe instance (failing if the name is taken) and
    /// starts the accept and relay threads. Nothing blocks if no client ever
    /// connects.
    pub fn start(config: DevicesPipeConfig, commands: Sender<DevicesCommand>) -> io::Result<Self> {
        let security = SecurityDescriptor::from_sddl(&pipe_sddl(&config.sid))?;
        let relay_online = config.relay_online.load(Ordering::Relaxed);
        let shared = Arc::new(Shared {
            pipe_name: config.pipe_name,
            security,
            pc_name: config.pc_name,
            pc_id: config.pc_id,
            relay_origin: config.relay_origin,
            relay_flag: config.relay_online,
            commands: Mutex::new(commands),
            stop: Event::new()?,
            state: Mutex::new(State {
                clients: Vec::new(),
                last_list: None,
                pairing: Pairing::None,
                relay_online,
                feedback_pending: None,
            }),
            client_threads: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        });
        let first = shared.create_instance(true)?;
        let mut pipe = Self {
            shared: shared.clone(),
            threads: Vec::new(),
        };
        let accept_shared = shared.clone();
        pipe.threads
            .push(spawn_named("devices-pipe-accept", move || {
                accept_shared.accept_loop(first)
            })?);
        let poll_shared = shared.clone();
        let poll = config.relay_poll;
        pipe.threads
            .push(spawn_named("devices-pipe-relay", move || {
                poll_shared.relay_loop(poll)
            })?);
        tracing::info!("Devices pipe listening");
        Ok(pipe)
    }

    pub fn handle(&self) -> DevicesPipeHandle {
        DevicesPipeHandle {
            shared: self.shared.clone(),
        }
    }

    #[cfg(test)]
    fn client_count(&self) -> usize {
        lock(&self.shared.state).clients.len()
    }
}

impl Drop for DevicesPipe {
    fn drop(&mut self) {
        self.shared.stop.set();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        let clients = std::mem::take(&mut lock(&self.shared.state).clients);
        for client in &clients {
            client.kill.set();
        }
        drop(clients);
        let threads = std::mem::take(&mut *lock(&self.shared.client_threads));
        for thread in threads {
            let _ = thread.join();
        }
    }
}

impl DevicesPipeHandle {
    /// Sends `show` to every connected client (not kept for replay).
    pub fn show_app(&self) {
        let mut state = lock(&self.shared.state);
        if let Some(line) = encode_line(&ServerMessage::Show) {
            self.shared.broadcast(&mut state, &Arc::new(line));
        }
    }

    /// Sends `openFeedback` to every connected client. With none connected
    /// (the app is still starting) it is held briefly for the next one.
    pub fn open_feedback(&self) {
        let mut state = lock(&self.shared.state);
        if state.clients.is_empty() {
            tracing::info!("open-feedback held: no Devices pipe client yet");
            state.feedback_pending = Some(Instant::now());
        } else if let Some(line) = encode_line(&ServerMessage::OpenFeedback) {
            self.shared.broadcast(&mut state, &Arc::new(line));
        }
    }
}

impl DevicesUi for DevicesPipeHandle {
    fn show(&mut self, event: DevicesEvent) {
        let message = ServerMessage::from(&event);
        let kind = message.kind();
        let Some(line) = encode_line(&message) else {
            tracing::warn!(kind, "Devices pipe message too long; not sent");
            return;
        };
        let line: Line = Arc::new(line);
        let mut state = lock(&self.shared.state);
        match &event {
            DevicesEvent::List(_) => state.last_list = Some(line.clone()),
            DevicesEvent::PairWaiting { .. } => state.pairing = Pairing::Waiting(line.clone()),
            DevicesEvent::PairRequest { .. } => state.pairing = Pairing::Request(line.clone()),
            DevicesEvent::PairExpired
            | DevicesEvent::PairStopped
            | DevicesEvent::PairFailed
            | DevicesEvent::PairDenied
            | DevicesEvent::PairDone { .. } => state.pairing = Pairing::None,
            DevicesEvent::SaveError(_) => {}
        }
        self.shared.broadcast(&mut state, &line);
    }
}

impl Shared {
    fn create_instance(&self, first: bool) -> io::Result<Pipe> {
        let name = wide(&self.pipe_name);
        let attributes = self.security.attributes();
        let mut open_mode = PIPE_ACCESS_DUPLEX.0 | FILE_FLAG_OVERLAPPED.0;
        if first {
            // Fail instead of joining a pipe someone else already created.
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE.0;
        }
        // SAFETY: NUL-terminated name; attributes outlive the call.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(open_mode),
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                MAX_INSTANCES,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                Some(&attributes),
            )
        };
        if handle.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh pipe handle.
        Ok(Pipe(unsafe { OwnedHandle::from_raw_handle(handle.0 as _) }))
    }

    /// Accepts clients until `stop`. One instance is always listening, so
    /// the pipe name never lapses while the server runs.
    fn accept_loop(self: Arc<Self>, mut listening: Pipe) {
        loop {
            match listening.accept(&self.stop) {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    tracing::warn!(%error, "Devices pipe connect failed");
                    listening.disconnect();
                    // SAFETY: valid event handle; short back-off.
                    if unsafe { WaitForSingleObject(self.stop.raw(), 100) } != WAIT_TIMEOUT {
                        break;
                    }
                    continue;
                }
            }
            if self.stop.is_set() {
                break;
            }
            match self.create_instance(false) {
                Ok(next) => {
                    let connected = std::mem::replace(&mut listening, next);
                    self.add_client(connected);
                }
                Err(error) => {
                    // Keep the connected instance as the listening one.
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) {
                        tracing::warn!("Devices pipe has too many clients; rejecting one");
                    } else {
                        tracing::warn!(%error, "creating a Devices pipe instance failed");
                    }
                    listening.disconnect();
                    // SAFETY: valid event handle; short back-off.
                    if unsafe { WaitForSingleObject(self.stop.raw(), 100) } != WAIT_TIMEOUT {
                        break;
                    }
                }
            }
        }
        listening.disconnect();
        tracing::info!("Devices pipe stopped");
    }

    fn relay_loop(self: Arc<Self>, poll: Duration) {
        let poll_ms = u32::try_from(poll.as_millis()).unwrap_or(u32::MAX).max(1);
        // SAFETY: valid event handle.
        while unsafe { WaitForSingleObject(self.stop.raw(), poll_ms) } == WAIT_TIMEOUT {
            let online = self.relay_flag.load(Ordering::Relaxed);
            let mut state = lock(&self.state);
            if state.relay_online != online {
                state.relay_online = online;
                if let Some(line) = encode_line(&ServerMessage::Relay { online }) {
                    self.broadcast(&mut state, &Arc::new(line));
                }
            }
        }
    }

    /// Queues `line` for every client; a client whose queue is full (stuck)
    /// or gone is dropped. Never blocks on a client.
    fn broadcast(&self, state: &mut State, line: &Line) {
        state
            .clients
            .retain(|client| match client.queue.try_send(line.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    tracing::warn!(client = client.id, "Devices pipe client too slow; dropped");
                    client.kill.set();
                    false
                }
                Err(TrySendError::Disconnected(_)) => {
                    client.kill.set();
                    false
                }
            });
    }

    fn remove_client(&self, id: u64) {
        let mut state = lock(&self.state);
        if let Some(index) = state.clients.iter().position(|client| client.id == id) {
            let client = state.clients.remove(index);
            client.kill.set();
            tracing::info!(client = id, "Devices pipe client left");
        }
    }

    /// The `hello` line and the current state, in protocol order.
    fn replay(&self, state: &mut State) -> Vec<Line> {
        let mut lines = Vec::new();
        let hello = ServerMessage::Hello {
            protocol: PROTOCOL_VERSION,
            pc_name: &self.pc_name,
            pc_id: &self.pc_id,
            relay_origin: &self.relay_origin,
            relay_online: state.relay_online,
        };
        if let Some(line) = encode_line(&hello) {
            lines.push(Arc::new(line));
        }
        if let Some(list) = &state.last_list {
            lines.push(list.clone());
        }
        match &state.pairing {
            Pairing::None => {}
            Pairing::Waiting(line) | Pairing::Request(line) => lines.push(line.clone()),
        }
        if let Some(since) = state.feedback_pending.take() {
            if since.elapsed() <= FEEDBACK_REPLAY_WINDOW {
                if let Some(line) = encode_line(&ServerMessage::OpenFeedback) {
                    lines.push(Arc::new(line));
                }
            }
        }
        lines
    }

    fn add_client(self: &Arc<Self>, pipe: Pipe) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let kill = match Event::new() {
            Ok(kill) => Arc::new(kill),
            Err(error) => {
                tracing::warn!(%error, "Devices pipe client setup failed");
                pipe.disconnect();
                return;
            }
        };
        let pipe = Arc::new(pipe);
        let (queue, lines) = std::sync::mpsc::sync_channel::<Line>(CLIENT_QUEUE);
        {
            // Replay and registration under one lock: no event is missed or
            // sent twice between them.
            let mut state = lock(&self.state);
            // Greet with the relay state as it is now, not as last polled.
            let online = self.relay_flag.load(Ordering::Relaxed);
            if state.relay_online != online {
                state.relay_online = online;
                if let Some(line) = encode_line(&ServerMessage::Relay { online }) {
                    self.broadcast(&mut state, &Arc::new(line));
                }
            }
            for line in self.replay(&mut state) {
                let _ = queue.try_send(line);
            }
            state.clients.push(Client {
                id,
                queue,
                kill: kill.clone(),
            });
        }
        tracing::info!(client = id, "Devices pipe client connected");
        let writer = {
            let (shared, pipe, kill) = (self.clone(), pipe.clone(), kill.clone());
            spawn_named("devices-pipe-write", move || {
                for line in lines {
                    if pipe.write_all(&line, &kill).is_err() {
                        break;
                    }
                }
                shared.remove_client(id);
            })
        };
        let commands = lock(&self.commands).clone();
        let reader = {
            let (shared, pipe, kill, commands) =
                (self.clone(), pipe.clone(), kill.clone(), commands.clone());
            spawn_named("devices-pipe-read", move || {
                read_client(&pipe, &kill, &commands, id);
                shared.remove_client(id);
            })
        };
        let mut threads = lock(&self.client_threads);
        threads.retain(|thread| !thread.is_finished());
        for thread in [writer, reader] {
            match thread {
                Ok(thread) => threads.push(thread),
                Err(error) => {
                    tracing::warn!(%error, "Devices pipe client thread failed");
                    self.remove_client(id);
                }
            }
        }
        drop(threads);
        // A fresh list for the new client (and the others).
        let _ = commands.send(DevicesCommand::Open);
    }
}

/// Reads client lines and forwards their commands until the client leaves,
/// sends a bad line, or is killed.
fn read_client(pipe: &Pipe, kill: &Event, commands: &Sender<DevicesCommand>, id: u64) {
    struct PipeRead<'a>(&'a Pipe, &'a Event);
    impl Read for PipeRead<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.0.read(buffer, self.1)
        }
    }
    let mut reader = LineReader::new(PipeRead(pipe, kill));
    loop {
        match reader.next_line() {
            Ok(Some(line)) => match decode_command(&line) {
                Ok(Decoded::Command(command)) => {
                    if commands.send(command).is_err() {
                        return;
                    }
                }
                Ok(Decoded::Unknown) => {
                    tracing::info!(client = id, "Devices pipe: unknown command ignored")
                }
                Ok(Decoded::Malformed(kind)) => {
                    tracing::info!(client = id, kind, "Devices pipe: malformed command ignored")
                }
                Err(error) => {
                    tracing::info!(client = id, %error, "Devices pipe: closing client");
                    return;
                }
            },
            Ok(None) => return,
            Err(LineError::Io(_)) => return,
            Err(error) => {
                tracing::info!(client = id, %error, "Devices pipe: closing client");
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Host wiring
// ---------------------------------------------------------------------------

/// Gives every event to `ui` (the Win32 window or a logger) and to the pipe.
pub struct TeeDevicesUi<U> {
    pub ui: U,
    pub pipe: Option<DevicesPipeHandle>,
}

impl<U: DevicesUi> DevicesUi for TeeDevicesUi<U> {
    fn show(&mut self, event: DevicesEvent) {
        if let Some(pipe) = &mut self.pipe {
            pipe.show(event.clone());
        }
        self.ui.show(event);
    }
}

/// Handles the tray's stdin requests: `open-devices` opens the Win32 window
/// (`open_window`) and asks for a fresh list; `show-devices-app` only tells
/// pipe clients to show Devices; `open-feedback` tells them to open the report
/// sheet. Ends when `requests` or `commands` closes.
pub fn route_stdin_requests(
    requests: Receiver<StdinRequest>,
    mut open_window: impl FnMut(),
    commands: Sender<DevicesCommand>,
    pipe: Option<DevicesPipeHandle>,
) {
    for request in requests {
        match request {
            StdinRequest::OpenDevices => {
                open_window();
                if commands.send(DevicesCommand::Open).is_err() {
                    return;
                }
            }
            StdinRequest::ShowDevicesApp => match &pipe {
                Some(pipe) => pipe.show_app(),
                None => tracing::info!("show-devices-app ignored: Devices pipe not running"),
            },
            StdinRequest::OpenFeedback => match &pipe {
                Some(pipe) => pipe.open_feedback(),
                None => tracing::info!("open-feedback ignored: Devices pipe not running"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows::Win32::Security::{
        AclSizeInformation, GetAce, GetAclInformation, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION,
    };
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_NONE, OPEN_EXISTING};
    use windows::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    const CODE: &str = "70Q1K-9MXR1";
    const QR: &str = "bubbleplay://pair?relay=x&pc=y&code=70Q1K9MXR1";
    const SAS: &str = "830 687";
    const WAIT: Duration = Duration::from_secs(5);

    fn waiting() -> DevicesEvent {
        DevicesEvent::PairWaiting {
            code_display: CODE.into(),
            qr_uri: QR.into(),
            pc_id: "77VH-7M56-8GJK-Y12J".into(),
            expires_at: SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123),
        }
    }

    fn request() -> DevicesEvent {
        DevicesEvent::PairRequest {
            id: "47Z3-3QX1-AJH6-2RKB".into(),
            name: "Pixel 8".into(),
            sas_display: SAS.into(),
        }
    }

    fn list() -> DevicesEvent {
        DevicesEvent::List(vec![
            DeviceRow {
                id: "47Z3-3QX1-AJH6-2RKB".into(),
                name: "Pixel \"8\"".into(),
                last_seen: Some("2026-10-30T12:00:00Z".into()),
                connected: true,
            },
            DeviceRow {
                id: "ABCD".into(),
                name: "Laptop".into(),
                last_seen: None,
                connected: false,
            },
        ])
    }

    fn text(message: &ServerMessage<'_>) -> String {
        let line = encode_line(message).expect("encodes");
        assert_eq!(line.last(), Some(&b'\n'));
        String::from_utf8(line[..line.len() - 1].to_vec()).expect("utf8")
    }

    fn event_text(event: &DevicesEvent) -> String {
        text(&ServerMessage::from(event))
    }

    // -- codec ------------------------------------------------------------

    #[test]
    fn golden_json_for_every_server_message() {
        assert_eq!(
            text(&ServerMessage::Hello {
                protocol: PROTOCOL_VERSION,
                pc_name: "DESK",
                pc_id: "77VH-7M56-8GJK-Y12J",
                relay_origin: "https://play.example.com",
                relay_online: true,
            }),
            r#"{"type":"hello","protocol":1,"pcName":"DESK","pcId":"77VH-7M56-8GJK-Y12J","relayOrigin":"https://play.example.com","relayOnline":true}"#
        );
        assert_eq!(
            text(&ServerMessage::Relay { online: false }),
            r#"{"type":"relay","online":false}"#
        );
        assert_eq!(
            event_text(&list()),
            r#"{"type":"list","devices":[{"id":"47Z3-3QX1-AJH6-2RKB","name":"Pixel \"8\"","lastSeen":"2026-10-30T12:00:00Z","connected":true},{"id":"ABCD","name":"Laptop","lastSeen":null,"connected":false}]}"#
        );
        assert_eq!(
            event_text(&DevicesEvent::List(Vec::new())),
            r#"{"type":"list","devices":[]}"#
        );
        assert_eq!(
            event_text(&waiting()),
            r#"{"type":"pairWaiting","codeDisplay":"70Q1K-9MXR1","qrUri":"bubbleplay://pair?relay=x&pc=y&code=70Q1K9MXR1","pcId":"77VH-7M56-8GJK-Y12J","expiresAtMs":1700000000123}"#
        );
        assert_eq!(
            event_text(&DevicesEvent::PairExpired),
            r#"{"type":"pairExpired"}"#
        );
        assert_eq!(
            event_text(&DevicesEvent::PairStopped),
            r#"{"type":"pairStopped"}"#
        );
        assert_eq!(
            event_text(&DevicesEvent::PairFailed),
            r#"{"type":"pairFailed"}"#
        );
        assert_eq!(
            event_text(&DevicesEvent::PairDenied),
            r#"{"type":"pairDenied"}"#
        );
        assert_eq!(
            event_text(&request()),
            r#"{"type":"pairRequest","id":"47Z3-3QX1-AJH6-2RKB","name":"Pixel 8","sasDisplay":"830 687"}"#
        );
        assert_eq!(
            event_text(&DevicesEvent::PairDone {
                id: "47Z3".into(),
                name: "Pixel 8".into()
            }),
            r#"{"type":"pairDone","id":"47Z3","name":"Pixel 8"}"#
        );
        assert_eq!(
            event_text(&DevicesEvent::SaveError("disk\nfull".into())),
            r#"{"type":"saveError","message":"disk\nfull"}"#
        );
        assert_eq!(text(&ServerMessage::Show), r#"{"type":"show"}"#);
        assert_eq!(
            text(&ServerMessage::OpenFeedback),
            r#"{"type":"openFeedback"}"#
        );
        assert_eq!(ServerMessage::OpenFeedback.kind(), "openFeedback");
    }

    #[test]
    fn expiry_before_the_epoch_is_zero() {
        let event = DevicesEvent::PairWaiting {
            code_display: String::new(),
            qr_uri: String::new(),
            pc_id: String::new(),
            expires_at: SystemTime::UNIX_EPOCH - Duration::from_secs(1),
        };
        assert!(event_text(&event).contains(r#""expiresAtMs":0"#));
    }

    #[test]
    fn overlong_server_lines_are_not_encoded() {
        let event = DevicesEvent::SaveError("x".repeat(MAX_LINE));
        assert!(encode_line(&ServerMessage::from(&event)).is_none());
    }

    #[test]
    fn every_client_command_decodes() {
        let cases: [(&str, DevicesCommand); 6] = [
            (r#"{"type":"open"}"#, DevicesCommand::Open),
            (
                r#"{"type":"revoke","id":"ABCD"}"#,
                DevicesCommand::Revoke("ABCD".into()),
            ),
            (r#"{"type":"startPairing"}"#, DevicesCommand::StartPairing),
            (r#"{"type":"cancelPairing"}"#, DevicesCommand::CancelPairing),
            (
                r#"{"id":"ABCD","type":"allow"}"#,
                DevicesCommand::Allow("ABCD".into()),
            ),
            (
                r#" {"type":"deny","id":"ABCD","extra":1} "#,
                DevicesCommand::Deny("ABCD".into()),
            ),
        ];
        for (line, command) in cases {
            assert_eq!(
                decode_command(line.as_bytes()).expect(line),
                Decoded::Command(command)
            );
        }
        // Trailing \r of a CRLF client is whitespace.
        assert_eq!(
            decode_command(b"{\"type\":\"open\"}\r").expect("crlf"),
            Decoded::Command(DevicesCommand::Open)
        );
    }

    #[test]
    fn unknown_and_malformed_commands_are_ignored() {
        for line in [
            r#"{"type":"format-disk"}"#,
            r#"{"type":"Open"}"#,
            r#"{"type":7}"#,
            r#"{}"#,
        ] {
            assert_eq!(
                decode_command(line.as_bytes()).expect(line),
                Decoded::Unknown
            );
        }
        assert_eq!(
            decode_command(br#"{"type":"revoke"}"#).expect("no id"),
            Decoded::Malformed("revoke")
        );
        assert_eq!(
            decode_command(br#"{"type":"allow","id":5}"#).expect("bad id"),
            Decoded::Malformed("allow")
        );
    }

    #[test]
    fn bad_lines_end_the_connection() {
        assert!(matches!(
            decode_command(b"{\"type\":\"open\xff\"}"),
            Err(LineError::BadUtf8)
        ));
        for line in [&b"{\"type\":"[..], b"", b"[1,2]", b"\"open\"", b"null"] {
            assert!(
                matches!(decode_command(line), Err(LineError::BadJson)),
                "{line:?}"
            );
        }
        let long = format!(r#"{{"type":"open","pad":"{}"}}"#, "x".repeat(MAX_LINE));
        assert!(matches!(
            decode_command(long.as_bytes()),
            Err(LineError::TooLong)
        ));
    }

    /// Hands out its bytes a few at a time, like a pipe.
    struct Trickle(Vec<u8>, usize);

    impl Read for Trickle {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let n = buffer.len().min(self.1).min(self.0.len());
            buffer[..n].copy_from_slice(&self.0[..n]);
            self.0.drain(..n);
            Ok(n)
        }
    }

    #[test]
    fn line_reader_splits_lines_across_reads() {
        let mut reader = LineReader::new(Trickle(b"ab\n\ncd\nlast".to_vec(), 3));
        assert_eq!(reader.next_line().expect("1"), Some(b"ab".to_vec()));
        assert_eq!(reader.next_line().expect("2"), Some(b"".to_vec()));
        assert_eq!(reader.next_line().expect("3"), Some(b"cd".to_vec()));
        // A partial last line is dropped at end of input.
        assert_eq!(reader.next_line().expect("eof"), None);
    }

    #[test]
    fn line_reader_accepts_max_and_rejects_longer_lines() {
        let mut input = vec![b'a'; MAX_LINE];
        input.push(b'\n');
        let mut reader = LineReader::new(Trickle(input, 5000));
        assert_eq!(
            reader.next_line().expect("max").map(|line| line.len()),
            Some(MAX_LINE)
        );

        let mut input = vec![b'a'; MAX_LINE + 1];
        input.push(b'\n');
        let mut reader = LineReader::new(Trickle(input, 5000));
        assert!(matches!(reader.next_line(), Err(LineError::TooLong)));

        // No newline ever: stops at the cap instead of growing.
        let mut reader = LineReader::new(io::repeat(b'a'));
        assert!(matches!(reader.next_line(), Err(LineError::TooLong)));
        assert!(reader.buffer.len() <= MAX_LINE + 4096);
    }

    #[test]
    fn debug_output_never_shows_secrets() {
        let (waiting, request) = (waiting(), request());
        for message in [ServerMessage::from(&waiting), ServerMessage::from(&request)] {
            let debug = format!("{message:?}");
            assert!(!debug.contains("70Q1K") && !debug.contains("bubbleplay://"));
            assert!(!debug.contains(SAS), "{debug}");
        }
        for error in [LineError::TooLong, LineError::BadUtf8, LineError::BadJson] {
            let shown = format!("{error} {error:?}");
            assert!(!shown.contains("70Q1K"), "{shown}");
        }
    }

    // -- pipe helpers -----------------------------------------------------

    fn unique_name() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!(
            r"\\.\pipe\bubbleplay-devices-v1-test-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    struct Fixture {
        pipe: DevicesPipe,
        name: String,
        commands: Receiver<DevicesCommand>,
        relay: Arc<AtomicBool>,
    }

    fn start() -> Fixture {
        start_named(unique_name()).expect("pipe starts")
    }

    fn config(name: String, relay: Arc<AtomicBool>) -> DevicesPipeConfig {
        DevicesPipeConfig {
            pipe_name: name,
            sid: current_user_sid().expect("sid"),
            pc_name: "DESK".into(),
            pc_id: "77VH-7M56-8GJK-Y12J".into(),
            relay_origin: "https://play.example.com".into(),
            relay_online: relay,
            relay_poll: Duration::from_millis(20),
        }
    }

    fn start_named(name: String) -> io::Result<Fixture> {
        let relay = Arc::new(AtomicBool::new(false));
        let (tx, commands) = mpsc::channel();
        let pipe = DevicesPipe::start(config(name.clone(), relay.clone()), tx)?;
        Ok(Fixture {
            pipe,
            name,
            commands,
            relay,
        })
    }

    impl Fixture {
        fn handle(&self) -> DevicesPipeHandle {
            self.pipe.handle()
        }

        fn command(&self) -> DevicesCommand {
            self.commands.recv_timeout(WAIT).expect("command")
        }

        fn no_command(&self) {
            assert!(self
                .commands
                .recv_timeout(Duration::from_millis(200))
                .is_err());
        }

        /// Connects a client and consumes the `Open` the server issues for it.
        fn connect(&self) -> TestClient {
            let client = TestClient::connect(&self.name);
            assert_eq!(self.command(), DevicesCommand::Open);
            client
        }

        fn wait_clients(&self, count: usize) {
            let deadline = Instant::now() + WAIT;
            while self.pipe.client_count() != count {
                assert!(Instant::now() < deadline, "client count {count}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn open_raw(name: &str) -> Pipe {
        let wide_name = wide(name);
        // SAFETY: NUL-terminated name; handle owned by Pipe.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide_name.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .expect("client connects");
        // SAFETY: fresh handle.
        Pipe(unsafe { OwnedHandle::from_raw_handle(handle.0 as _) })
    }

    struct PipeRead(Arc<Pipe>, Arc<Event>);

    impl Read for PipeRead {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.0.read(buffer, &self.1)
        }
    }

    struct TestClient {
        pipe: Arc<Pipe>,
        kill: Arc<Event>,
        lines: Receiver<Vec<u8>>,
        reader: Option<JoinHandle<()>>,
    }

    impl TestClient {
        fn connect(name: &str) -> Self {
            let pipe = Arc::new(open_raw(name));
            let kill = Arc::new(Event::new().expect("event"));
            let (tx, lines) = mpsc::channel();
            let mut reader = LineReader::new(PipeRead(pipe.clone(), kill.clone()));
            let reader = std::thread::spawn(move || {
                while let Ok(Some(line)) = reader.next_line() {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
            Self {
                pipe,
                kill,
                lines,
                reader: Some(reader),
            }
        }

        fn next(&self) -> serde_json::Value {
            let line = self.lines.recv_timeout(WAIT).expect("a line from the host");
            serde_json::from_slice(&line).expect("json line")
        }

        fn next_type(&self) -> String {
            self.next()["type"].as_str().expect("type").to_owned()
        }

        fn nothing_more(&self) {
            assert!(self.lines.recv_timeout(Duration::from_millis(200)).is_err());
        }

        fn send(&self, bytes: &[u8]) {
            self.pipe
                .write_all(bytes, &self.kill)
                .expect("client write");
        }

        fn expect_closed(&self) {
            let deadline = Instant::now() + WAIT;
            loop {
                match self.lines.recv_timeout(Duration::from_millis(50)) {
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    _ => assert!(Instant::now() < deadline, "connection should close"),
                }
            }
        }

        /// Checks `hello`, then returns the `type`s of `count` more lines.
        fn greeting(&self, count: usize) -> Vec<String> {
            let hello = self.next();
            assert_eq!(hello["type"], "hello");
            assert_eq!(hello["protocol"], 1);
            assert_eq!(hello["pcName"], "DESK");
            assert_eq!(hello["pcId"], "77VH-7M56-8GJK-Y12J");
            assert_eq!(hello["relayOrigin"], "https://play.example.com");
            (0..count).map(|_| self.next_type()).collect()
        }
    }

    impl Drop for TestClient {
        fn drop(&mut self) {
            self.kill.set();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    // -- pipe server ------------------------------------------------------

    #[test]
    fn dacl_grants_the_current_user_only() {
        let fixture = start();
        let client = fixture.connect();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: valid handle opened with READ_CONTROL (GENERIC_READ).
        let status = unsafe {
            GetSecurityInfo(
                client.pipe.raw(),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                Some(&mut descriptor),
            )
        };
        assert_eq!(status.0, 0, "GetSecurityInfo");
        assert!(!dacl.is_null(), "the pipe must have a DACL");
        let mut size = ACL_SIZE_INFORMATION::default();
        // SAFETY: valid ACL from GetSecurityInfo; out-struct sized correctly.
        unsafe {
            GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        }
        .expect("acl info");
        let sid = current_user_sid().expect("sid");
        let mut sids = Vec::new();
        for index in 0..size.AceCount {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            // SAFETY: index below AceCount.
            unsafe { GetAce(dacl, index, &mut ace) }.expect("ace");
            // SAFETY: every ACE starts with an ACE_HEADER.
            let header = unsafe { &*(ace as *const ACE_HEADER) };
            assert_eq!(
                u32::from(header.AceType),
                ACCESS_ALLOWED_ACE_TYPE,
                "only allow entries"
            );
            // SAFETY: an ACCESS_ALLOWED_ACE; the SID starts at SidStart.
            let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            let ace_sid = PSID(std::ptr::addr_of!(allowed.SidStart) as *mut _);
            sids.push(sid_string(ace_sid).expect("ace sid"));
        }
        // SAFETY: allocated by GetSecurityInfo.
        let _ = unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
        assert_eq!(sids, vec![sid]);
        assert_eq!(pipe_sddl("S-1-5-21-1"), "D:P(A;;GA;;;S-1-5-21-1)");
    }

    #[test]
    fn production_name_uses_the_user_sid() {
        let sid = current_user_sid().expect("sid");
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert_eq!(
            pipe_name_for_sid(&sid),
            format!(r"\\.\pipe\bubbleplay-devices-v1-{sid}")
        );
    }

    #[test]
    fn a_second_server_cannot_take_the_name() {
        let fixture = start();
        let error = start_named(fixture.name.clone())
            .err()
            .expect("second server must fail");
        assert_eq!(error.raw_os_error(), Some(5), "{error}");
        // The first one still serves.
        let client = fixture.connect();
        assert!(client.greeting(0).is_empty());
    }

    #[test]
    fn client_commands_reach_the_host() {
        let fixture = start();
        let client = fixture.connect();
        client.greeting(0);
        client.send(
            concat!(
                r#"{"type":"open"}"#,
                "\n",
                r#"{"type":"revoke","id":"R1"}"#,
                "\n",
                r#"{"type":"futureThing","x":1}"#,
                "\n",
                r#"{"type":"startPairing"}"#,
                "\r\n",
                r#"{"type":"allow"}"#,
                "\n",
                r#"{"type":"cancelPairing"}"#,
                "\n",
                r#"{"type":"allow","id":"A1"}"#,
                "\n",
                r#"{"type":"deny","id":"D1"}"#,
                "\n",
            )
            .as_bytes(),
        );
        for expected in [
            DevicesCommand::Open,
            DevicesCommand::Revoke("R1".into()),
            DevicesCommand::StartPairing,
            DevicesCommand::CancelPairing,
            DevicesCommand::Allow("A1".into()),
            DevicesCommand::Deny("D1".into()),
        ] {
            assert_eq!(fixture.command(), expected);
        }
        fixture.no_command();
        fixture.wait_clients(1);
    }

    #[test]
    fn bad_lines_close_only_that_client() {
        let fixture = start();
        let good = fixture.connect();
        good.greeting(0);
        let mut overlong = vec![b' '; MAX_LINE + 10];
        overlong.push(b'\n');
        let bad_lines: [&[u8]; 4] = [
            b"{\"type\":\"open\xfe\"}\n",
            b"not json\n",
            b"[]\n",
            &overlong,
        ];
        for bad in bad_lines {
            let client = fixture.connect();
            client.greeting(0);
            // The host may close before the whole line is written.
            let _ = client.pipe.write_all(bad, &client.kill);
            client.expect_closed();
            fixture.wait_clients(1);
        }
        fixture.no_command();
        good.send(b"{\"type\":\"open\"}\n");
        assert_eq!(fixture.command(), DevicesCommand::Open);
    }

    #[test]
    fn events_fan_out_to_every_client() {
        let fixture = start();
        let mut handle = fixture.handle();
        let (a, b) = (fixture.connect(), fixture.connect());
        assert!(a.greeting(0).is_empty() && b.greeting(0).is_empty());
        handle.show(list());
        handle.show(DevicesEvent::PairDone {
            id: "ID".into(),
            name: "Pixel".into(),
        });
        for client in [&a, &b] {
            assert_eq!(client.next()["devices"][0]["name"], "Pixel \"8\"");
            let done = client.next();
            assert_eq!(done["type"], "pairDone");
            assert_eq!(done["id"], "ID");
        }
        handle.show_app();
        assert_eq!(a.next_type(), "show");
        assert_eq!(b.next_type(), "show");
    }

    #[test]
    fn late_clients_get_the_current_state() {
        let fixture = start();
        let mut handle = fixture.handle();
        handle.show(list());
        handle.show(waiting());
        let first = fixture.connect();
        assert_eq!(first.greeting(2), ["list", "pairWaiting"]);

        handle.show(request());
        handle.show(DevicesEvent::SaveError("disk full".into()));
        let second = fixture.connect();
        assert_eq!(second.greeting(1), ["list"]);
        let pending = second.next();
        assert_eq!(pending["type"], "pairRequest");
        assert_eq!(pending["sasDisplay"], SAS);
        second.nothing_more();

        handle.show(DevicesEvent::PairDenied);
        let third = fixture.connect();
        assert_eq!(third.greeting(1), ["list"]);
        third.nothing_more();
        assert_eq!(first.next_type(), "pairRequest");
    }

    #[test]
    fn relay_changes_are_sent_and_greeted() {
        let fixture = start();
        let early = fixture.connect();
        assert_eq!(early.next()["relayOnline"], false);
        fixture.relay.store(true, Ordering::Relaxed);
        let relay = early.next();
        assert_eq!(relay["type"], "relay");
        assert_eq!(relay["online"], true);
        let late = fixture.connect();
        assert_eq!(late.next()["relayOnline"], true);
        fixture.relay.store(false, Ordering::Relaxed);
        assert_eq!(early.next()["online"], false);
        assert_eq!(late.next()["online"], false);
    }

    #[test]
    fn a_stuck_client_is_dropped_without_blocking() {
        let fixture = start();
        let mut handle = fixture.handle();
        // Never reads.
        let stuck = open_raw(&fixture.name);
        assert_eq!(fixture.command(), DevicesCommand::Open);
        let live = fixture.connect();
        live.greeting(0);
        fixture.wait_clients(2);
        let padding = "x".repeat(4000);
        let mut slowest = Duration::ZERO;
        for index in 0..400 {
            let started = Instant::now();
            handle.show(DevicesEvent::SaveError(format!("{index} {padding}")));
            slowest = slowest.max(started.elapsed());
            let message = live.next();
            assert!(message["message"]
                .as_str()
                .expect("message")
                .starts_with(&format!("{index} ")));
        }
        assert!(
            slowest < Duration::from_secs(1),
            "show blocked for {slowest:?}"
        );
        fixture.wait_clients(1);
        // The stuck client's end sees the disconnect (reads end, no hang).
        let (done_tx, done) = mpsc::channel();
        std::thread::spawn(move || {
            let kill = Event::new().expect("event");
            let mut buffer = vec![0u8; 64 * 1024];
            while let Ok(read) = stuck.read(&mut buffer, &kill) {
                if read == 0 {
                    break;
                }
            }
            let _ = done_tx.send(());
        });
        done.recv_timeout(WAIT).expect("stuck client disconnected");
    }

    #[test]
    fn leaving_clients_are_cleaned_up_and_shutdown_is_clean() {
        let fixture = start();
        let leaving = fixture.connect();
        let staying = fixture.connect();
        fixture.wait_clients(2);
        drop(leaving);
        fixture.wait_clients(1);
        staying.greeting(0);
        let started = Instant::now();
        let Fixture { pipe, name, .. } = fixture;
        drop(pipe);
        assert!(started.elapsed() < WAIT);
        staying.expect_closed();
        // The name is free again.
        drop(start_named(name).expect("restart after shutdown"));
    }

    #[test]
    fn nothing_blocks_without_clients() {
        let fixture = start();
        let mut handle = fixture.handle();
        for _ in 0..1000 {
            handle.show(list());
        }
        handle.show_app();
        fixture.no_command();
    }

    #[test]
    fn show_app_reaches_clients_and_never_opens_the_window() {
        let fixture = start();
        let client = fixture.connect();
        client.greeting(0);
        let (requests_tx, requests) = mpsc::channel();
        let (commands_tx, commands) = mpsc::channel();
        let opened = Arc::new(AtomicU64::new(0));
        let router = {
            let (opened, pipe) = (opened.clone(), Some(fixture.handle()));
            std::thread::spawn(move || {
                route_stdin_requests(
                    requests,
                    move || {
                        opened.fetch_add(1, Ordering::Relaxed);
                    },
                    commands_tx,
                    pipe,
                )
            })
        };
        requests_tx
            .send(StdinRequest::ShowDevicesApp)
            .expect("send");
        assert_eq!(client.next_type(), "show");
        assert_eq!(opened.load(Ordering::Relaxed), 0);
        assert!(commands.recv_timeout(Duration::from_millis(100)).is_err());
        requests_tx.send(StdinRequest::OpenDevices).expect("send");
        assert_eq!(
            commands.recv_timeout(WAIT).expect("open"),
            DevicesCommand::Open
        );
        assert_eq!(opened.load(Ordering::Relaxed), 1);
        client.nothing_more();
        drop(requests_tx);
        router.join().expect("router");
    }

    #[test]
    fn open_feedback_reaches_clients() {
        let fixture = start();
        let client = fixture.connect();
        client.greeting(0);
        let (requests_tx, requests) = mpsc::channel();
        let (commands_tx, commands) = mpsc::channel();
        let router = {
            let pipe = Some(fixture.handle());
            std::thread::spawn(move || route_stdin_requests(requests, || {}, commands_tx, pipe))
        };
        requests_tx.send(StdinRequest::OpenFeedback).expect("send");
        assert_eq!(client.next_type(), "openFeedback");
        assert!(commands.recv_timeout(Duration::from_millis(100)).is_err());
        client.nothing_more();
        drop(requests_tx);
        router.join().expect("router");
    }

    #[test]
    fn open_feedback_without_a_client_waits_for_the_next_one_once() {
        let fixture = start();
        fixture.handle().open_feedback();
        let first = fixture.connect();
        first.greeting(0);
        assert_eq!(first.next_type(), "openFeedback");
        first.nothing_more();
        let second = fixture.connect();
        second.greeting(0);
        second.nothing_more();
    }

    #[test]
    fn open_feedback_without_a_pipe_is_ignored() {
        let (requests_tx, requests) = mpsc::channel();
        let (commands_tx, commands) = mpsc::channel();
        requests_tx.send(StdinRequest::OpenFeedback).expect("send");
        drop(requests_tx);
        route_stdin_requests(requests, || {}, commands_tx, None);
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn show_app_without_a_pipe_is_ignored() {
        let (requests_tx, requests) = mpsc::channel();
        let (commands_tx, commands) = mpsc::channel();
        requests_tx
            .send(StdinRequest::ShowDevicesApp)
            .expect("send");
        drop(requests_tx);
        let mut opened = 0;
        route_stdin_requests(requests, || opened += 1, commands_tx, None);
        assert_eq!(opened, 0);
        assert!(commands.try_recv().is_err());
    }

    struct Recording(Arc<Mutex<Vec<DevicesEvent>>>);

    impl DevicesUi for Recording {
        fn show(&mut self, event: DevicesEvent) {
            lock(&self.0).push(event);
        }
    }

    #[test]
    fn tee_feeds_the_window_and_the_pipe() {
        let fixture = start();
        let client = fixture.connect();
        client.greeting(0);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut tee = TeeDevicesUi {
            ui: Recording(seen.clone()),
            pipe: Some(fixture.handle()),
        };
        tee.show(DevicesEvent::PairExpired);
        assert_eq!(client.next_type(), "pairExpired");
        assert_eq!(*lock(&seen), vec![DevicesEvent::PairExpired]);
        let mut alone = TeeDevicesUi {
            ui: Recording(seen.clone()),
            pipe: None,
        };
        alone.show(DevicesEvent::PairStopped);
        assert_eq!(lock(&seen).len(), 2);
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            lock(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn secrets_never_reach_the_log() {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            // Callsites first hit by another test before this subscriber existed
            // may have cached "never"; recompute interest now that it is live.
            tracing::callsite::rebuild_interest_cache();
            let fixture = start();
            let mut handle = fixture.handle();
            let client = fixture.connect();
            client.greeting(0);
            handle.show(waiting());
            handle.show(request());
            assert_eq!(client.next_type(), "pairWaiting");
            assert_eq!(client.next_type(), "pairRequest");
            let late = fixture.connect();
            assert_eq!(late.greeting(1), ["pairRequest"]);
            client.send(format!("{{\"type\":\"{CODE}\"}}\n").as_bytes());
            client.send(format!("{{\"type\":\"allow\",\"x\":\"{SAS}\"}}\n").as_bytes());
            client.send(format!("{QR}\n").as_bytes());
            client.expect_closed();
            drop(late);
            let Fixture { pipe, .. } = fixture;
            drop(pipe);
        });
        let logged = String::from_utf8(lock(&capture.0).clone()).expect("utf8 log");
        assert!(logged.contains("Devices pipe client connected"), "{logged}");
        assert!(logged.contains("unknown command ignored"), "{logged}");
        assert!(logged.contains("malformed command ignored"), "{logged}");
        for secret in [CODE, "70Q1K9MXR1", "bubbleplay://", SAS] {
            assert!(!logged.contains(secret), "{secret} in {logged}");
        }
    }
}
