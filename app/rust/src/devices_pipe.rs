//! Client of the host's Devices pipe (`docs/app/devices-pipe.md`).
//!
//! The client is thin: it moves JSON lines and nothing else. Every server line goes to the sink
//! verbatim, and Dart parses and owns the state. Two synthetic lines report the link itself:
//! `{"type":"connected"}` right after a verified connect, and
//! `{"type":"disconnected","reason":...}` whenever the link is absent or lost (once per state
//! change, not once per retry).
//!
//! Pairing codes, QR URIs and comparison numbers travel on this pipe. Nothing in this module
//! logs, prints or formats the content of a line into an error message.
//!
//! On targets other than Windows the client compiles to a stub that reports `disconnected` with
//! reason `unsupported` once and refuses every command.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

/// A server line over this size (without its newline) ends the connection; commands are bound by
/// the same limit.
pub const MAX_LINE: usize = 16 * 1024;

/// How the client reports that the link is down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// No pipe of that name exists: the host is not running (or has no Devices server).
    HostNotRunning,
    /// The pipe exists but its server is not the current user's: nothing was sent.
    NotTrusted,
    /// The server broke the framing: line too long, invalid UTF-8, not a JSON object.
    Protocol,
    /// Any other I/O failure, including the server closing the pipe.
    Io,
    /// Not a Windows build.
    Unsupported,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::HostNotRunning => "hostNotRunning",
            Reason::NotTrusted => "notTrusted",
            Reason::Protocol => "protocol",
            Reason::Io => "io",
            Reason::Unsupported => "unsupported",
        }
    }
}

/// The synthetic line sent right after a verified connect.
pub fn connected_line() -> String {
    r#"{"type":"connected"}"#.to_owned()
}

/// The synthetic line sent when the link is absent or lost.
pub fn disconnected_line(reason: Reason) -> String {
    format!(
        r#"{{"type":"disconnected","reason":"{}"}}"#,
        reason.as_str()
    )
}

/// Receives raw JSON lines. Returns `false` when the consumer is gone; the client then stops.
pub type Sink = Arc<dyn Fn(String) -> bool + Send + Sync>;

/// Client settings. `Default` is the production setting.
#[derive(Clone, Debug)]
pub struct Config {
    /// First wait before a reconnect attempt; doubles up to `backoff_max`.
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// The SID the server must run as, as raw SID bytes. `None` means the current process's
    /// user, which is what production uses; tests set another value to exercise the rejection.
    pub trusted_sid: Option<Vec<u8>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(5),
            trusted_sid: None,
        }
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn next_backoff(current: Duration, max: Duration) -> Duration {
    (current * 2).min(max)
}

/// Whether two binary SIDs are the same identity.
pub fn sids_equal(a: &[u8], b: &[u8]) -> bool {
    !a.is_empty() && a == b
}

/// Splits the byte stream into validated lines.
#[derive(Default)]
pub struct LineBuffer {
    buf: Vec<u8>,
}

impl LineBuffer {
    /// Adds bytes and appends every complete line to `out` (without its newline). Lines already
    /// complete when an error is found are still appended before the error is returned.
    pub fn push(&mut self, data: &[u8], out: &mut Vec<String>) -> Result<(), Reason> {
        self.buf.extend_from_slice(data);
        loop {
            let Some(end) = self.buf.iter().position(|&b| b == b'\n') else {
                return if self.buf.len() > MAX_LINE {
                    Err(Reason::Protocol)
                } else {
                    Ok(())
                };
            };
            if end > MAX_LINE {
                return Err(Reason::Protocol);
            }
            let line: Vec<u8> = self.buf.drain(..=end).take(end).collect();
            let text = String::from_utf8(line).map_err(|_| Reason::Protocol)?;
            match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(_)) => out.push(text),
                _ => return Err(Reason::Protocol),
            }
        }
    }
}

/// Command types the server understands, and whether each carries a string `id`.
const COMMANDS: [(&str, bool); 6] = [
    ("open", false),
    ("revoke", true),
    ("startPairing", false),
    ("cancelPairing", false),
    ("allow", true),
    ("deny", true),
];

