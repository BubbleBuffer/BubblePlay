//! BubblePlay admin input agent ("Admin mode").
//!
//! A deliberately small elevated process: a local named-pipe server that
//! validates `INPUT` records from the user-level host and passes them to
//! `SendInput`, so remote input reaches elevated windows. It has no network,
//! capture, process-launching or file-writing code beyond its own log.
//!
//! Started on demand by the tray through the `\BubblePlay\AdminAgent`
//! scheduled task (RunLevel Highest). It exits when the tray signals its stop
//! event (Admin mode off, tray quitting) or the tray disappears.
//!
//! Elevated, it refuses to run from outside `%ProgramFiles%` and ignores dev
//! options. `--dev` (tests only) is accepted only when NOT elevated and
//! requires `BUBBLEPLAY_TRAY_DATA_DIR`, which scopes every object name.
#![windows_subsystem = "windows"]

#[allow(dead_code)]
#[path = "../tray/logic.rs"]
mod logic;
#[allow(dead_code)]
#[path = "../admin_agent/protocol.rs"]
mod protocol;
#[allow(dead_code)]
#[path = "../admin_agent/session.rs"]
mod session;
#[allow(dead_code)]
#[path = "../admin_agent/trust.rs"]
mod trust;

use std::ffi::OsString;
use std::fs::File;
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ALREADY_EXISTS, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::IO::CancelIoEx;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, GetCurrentProcessId, OpenEventW, OpenMutexW,
    OpenProcessToken, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};

use logic::SharedLog;
use protocol::{read_frame, scoped_name, write_agent, Record, AGENT_MUTEX, AGENT_STOP_EVENT, TRAY_MUTEX};
use session::{Injector, Outcome, Session};

/// Pipe instances: one listening plus up to three clients (host, probe, spare).
const MAX_INSTANCES: u32 = 4;
const HELLO_TIMEOUT: Duration = Duration::from_secs(3);
const TRAY_MISSING_LIMIT: u32 = 3;
const RATE_LOG_INTERVAL: Duration = Duration::from_secs(5);

struct Handle(HANDLE);

// SAFETY: kernel handles may be used from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: owned handle, closed exactly once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

fn wide(text: impl Into<OsString>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.into().encode_wide().chain(Some(0)).collect()
}

struct Options {
    dev: bool,
    dev_range: Option<(u16, u16)>,
}

fn parse_options() -> Option<Options> {
    let mut options = Options {
        dev: false,
        dev_range: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dev" => options.dev = true,
            // Emulate an agent speaking other protocol versions (tests).
            "--dev-protocol-range" => {
                let value = args.next()?;
                let (min, max) = value.split_once('-')?;
                options.dev_range = Some((min.parse().ok()?, max.parse().ok()?));
            }
            _ => return None,
        }
    }
    Some(options)
}

/// Security descriptor for the pipe and the liveness mutex: SYSTEM and
/// Administrators full control, the agent's own (interactive) user limited
/// access, medium integrity label so low-integrity (sandboxed) processes of
/// the user cannot connect.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is immutable after creation and only read by
// CreateNamedPipeW; it is freed once, on drop.
unsafe impl Send for SecurityDescriptor {}
unsafe impl Sync for SecurityDescriptor {}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.0 .0))) };
    }
}

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> anyhow::Result<Self> {
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
        }?;
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

fn current_user_sid() -> anyhow::Result<String> {
    let mut token = HANDLE::default();
    // SAFETY: out-pointer valid; handle closed by Handle.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }?;
    let token = Handle(token);
    let mut buffer = vec![0u8; 256];
    let mut returned = 0u32;
    // SAFETY: buffer is writable for its length.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            buffer.len() as u32,
            &mut returned,
        )
    }?;
    // SAFETY: GetTokenInformation filled a TOKEN_USER at the buffer start.
    let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    let mut text = PWSTR::null();
    // SAFETY: valid SID; text freed with LocalFree.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }?;
    // SAFETY: NUL-terminated string from the API.
    let sid = unsafe { text.to_string() }?;
    // SAFETY: allocated by ConvertSidToStringSidW.
    let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
    if !sid.starts_with("S-1-5-21-") && !sid.starts_with("S-1-12-1-") {
        anyhow::bail!("unexpected user SID {sid}");
    }
    Ok(sid)
}