/// Checks a client command before it is written. The error text never contains the line.
pub fn validate_command(line: &str) -> Result<(), String> {
    if line.len() > MAX_LINE {
        return Err("command too long".to_owned());
    }
    if line.contains(['\n', '\r']) {
        return Err("command must be one line".to_owned());
    }
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(line) else {
        return Err("command is not a JSON object".to_owned());
    };
    let Some(kind) = map.get("type").and_then(Value::as_str) else {
        return Err("command has no type".to_owned());
    };
    let Some(&(_, needs_id)) = COMMANDS.iter().find(|(name, _)| *name == kind) else {
        return Err("unknown command type".to_owned());
    };
    if needs_id && !matches!(map.get("id"), Some(Value::String(id)) if !id.is_empty()) {
        return Err("command needs a string id".to_owned());
    }
    Ok(())
}

pub use imp::{pipe_name_for_current_user, DevicesClient};

#[cfg(windows)]
mod imp {
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, LocalFree, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING,
        GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{
        GetLengthSid, GetTokenInformation, TokenUser, PSID, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING,
        SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows::Win32::System::Threading::{
        CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken, ResetEvent, SetEvent,
        WaitForMultipleObjects, WaitForSingleObject, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    use super::{
        connected_line, disconnected_line, next_backoff, sids_equal, validate_command, Config,
        LineBuffer, Reason, Sink,
    };

    /// A write that does not finish in this time drops the connection.
    const WRITE_TIMEOUT_MS: u32 = 5_000;
    /// Commands waiting for the connection thread; more are refused.
    const MAX_QUEUED: usize = 32;
    const READ_CHUNK: usize = 4096;

    /// An owned kernel handle.
    struct Handle(HANDLE);

    // SAFETY: a kernel handle is a plain id; every use below is thread safe.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: the handle is owned and closed once.
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    fn event(manual_reset: bool) -> Option<Handle> {
        // SAFETY: plain call; the result is owned by `Handle`.
        unsafe { CreateEventW(None, manual_reset, false, PCWSTR::null()) }
            .ok()
            .map(Handle)
    }

    /// The user SID of a token, as raw SID bytes.
    fn token_user_sid(token: HANDLE) -> Option<Vec<u8>> {
        let mut len = 0u32;
        // SAFETY: a size query with no buffer.
        unsafe {
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        }
        if len == 0 {
            return None;
        }
        // u64 elements keep the buffer aligned for TOKEN_USER.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: the buffer is at least `len` bytes; the SID pointer points into it.
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr().cast::<c_void>()),
                len,
                &mut len,
            )
            .ok()?;
            let user = &*(buf.as_ptr().cast::<TOKEN_USER>());
            let sid = user.User.Sid;
            let n = GetLengthSid(sid) as usize;
            if n == 0 {
                return None;
            }
            Some(std::slice::from_raw_parts(sid.0.cast::<u8>(), n).to_vec())
        }
    }

    fn process_user_sid(process: HANDLE) -> Option<Vec<u8>> {
        let mut token = HANDLE::default();
        // SAFETY: `token` receives a handle that `Handle` then owns.
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.ok()?;
        let token = Handle(token);
        token_user_sid(token.0)
    }

    fn own_sid() -> Option<Vec<u8>> {
        // SAFETY: the pseudo handle of the current process needs no closing.
        process_user_sid(unsafe { GetCurrentProcess() })
    }

    fn sid_to_string(sid: &[u8]) -> Option<String> {
        let mut text = PWSTR::null();
        // SAFETY: `sid` is a complete SID; the returned string is freed with LocalFree.
        unsafe {
            ConvertSidToStringSidW(PSID(sid.as_ptr().cast_mut().cast::<c_void>()), &mut text)
                .ok()?;
            let out = text.to_string().ok();
            let _ = LocalFree(Some(HLOCAL(text.0.cast::<c_void>())));
            out
        }
    }

    /// `\\.\pipe\bubbleplay-devices-v1-<SID>` for the current process's user.
    pub fn pipe_name_for_current_user() -> Result<String, String> {
        let sid = own_sid()
            .and_then(|sid| sid_to_string(&sid))
            .ok_or_else(|| "cannot read the current user's SID".to_owned())?;
        Ok(format!(r"\\.\pipe\bubbleplay-devices-v1-{sid}"))
    }

    /// The user SID of the process serving the pipe.
    fn server_sid(pipe: HANDLE) -> Option<Vec<u8>> {
        let mut pid = 0u32;
        // SAFETY: plain calls; the process handle is owned by `Handle`.
        unsafe {
            GetNamedPipeServerProcessId(pipe, &mut pid).ok()?;
            let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?);
            process_user_sid(process.0)
        }
    }

    fn open_failure(error: &windows::core::Error) -> Reason {
        if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() {
            Reason::HostNotRunning
        } else if error.code() == ERROR_ACCESS_DENIED.to_hresult() {
            // The pipe exists but is closed to us: not the host we expect.
            Reason::NotTrusted
        } else {
            Reason::Io
        }
    }

    /// Opens the pipe and verifies its server. Nothing is written before the check passes.
    fn connect(name: &str, expected: Option<&[u8]>) -> Result<Handle, Reason> {
        let wide: Vec<u16> = std::ffi::OsStr::new(name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `wide` is NUL terminated and outlives the call. Identification level keeps
        // the server from acting as us.
        let opened = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                None,
            )
        };
        let pipe = Handle(opened.map_err(|e| open_failure(&e))?);
        let Some(expected) = expected else {
            return Err(Reason::NotTrusted);
        };
        match server_sid(pipe.0) {
            Some(sid) if sids_equal(&sid, expected) => Ok(pipe),
            _ => Err(Reason::NotTrusted),
        }
    }

    enum End {
        Stopped,
        Down(Reason),
    }

    struct Outbox {
        connected: bool,
        queue: VecDeque<String>,
    }

    struct Shared {
        stop: Handle,
        wake: Handle,
        out: Mutex<Outbox>,
    }

    impl Shared {
        fn outbox(&self) -> std::sync::MutexGuard<'_, Outbox> {
            self.out.lock().unwrap_or_else(|p| p.into_inner())
        }
    }

    /// One pending overlapped read. Dropping it cancels the read and waits for the cancel, so
    /// the buffer and the `OVERLAPPED` are never freed under the kernel.
    struct ReadOp {
        pipe: HANDLE,
        event: HANDLE,
        ov: Box<OVERLAPPED>,
        buf: Box<[u8; READ_CHUNK]>,
        pending: bool,
    }

    impl ReadOp {
        fn new(pipe: HANDLE, event: HANDLE) -> Self {
            Self {
                pipe,
                event,
                ov: Box::default(),
                buf: Box::new([0; READ_CHUNK]),
                pending: false,
            }
        }

        fn start(&mut self) -> Result<(), Reason> {
            *self.ov = OVERLAPPED {
                hEvent: self.event,
                ..Default::default()
            };
            // SAFETY: the event is valid; `ov` and `buf` are boxed and live until the read is
            // finished or cancelled (see Drop).
            unsafe {
                let _ = ResetEvent(self.event);
                match ReadFile(
                    self.pipe,
                    Some(&mut self.buf[..]),
                    None,
                    Some(std::ptr::from_mut(&mut *self.ov)),
                ) {
                    Ok(()) => {
                        self.pending = true;
                        Ok(())
                    }
                    Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                        self.pending = true;
                        Ok(())
                    }
                    Err(_) => Err(Reason::Io),
                }
            }
        }

        /// The bytes of the finished read; call once the event is signalled.
        fn finish(&mut self) -> Result<&[u8], Reason> {
            let mut n = 0u32;
            // SAFETY: the read was started on this handle with this `ov`.
            let result = unsafe { GetOverlappedResult(self.pipe, &*self.ov, &mut n, false) };
            self.pending = false;
            match result {
                Ok(()) if n > 0 => Ok(&self.buf[..n as usize]),
                _ => Err(Reason::Io),
            }
        }
    }

    impl Drop for ReadOp {
        fn drop(&mut self) {
            if self.pending {
                let mut n = 0u32;
                // SAFETY: cancels our own read and waits until the kernel is done with it.
                unsafe {
                    let _ = CancelIoEx(self.pipe, Some(&*self.ov));
                    let _ = GetOverlappedResult(self.pipe, &*self.ov, &mut n, true);
                }
            }
        }
    }

    /// Writes one line. Waits for completion, the stop event, or the write timeout.
    fn write_line(pipe: HANDLE, event: HANDLE, stop: HANDLE, line: &str) -> Result<(), End> {
        let mut data = Vec::with_capacity(line.len() + 1);
        data.extend_from_slice(line.as_bytes());
        data.push(b'\n');
        let mut ov = Box::new(OVERLAPPED {
            hEvent: event,
            ..Default::default()
        });
        let mut n = 0u32;
        // SAFETY: `data` and `ov` stay alive until the write has finished or been cancelled.
        unsafe {
            let _ = ResetEvent(event);
            match WriteFile(pipe, Some(&data), None, Some(std::ptr::from_mut(&mut *ov))) {
                Ok(()) => {}
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                    let waited = WaitForMultipleObjects(&[event, stop], false, WRITE_TIMEOUT_MS);
                    if waited != WAIT_OBJECT_0 {
                        let _ = CancelIoEx(pipe, Some(&*ov));
                        let _ = GetOverlappedResult(pipe, &*ov, &mut n, true);
                        return Err(if waited == WAIT_EVENT(WAIT_OBJECT_0.0 + 1) {
                            End::Stopped
                        } else {
                            End::Down(Reason::Io)
                        });
                    }
                }
                Err(_) => return Err(End::Down(Reason::Io)),
            }
            if GetOverlappedResult(pipe, &*ov, &mut n, true).is_err() || n as usize != data.len() {
                return Err(End::Down(Reason::Io));
            }
        }
        Ok(())
    }

    /// Reads lines into the sink and writes queued commands until the link ends.
    fn serve(pipe: &Handle, shared: &Shared, sink: &Sink) -> End {
        let (Some(read_event), Some(write_event)) = (event(true), event(true)) else {
            return End::Down(Reason::Io);
        };
        let mut read = ReadOp::new(pipe.0, read_event.0);
        let mut lines = LineBuffer::default();
        let mut out = Vec::new();
        loop {
            if let Err(reason) = read.start() {
                return End::Down(reason);
            }
            loop {
                let handles = [read_event.0, shared.stop.0, shared.wake.0];
                // SAFETY: three valid handles.
                let waited = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
                match waited.0.wrapping_sub(WAIT_OBJECT_0.0) {
                    0 => break,
                    1 => return End::Stopped,
                    2 => loop {
                        let Some(command) = shared.outbox().queue.pop_front() else {
                            break;
                        };
                        if let Err(end) = write_line(pipe.0, write_event.0, shared.stop.0, &command)
                        {
                            return end;
                        }
                    },
                    _ => return End::Down(Reason::Io),
                }
            }
            out.clear();
            let pushed = match read.finish() {
                Ok(bytes) => lines.push(bytes, &mut out),
                Err(reason) => return End::Down(reason),
            };
            for line in out.drain(..) {
                if !sink(line) {
                    return End::Stopped;
                }
            }
            if let Err(reason) = pushed {
                return End::Down(reason);
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Status {
        Connected,
        Down(Reason),
    }

    fn stopped(shared: &Shared) -> bool {
        // SAFETY: a valid event handle.
        unsafe { WaitForSingleObject(shared.stop.0, 0) == WAIT_OBJECT_0 }
    }

    /// Waits out the backoff; `false` when the stop event fired meanwhile.
    fn pause(shared: &Shared, wait: std::time::Duration) -> bool {
        let ms = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: a valid event handle.
        unsafe { WaitForSingleObject(shared.stop.0, ms) == WAIT_TIMEOUT }
    }

    fn supervise(pipe_name: &str, sink: &Sink, config: &Config, shared: &Shared) {
        let mut last: Option<Status> = None;
        // Reports a status change to the sink; `false` when the sink is gone.
        let mut announce = |status: Status| -> bool {
            if last == Some(status) {
                return true;
            }
            last = Some(status);
            sink(match status {
                Status::Connected => connected_line(),
                Status::Down(reason) => disconnected_line(reason),
            })
        };
        let expected = config.trusted_sid.clone().or_else(own_sid);
        let mut backoff = config.backoff_initial;
        loop {
            if stopped(shared) {
                return;
            }
            match connect(pipe_name, expected.as_deref()) {
                Ok(pipe) => {
                    backoff = config.backoff_initial;
                    if !announce(Status::Connected) {
                        return;
                    }
                    {
                        let mut out = shared.outbox();
                        out.queue.clear();
                        out.connected = true;
                    }
                    let end = serve(&pipe, shared, sink);
                    {
                        let mut out = shared.outbox();
                        out.connected = false;
                        out.queue.clear();
                    }
                    drop(pipe);
                    match end {
                        End::Stopped => return,
                        End::Down(reason) => {
                            if !announce(Status::Down(reason)) {
                                return;
                            }
                        }
                    }
                }
                Err(reason) => {
                    if !announce(Status::Down(reason)) {
                        return;
                    }
                }
            }
            if !pause(shared, backoff) {
                return;
            }
            backoff = next_backoff(backoff, config.backoff_max);
        }
    }

    /// A running connection supervisor. Dropping it (or `stop`) ends the thread and closes the
    /// pipe.
    pub struct DevicesClient {
        shared: Arc<Shared>,
        thread: Option<JoinHandle<()>>,
    }

    impl DevicesClient {
        /// Starts connecting to `pipe_name`; lines go to `sink`.
        pub fn start(pipe_name: String, sink: Sink, config: Config) -> Self {
            let shared = match (event(true), event(false)) {
                (Some(stop), Some(wake)) => Some(Arc::new(Shared {
                    stop,
                    wake,
                    out: Mutex::new(Outbox {
                        connected: false,
                        queue: VecDeque::new(),
                    }),
                })),
                _ => None,
            };
            let Some(shared) = shared else {
                sink(disconnected_line(Reason::Io));
                return Self {
                    shared: Arc::new(Shared {
                        stop: Handle(HANDLE::default()),
                        wake: Handle(HANDLE::default()),
                        out: Mutex::new(Outbox {
                            connected: false,
                            queue: VecDeque::new(),
                        }),
                    }),
                    thread: None,
                };
            };
            let worker = shared.clone();
            let thread = std::thread::Builder::new()
                .name("devices-pipe".to_owned())
                .spawn(move || supervise(&pipe_name, &sink, &config, &worker))
                .ok();
            Self { shared, thread }
        }

        /// Starts against the real pipe of the current user.
        pub fn start_default(sink: Sink) -> Self {
            match pipe_name_for_current_user() {
                Ok(name) => Self::start(name, sink, Config::default()),
                Err(_) => {
                    sink(disconnected_line(Reason::Io));
                    Self::start(String::new(), Arc::new(|_| false), Config::default())
                }
            }
        }

        /// Validates one command and queues it for the connection. Never logs the line.
        pub fn send(&self, line: &str) -> Result<(), String> {
            validate_command(line)?;
            {
                let mut out = self.shared.outbox();
                if !out.connected {
                    return Err("the host is not connected".to_owned());
                }
                if out.queue.len() >= MAX_QUEUED {
                    return Err("too many commands waiting".to_owned());
                }
                out.queue.push_back(line.to_owned());
            }
            // SAFETY: a valid event handle.
            unsafe {
                let _ = SetEvent(self.shared.wake.0);
            }
            Ok(())
        }

        /// Ends the supervisor and waits for it. Safe to call twice.
        pub fn stop(&mut self) {
            if self.shared.stop.0.is_invalid() {
                return;
            }
            // SAFETY: a valid event handle.
            unsafe {
                let _ = SetEvent(self.shared.stop.0);
            }
            if let Some(thread) = self.thread.take() {
                if thread.thread().id() != std::thread::current().id() {
                    let _ = thread.join();
                }
            }
        }
    }

    impl Drop for DevicesClient {
        fn drop(&mut self) {
            self.stop();
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{disconnected_line, Config, Reason, Sink};

    pub fn pipe_name_for_current_user() -> Result<String, String> {
        Err("the Devices pipe is Windows only".to_owned())
    }

    /// Reports `unsupported` once and refuses every command.
    pub struct DevicesClient;

    impl DevicesClient {
        pub fn start(_pipe_name: String, sink: Sink, _config: Config) -> Self {
            sink(disconnected_line(Reason::Unsupported));
            Self
        }

        pub fn start_default(sink: Sink) -> Self {
            Self::start(String::new(), sink, Config::default())
        }

        pub fn send(&self, _line: &str) -> Result<(), String> {
            Err("the Devices pipe is Windows only".to_owned())
        }

        pub fn stop(&mut self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_all(chunks: &[&[u8]]) -> (Result<(), Reason>, Vec<String>) {
        let mut buffer = LineBuffer::default();
        let mut out = Vec::new();
        for chunk in chunks {
            if let Err(reason) = buffer.push(chunk, &mut out) {
                return (Err(reason), out);
            }
        }
        (Ok(()), out)
    }

    #[test]
    fn lines_split_across_chunks_and_batched() {
        let (result, out) = push_all(&[b"{\"type\":\"a\"}\n{\"ty", b"pe\":\"b\"}", b"\n"]);
        assert_eq!(result, Ok(()));
        assert_eq!(out, ["{\"type\":\"a\"}", "{\"type\":\"b\"}"]);
    }

    #[test]
    fn lines_keep_their_exact_text() {
        let line = "{ \"type\" : \"list\",\"n\":\"\u{e4}\" }";
        let (result, out) = push_all(&[format!("{line}\n").as_bytes()]);
        assert_eq!(result, Ok(()));
        assert_eq!(out, [line]);
    }

    #[test]
    fn protocol_violations_are_rejected() {
        let long = format!("{{\"a\":\"{}\"}}\n", "x".repeat(MAX_LINE));
        let no_newline = vec![b'a'; MAX_LINE + 1];
        for bad in [
            long.as_bytes(),
            &no_newline,
            b"{\"a\":\"\xff\"}\n",
            b"[1]\n",
            b"\"s\"\n",
            b"{\n",
            b"\n",
        ] {
            assert_eq!(push_all(&[bad]).0, Err(Reason::Protocol));
        }
    }

    #[test]
    fn a_line_of_exactly_the_limit_is_accepted() {
        let filler = "x".repeat(MAX_LINE - 8);
        let line = format!("{{\"a\":\"{filler}\"}}");
        assert_eq!(line.len(), MAX_LINE);
        let (result, out) = push_all(&[line.as_bytes(), b"\n"]);
        assert_eq!(result, Ok(()));
        assert_eq!(out, [line]);
    }

    #[test]
    fn lines_before_a_violation_are_still_delivered() {
        let (result, out) = push_all(&[b"{\"type\":\"a\"}\n[1]\n"]);
        assert_eq!(result, Err(Reason::Protocol));
        assert_eq!(out, ["{\"type\":\"a\"}"]);
    }

    #[test]
    fn valid_commands_pass() {
        for line in [
            r#"{"type":"open"}"#,
            r#"{"type":"startPairing"}"#,
            r#"{"type":"cancelPairing"}"#,
            r#"{"type":"revoke","id":"abc"}"#,
            r#"{"type":"allow","id":"abc"}"#,
            r#"{"type":"deny","id":"abc"}"#,
        ] {
            assert_eq!(validate_command(line), Ok(()), "{line}");
        }
    }

    #[test]
    fn invalid_commands_fail() {
        let long = format!("{{\"type\":\"open\",\"x\":\"{}\"}}", "x".repeat(MAX_LINE));
        for line in [
            "",
            "open",
            "[1]",
            r#"{"type":"nope"}"#,
            r#"{"type":5}"#,
            r#"{"id":"a"}"#,
            r#"{"type":"revoke"}"#,
            r#"{"type":"allow","id":7}"#,
            r#"{"type":"deny","id":""}"#,
            "{\"type\":\"open\"}\n{\"type\":\"open\"}",
            "{\"type\":\"open\"}\n",
            "{\"type\":\"open\",\n\"x\":1}",
            long.as_str(),
        ] {
            assert!(validate_command(line).is_err(), "{line:?}");
        }
    }

    #[test]
    fn errors_never_contain_the_line() {
        let secret = "SECRET-PAIRING-CODE";
        let err = validate_command(&format!(r#"{{"type":"{secret}"}}"#)).unwrap_err();
        assert!(!err.contains(secret));
    }

    #[test]
    fn sid_comparison() {
        let a = [1u8, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
        let mut b = a;
        b[11] = 1;
        assert!(sids_equal(&a, &a));
        assert!(!sids_equal(&a, &b));
        assert!(!sids_equal(&a, &a[..11]));
        assert!(!sids_equal(&[], &[]));
    }

    #[test]
    fn backoff_doubles_to_the_cap() {
        let max = Duration::from_secs(5);
        let mut wait = Duration::from_secs(1);
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(wait.as_secs());
            wait = next_backoff(wait, max);
        }
        assert_eq!(seen, [1, 2, 4, 5, 5]);
    }

    #[test]
    fn synthetic_lines() {
        assert_eq!(connected_line(), r#"{"type":"connected"}"#);
        assert_eq!(
            disconnected_line(Reason::NotTrusted),
            r#"{"type":"disconnected","reason":"notTrusted"}"#
        );
    }
}