fn named_object_exists(name: &str, event: bool) -> bool {
    let name = wide(name);
    // SAFETY: NUL-terminated name; handle closed on drop.
    let opened = unsafe {
        if event {
            OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr()))
        } else {
            OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr()))
        }
    };
    match opened {
        Ok(handle) => {
            drop(Handle(handle));
            true
        }
        // Access denied still proves the object exists.
        Err(error) => error.code() == windows::Win32::Foundation::E_ACCESSDENIED,
    }
}

fn open_event(name: &str) -> Option<Handle> {
    let name = wide(name);
    // SAFETY: NUL-terminated name.
    unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) }
        .ok()
        .map(Handle)
}

fn is_signaled(handle: &Handle) -> bool {
    // SAFETY: valid waitable handle; zero timeout.
    (unsafe { WaitForSingleObject(handle.0, 0) }) == WAIT_OBJECT_0
}

/// `SendInput` for validated protocol records.
struct SendInputInjector;

impl Injector for SendInputInjector {
    fn inject(&mut self, records: &[Record]) -> (u32, i32) {
        let inputs: Vec<INPUT> = records
            .iter()
            .map(|record| match *record {
                Record::Mouse {
                    dx,
                    dy,
                    data,
                    flags,
                } => INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx,
                            dy,
                            mouseData: data,
                            dwFlags: MOUSE_EVENT_FLAGS(flags),
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                },
                Record::Key { vk, scan, flags } => INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(vk),
                            wScan: scan,
                            dwFlags: KEYBD_EVENT_FLAGS(flags),
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                },
            })
            .collect();
        if inputs.is_empty() {
            return (0, 0);
        }
        // SAFETY: inputs is a valid slice of fully initialized INPUTs.
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        let error = if sent as usize == inputs.len() {
            0
        } else {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        };
        (sent, error)
    }
}

type SharedSession = Arc<Mutex<Session<SendInputInjector>>>;

struct Agent {
    pipe: String,
    security: SecurityDescriptor,
    elevated: bool,
    range: (u16, u16),
    sessions: Mutex<Vec<SharedSession>>,
    active: AtomicUsize,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Agent {
    fn create_instance(&self, first: bool) -> std::io::Result<Handle> {
        let name = wide(self.pipe.as_str());
        let attributes = self.security.attributes();
        let mut open_mode = PIPE_ACCESS_DUPLEX;
        if first {
            // Fail instead of joining a pipe someone else already created.
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        // SAFETY: NUL-terminated name; attributes outlive the call.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                MAX_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                Some(&attributes),
            )
        };
        if handle.is_invalid() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Handle(handle))
    }

    /// Create a further instance, retrying until it exists: a listening
    /// instance must always be present, or clients see "file not found".
    fn next_instance(&self) -> Handle {
        loop {
            match self.create_instance(false) {
                Ok(instance) => return instance,
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    tracing::warn!(%error, "creating pipe instance failed");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }

    /// Accept clients forever (the main thread ends the process). The next
    /// instance is created before the connected one is handed off or
    /// dropped, so the pipe never vanishes between clients.
    fn serve(self: Arc<Self>) {
        let mut listening = match self.create_instance(true) {
            Ok(instance) => instance,
            Err(error) => {
                tracing::error!(%error, pipe = %self.pipe, "cannot create the agent pipe (name taken?); exiting");
                std::process::exit(1);
            }
        };
        tracing::info!(pipe = %self.pipe, "listening");
        loop {
            // SAFETY: valid pipe handle; synchronous connect.
            let connected = unsafe { ConnectNamedPipe(listening.0, None) };
            let instance = std::mem::replace(&mut listening, self.next_instance());
            if let Err(error) = connected {
                if error.code() != ERROR_PIPE_CONNECTED.to_hresult() {
                    tracing::warn!(%error, "client connect failed");
                    continue;
                }
            }
            let mut client_pid = 0u32;
            // SAFETY: connected pipe handle and out-pointer.
            let _ = unsafe { GetNamedPipeClientProcessId(instance.0, &mut client_pid) };
            let image = trust::process_image_path(client_pid);
            let client_session = trust::process_session_id(client_pid);
            if client_session != Some(trust::current_session_id()) {
                tracing::warn!(client_pid, ?client_session, "rejecting client from another session");
                // SAFETY: connected pipe handle.
                let _ = unsafe { DisconnectNamedPipe(instance.0) };
                continue;
            }
            if self.active.load(Ordering::SeqCst) >= (MAX_INSTANCES - 1) as usize {
                tracing::warn!(client_pid, "too many clients; rejecting");
                // SAFETY: connected pipe handle.
                let _ = unsafe { DisconnectNamedPipe(instance.0) };
                continue;
            }
            tracing::info!(
                client_pid,
                image = %image.as_deref().map(|p| p.display().to_string()).unwrap_or_default(),
                "client connected"
            );
            self.active.fetch_add(1, Ordering::SeqCst);
            let agent = self.clone();
            let spawned = std::thread::Builder::new()
                .name("agent-client".into())
                .spawn(move || {
                    agent.handle_client(instance, client_pid);
                    agent.active.fetch_sub(1, Ordering::SeqCst);
                });
            if spawned.is_err() {
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    fn handle_client(&self, instance: Handle, client_pid: u32) {
        let raw = instance.0;
        std::mem::forget(instance);
        // SAFETY: we own the pipe handle; File closes it.
        let file = unsafe { File::from_raw_handle(raw.0 as _) };
        let greeted = Arc::new(AtomicBool::new(false));
        // Set (under the lock) before the handle is closed, so the watchdog
        // never touches a closed or reused handle.
        let closed = Arc::new(Mutex::new(false));
        {
            // Drop clients that never say hello. Synchronous I/O on one pipe
            // handle is serialized, so cancel the pending read instead of
            // disconnecting underneath it.
            let greeted = greeted.clone();
            let closed = closed.clone();
            let raw_value = raw.0 as usize;
            let _ = std::thread::Builder::new()
                .name("agent-hello-timeout".into())
                .spawn(move || {
                    std::thread::sleep(HELLO_TIMEOUT);
                    let closed = lock(&closed);
                    if !*closed && !greeted.load(Ordering::SeqCst) {
                        tracing::warn!(client_pid, "no hello in time; disconnecting");
                        // SAFETY: `closed` is false and held, so the handle is open.
                        let _ = unsafe { CancelIoEx(HANDLE(raw_value as _), None) };
                    }
                });
        }
        let session: SharedSession = Arc::new(Mutex::new(Session::with_range(
            SendInputInjector,
            self.elevated,
            // SAFETY: trivial query.
            unsafe { GetCurrentProcessId() },
            Instant::now(),
            self.range,
        )));
        lock(&self.sessions).push(session.clone());
        let mut reader: &File = &file;
        let mut writer: &File = &file;
        let mut last_rate_log: Option<Instant> = None;
        let mut rate_limited_seen = 0u64;
        let reason = loop {
            let (kind, payload) = match read_frame(&mut reader) {
                Ok(frame) => frame,
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                    break "client closed".to_string()
                }
                Err(error) => break format!("read: {error}"),
            };
            let outcome = {
                let mut session = lock(&session);
                let outcome = session.on_frame(kind, &payload, Instant::now());
                if !greeted.load(Ordering::SeqCst) {
                    if let Some((version, role)) = session.negotiated() {
                        greeted.store(true, Ordering::SeqCst);
                        tracing::info!(client_pid, version, ?role, "hello accepted");
                    }
                }
                let limited = session.stats.rate_limited;
                if limited > rate_limited_seen
                    && last_rate_log.is_none_or(|at| at.elapsed() >= RATE_LOG_INTERVAL)
                {
                    tracing::warn!(client_pid, total = limited, "client rate limited");
                    last_rate_log = Some(Instant::now());
                    rate_limited_seen = limited;
                }
                outcome
            };
            match outcome {
                Outcome::Reply(message) => {
                    if let Err(error) = write_agent(&mut writer, &message) {
                        break format!("write: {error}");
                    }
                }
                Outcome::ReplyAndClose(message) => {
                    let _ = write_agent(&mut writer, &message);
                    break format!("refused: {message:?}");
                }
            }
        };
        greeted.store(true, Ordering::SeqCst);
        let (released, stats) = {
            let mut session = lock(&session);
            (session.release_held(), session.stats)
        };
        lock(&self.sessions).retain(|other| !Arc::ptr_eq(other, &session));
        {
            let mut closed = lock(&closed);
            *closed = true;
            // Closing (not disconnecting) lets the client still read a final
            // refusal, then EOF.
            drop(file);
        }
        tracing::info!(
            client_pid,
            %reason,
            frames = stats.frames,
            records = stats.records,
            applied = stats.applied,
            rate_limited = stats.rate_limited,
            released,
            "client disconnected"
        );
    }

    fn release_everything(&self) {
        for session in lock(&self.sessions).iter() {
            lock(session).release_held();
        }
    }
}

fn init_log(path: PathBuf) {
    let log = SharedLog::new(path);
    let _ = tracing_subscriber::fmt()
        .with_writer(move || log.clone())
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .try_init();
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let Ok(exe) = std::env::current_exe() else {
        return 1;
    };
    let options = parse_options();
    let elevated = trust::is_process_elevated();
    let test_dir = std::env::var_os("BUBBLEPLAY_TRAY_DATA_DIR");
    let scope = if elevated {
        // Elevated: fixed names and a log in the admin-only install folder;
        // environment overrides are ignored.
        let Some(dir) = exe.parent() else {
            return 1;
        };
        init_log(dir.join("logs").join("admin-agent.log"));
        let Some(options) = options.as_ref() else {
            tracing::error!("unknown arguments");
            return 2;
        };
        if options.dev || options.dev_range.is_some() {
            tracing::error!("dev options are refused while elevated");
            return 1;
        }
        if !trust::is_under_program_files(&exe) {
            tracing::error!(exe = %exe.display(), "refusing to run elevated from a non-Program Files location");
            return 1;
        }
        None
    } else {
        let (Some(options), Some(dir)) = (options.as_ref(), test_dir.as_ref()) else {
            // Not elevated and not an isolated dev instance: nothing to do.
            return 1;
        };
        init_log(PathBuf::from(dir).join("logs").join("admin-agent.log"));
        if !options.dev {
            tracing::error!("not elevated; only --dev instances may run unelevated");
            return 1;
        }
        Some(dir.clone())
    };
    let range = options
        .as_ref()
        .and_then(|options| options.dev_range)
        .unwrap_or((protocol::PROTOCOL_MIN, protocol::PROTOCOL_MAX));
    let scoped = |base: &str| scoped_name(base, scope.as_deref());

    let sid = match current_user_sid() {
        Ok(sid) => sid,
        Err(error) => {
            tracing::error!(%error, "cannot resolve the user SID");
            return 1;
        }
    };
    let security = match SecurityDescriptor::from_sddl(&protocol::pipe_sddl(&sid, elevated)) {
        Ok(security) => security,
        Err(error) => {
            tracing::error!(%error, "building the pipe security descriptor failed");
            return 1;
        }
    };
    let mutex_security = match SecurityDescriptor::from_sddl(&format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00100000;;;{sid})S:(ML;;NW;;;ME)"
    )) {
        Ok(security) => security,
        Err(error) => {
            tracing::error!(%error, "building the mutex security descriptor failed");
            return 1;
        }
    };
    let mutex_name = wide(scoped(AGENT_MUTEX));
    let attributes = mutex_security.attributes();
    // SAFETY: NUL-terminated name; attributes outlive the call.
    let instance = match unsafe { CreateMutexW(Some(&attributes), true, PCWSTR(mutex_name.as_ptr())) }
    {
        Ok(handle) => Handle(handle),
        Err(error) => {
            tracing::error!(%error, "cannot create the agent mutex");
            return 1;
        }
    };
    // SAFETY: reads the error left by CreateMutexW.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        tracing::info!("agent already running; exiting");
        return 0;
    }
    let Some(stop_event) = open_event(&scoped(AGENT_STOP_EVENT)) else {
        tracing::warn!("tray is not running; exiting");
        return 0;
    };
    // SAFETY: trivial query.
    let pid = unsafe { GetCurrentProcessId() };
    let agent = Arc::new(Agent {
        pipe: protocol::pipe_name(trust::current_session_id(), scope.as_deref()),
        security,
        elevated,
        range,
        sessions: Mutex::new(Vec::new()),
        active: AtomicUsize::new(0),
    });
    tracing::info!(
        pid,
        elevated,
        dev = !elevated,
        protocol_min = range.0,
        protocol_max = range.1,
        "admin input agent started"
    );
    {
        let agent = agent.clone();
        if std::thread::Builder::new()
            .name("agent-accept".into())
            .spawn(move || agent.serve())
            .is_err()
        {
            tracing::error!("cannot start the accept thread");
            return 1;
        }
    }
    let tray_mutex = scoped(TRAY_MUTEX);
    let mut tray_missing = 0u32;
    let reason = loop {
        if is_signaled(&stop_event) {
            break "tray requested stop";
        }
        if named_object_exists(&tray_mutex, false) {
            tray_missing = 0;
        } else {
            tray_missing += 1;
            if tray_missing >= TRAY_MISSING_LIMIT {
                break "tray disappeared";
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    agent.release_everything();
    tracing::info!(reason, "admin input agent exiting");
    drop(instance);
    // Client threads may be blocked in pipe reads; end the process.
    std::process::exit(0);
}
