//! Local duplex named-pipe transport for the BPIP privileged boundary.
//!
//! The per-session Agent (running as LocalSystem) creates one byte-mode server
//! instance per role, and the network-facing Transport connects as its client.
//! The protected DACL permits only LocalSystem and the exact
//! `NT SERVICE\BubblePlayTransport` service SID. `FILE_FLAG_FIRST_PIPE_INSTANCE` prevents silently
//! joining an attacker-owned pipe with the same fixed name. The transport is a
//! byte stream; BPIP owns framing and payload validation above this layer.

use std::ffi::c_void;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const CONTROL_PIPE_NAME: &str = r"\\.\pipe\BubblePlay.Control.v1";
pub const MEDIA_PIPE_NAME: &str = r"\\.\pipe\BubblePlay.Media.v1";
const CONTROL_MAX_PAYLOAD_BYTES: usize = 64 * 1024;
const MEDIA_MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const CONTROL_BUFFER_BYTES: u32 = 64 * 1024;
const MEDIA_BUFFER_BYTES: u32 = 1024 * 1024;
pub const TRANSPORT_SERVICE_SID: &str =
    "S-1-5-80-36460983-23004156-1113575813-106319344-2112134846";
const SERVER_SDDL: &str = concat!(
    "D:P(A;;GA;;;SY)(A;;GRGW;;;",
    "S-1-5-80-36460983-23004156-1113575813-106319344-2112134846",
    ")"
);
const MAX_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
static SESSION_CLIENT_PROCESS_ID: AtomicU32 = AtomicU32::new(0);
#[cfg(test)]
static TEST_PIPE_COUNTER: AtomicU32 = AtomicU32::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeRole {
    Control,
    Media,
}

impl PipeRole {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Control => CONTROL_PIPE_NAME,
            Self::Media => MEDIA_PIPE_NAME,
        }
    }

    pub const fn max_payload_bytes(self) -> usize {
        match self {
            Self::Control => CONTROL_MAX_PAYLOAD_BYTES,
            Self::Media => MEDIA_MAX_PAYLOAD_BYTES,
        }
    }

    pub fn validate_payload_len(self, size: usize) -> io::Result<()> {
        let max = self.max_payload_bytes();
        if size <= max {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{:?} pipe payload is {size} bytes; maximum is {max}", self),
            ))
        }
    }

    const fn buffer_bytes(self) -> u32 {
        match self {
            Self::Control => CONTROL_BUFFER_BYTES,
            Self::Media => MEDIA_BUFFER_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    timeout: Duration,
    interval: Duration,
}

impl RetryPolicy {
    pub fn new(timeout: Duration, interval: Duration) -> io::Result<Self> {
        if interval.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "named-pipe retry interval must be non-zero",
            ));
        }
        if timeout > MAX_CONNECT_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "named-pipe connect timeout must not exceed 60 seconds",
            ));
        }
        Ok(Self { timeout, interval })
    }

    pub const fn timeout(self) -> Duration {
        self.timeout
    }

    pub const fn interval(self) -> Duration {
        self.interval
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            interval: Duration::from_millis(100),
        }
    }
}

pub struct PipeServer {
    handle: Option<OwnedPipeHandle>,
    role: PipeRole,
}

impl PipeServer {
    /// Creates the unique Agent-side server instance with the LocalSystem /
    /// BubblePlayTransport-service-only DACL. The caller is expected to be the per-session
    /// LocalSystem Agent; this function does not elevate its caller.
    pub fn bind_system(role: PipeRole) -> io::Result<Self> {
        let descriptor = SecurityDescriptor::from_sddl(SERVER_SDDL)?;
        let attributes = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as u32,
            security_descriptor: descriptor.0,
            inherit_handle: 0,
        };
        let name = wide_null(role.name());
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                role.buffer_bytes(),
                role.buffer_bytes(),
                0,
                &attributes,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(last_os_error());
        }
        Ok(Self {
            handle: Some(OwnedPipeHandle(handle)),
            role,
        })
    }

    pub const fn role(&self) -> PipeRole {
        self.role
    }

    /// Waits for one authorized client and transfers ownership of the connected
    /// duplex handle. Bind a new server after the returned stream closes.
    pub fn accept(mut self) -> io::Result<PipeStream> {
        let handle = self.handle.as_ref().expect("pipe server handle").0;
        let connected = unsafe { ConnectNamedPipe(handle, ptr::null_mut()) };
        if connected == 0 {
            let error = last_error_code();
            if error != ERROR_PIPE_CONNECTED {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        let peer_process_id = validate_transport_client(handle)?;
        associate_session_peer(self.role, peer_process_id)?;
        Ok(PipeStream {
            handle: self.handle.take().expect("pipe server handle"),
            role: self.role,
            peer_process_id,
        })
    }
}

pub struct PipeStream {
    handle: OwnedPipeHandle,
    role: PipeRole,
    peer_process_id: u32,
}

impl PipeStream {
    pub fn connect(role: PipeRole) -> io::Result<Self> {
        Self::connect_with_retry(role, RetryPolicy::default())
    }

    pub fn connect_with_retry(role: PipeRole, policy: RetryPolicy) -> io::Result<Self> {
        let name = wide_null(role.name());
        let started = Instant::now();
        loop {
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                return Ok(Self {
                    handle: OwnedPipeHandle(handle),
                    role,
                    peer_process_id: unsafe { GetCurrentProcessId() },
                });
            }

            let open_error = last_error_code();
            if !is_retryable_connect_error(open_error) {
                return Err(io::Error::from_raw_os_error(open_error as i32));
            }
            let elapsed = started.elapsed();
            if elapsed >= policy.timeout {
                return Err(io::Error::from_raw_os_error(open_error as i32));
            }
            let remaining = policy.timeout - elapsed;
            let wait = policy.interval.min(remaining);
            let wait_ms = duration_millis_u32(wait);
            let available = unsafe { WaitNamedPipeW(name.as_ptr(), wait_ms) };
            if available == 0 {
                let wait_error = last_error_code();
                if !is_retryable_connect_error(wait_error) && wait_error != ERROR_SEM_TIMEOUT {
                    return Err(io::Error::from_raw_os_error(wait_error as i32));
                }
                // WaitNamedPipe returns immediately while the pipe path does not
                // exist; retain the configured backoff in that case.
                if matches!(wait_error, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
                    thread::sleep(wait.min(policy.timeout.saturating_sub(started.elapsed())));
                }
            }
        }
    }

    pub const fn role(&self) -> PipeRole {
        self.role
    }

    /// Process ID authenticated by the server from the connected pipe handle.
    /// On the client side this is the current process ID.
    pub const fn peer_process_id(&self) -> u32 {
        self.peer_process_id
    }

    /// Duplicates the underlying connection handle. Synchronous reads and
    /// writes must not run concurrently across the two handles: Windows keeps
    /// them on the same synchronous file object, so an idle read can serialize
    /// a write indefinitely.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            handle: duplicate_handle(self.handle.0)?,
            role: self.role,
            peer_process_id: self.peer_process_id,
        })
    }

    /// Returns the number of bytes that can be read without blocking.
    ///
    /// Callers that own a synchronous duplex connection can use this before
    /// `read_frame` so the same thread remains available for outbound writes.
    pub fn available_bytes(&self) -> io::Result<u32> {
        let mut available = 0u32;
        let succeeded = unsafe {
            PeekNamedPipe(
                self.handle.0,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut available,
                ptr::null_mut(),
            )
        };
        if succeeded != 0 {
            Ok(available)
        } else {
            Err(last_os_error())
        }
    }
}

#[cfg(test)]
pub(crate) fn connected_test_pair(role: PipeRole) -> io::Result<(PipeStream, PipeStream)> {
    let id = TEST_PIPE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        r"\\.\pipe\BubblePlay.Test.{}.{}",
        unsafe { GetCurrentProcessId() },
        id
    );
    let wide_name = wide_null(&name);
    let server_handle = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            role.buffer_bytes(),
            role.buffer_bytes(),
            0,
            ptr::null(),
        )
    };
    if server_handle == INVALID_HANDLE_VALUE {
        return Err(last_os_error());
    }
    let server = OwnedPipeHandle(server_handle);
    let accept = thread::spawn(move || {
        let connected = unsafe { ConnectNamedPipe(server.0, ptr::null_mut()) };
        if connected == 0 {
            let error = last_error_code();
            if error != ERROR_PIPE_CONNECTED {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        Ok(PipeStream {
            handle: server,
            role,
            peer_process_id: unsafe { GetCurrentProcessId() },
        })
    });
    let client_handle = unsafe {
        CreateFileW(
            wide_name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if client_handle == INVALID_HANDLE_VALUE {
        return Err(last_os_error());
    }
    let client = PipeStream {
        handle: OwnedPipeHandle(client_handle),
        role,
        peer_process_id: unsafe { GetCurrentProcessId() },
    };
    let server = accept
        .join()
        .map_err(|_| io::Error::other("test pipe accept thread panicked"))??;
    Ok((client, server))
}

impl Read for PipeStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let requested = buffer.len().min(u32::MAX as usize) as u32;
        let mut read = 0u32;
        let succeeded = unsafe {
            ReadFile(
                self.handle.0,
                buffer.as_mut_ptr().cast(),
                requested,
                &mut read,
                ptr::null_mut(),
            )
        };
        if succeeded != 0 {
            Ok(read as usize)
        } else {
            let error = last_error_code();
            if matches!(error, ERROR_BROKEN_PIPE | ERROR_NO_DATA) {
                Ok(0)
            } else {
                Err(io::Error::from_raw_os_error(error as i32))
            }
        }
    }
}

impl Write for PipeStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let requested = buffer.len().min(u32::MAX as usize) as u32;
        let mut written = 0u32;
        let succeeded = unsafe {
            WriteFile(
                self.handle.0,
                buffer.as_ptr().cast(),
                requested,
                &mut written,
                ptr::null_mut(),
            )
        };
        if succeeded != 0 {
            if written == 0 {
                Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "named pipe wrote zero bytes",
                ))
            } else {
                Ok(written as usize)
            }
        } else {
            Err(last_os_error())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if unsafe { FlushFileBuffers(self.handle.0) } != 0 {
            Ok(())
        } else {
            Err(last_os_error())
        }
    }
}

struct OwnedPipeHandle(RawHandle);

unsafe impl Send for OwnedPipeHandle {}

impl Drop for OwnedPipeHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn duplicate_handle(handle: RawHandle) -> io::Result<OwnedPipeHandle> {
    let process = unsafe { GetCurrentProcess() };
    let mut duplicate = ptr::null_mut();
    let succeeded = unsafe {
        DuplicateHandle(
            process,
            handle,
            process,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if succeeded == 0 {
        Err(last_os_error())
    } else if duplicate.is_null() || duplicate == INVALID_HANDLE_VALUE {
        Err(io::Error::other(
            "DuplicateHandle returned an invalid handle",
        ))
    } else {
        Ok(OwnedPipeHandle(duplicate))
    }
}

fn validate_transport_client(pipe: RawHandle) -> io::Result<u32> {
    let mut process_id = 0u32;
    if unsafe { GetNamedPipeClientProcessId(pipe, &mut process_id) } == 0 || process_id == 0 {
        return Err(last_os_error());
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return Err(last_os_error());
    }
    let process = OwnedPipeHandle(process);
    validate_transport_image(process.0, process_id)?;

    let mut primary_token = ptr::null_mut();
    if unsafe {
        OpenProcessToken(
            process.0,
            transport_token_source_access(),
            &mut primary_token,
        )
    } == 0
    {
        return Err(last_os_error());
    }
    let primary_token = OwnedPipeHandle(primary_token);
    let mut impersonation_token = ptr::null_mut();
    if unsafe {
        DuplicateTokenEx(
            primary_token.0,
            TOKEN_QUERY,
            ptr::null(),
            SECURITY_IDENTIFICATION,
            TOKEN_IMPERSONATION,
            &mut impersonation_token,
        )
    } == 0
    {
        return Err(last_os_error());
    }
    let impersonation_token = OwnedPipeHandle(impersonation_token);

    let sid_text = wide_null(TRANSPORT_SERVICE_SID);
    let mut sid = ptr::null_mut();
    if unsafe { ConvertStringSidToSidW(sid_text.as_ptr(), &mut sid) } == 0 || sid.is_null() {
        return Err(last_os_error());
    }
    let sid = LocalAllocation(sid);
    let mut member = 0i32;
    if unsafe { CheckTokenMembership(impersonation_token.0, sid.0, &mut member) } == 0 {
        return Err(last_os_error());
    }
    if member == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "named-pipe client process {process_id} does not carry the BubblePlayTransport service SID"
            ),
        ));
    }
    Ok(process_id)
}

const fn transport_token_source_access() -> u32 {
    TOKEN_QUERY | TOKEN_DUPLICATE
}

fn validate_transport_image(process: RawHandle, process_id: u32) -> io::Result<()> {
    let mut image = vec![0u16; 32_768];
    let mut length = image.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process, 0, image.as_mut_ptr(), &mut length) } == 0 {
        return Err(last_os_error());
    }
    image.truncate(length as usize);
    let actual = PathBuf::from(OsString::from_wide(&image));
    let expected = std::env::current_exe()?
        .parent()
        .ok_or_else(|| io::Error::other("BubblePlay Agent executable has no parent directory"))?
        .join("bubbleplay-host.exe");
    if normalize_windows_path(&actual) != normalize_windows_path(&expected) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "named-pipe client process {process_id} image '{}' is not the installed BubblePlay transport host",
                actual.display()
            ),
        ));
    }
    Ok(())
}

fn normalize_windows_path(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('/', r"\")
        .to_ascii_lowercase()
}

fn associate_session_peer(role: PipeRole, process_id: u32) -> io::Result<()> {
    match role {
        PipeRole::Control => {
            SESSION_CLIENT_PROCESS_ID.store(process_id, Ordering::Release);
            Ok(())
        }
        PipeRole::Media => {
            let control_process_id = SESSION_CLIENT_PROCESS_ID.load(Ordering::Acquire);
            if control_process_id != 0 && control_process_id == process_id {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "media pipe process {process_id} does not match authenticated control process {control_process_id}"
                    ),
                ))
            }
        }
    }
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

struct SecurityDescriptor(*mut c_void);

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let sddl = wide_null(sddl);
        let mut descriptor = ptr::null_mut();
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if converted == 0 {
            Err(last_os_error())
        } else {
            Ok(Self(descriptor))
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    security_descriptor: *mut c_void,
    inherit_handle: i32,
}

type RawHandle = *mut c_void;

const INVALID_HANDLE_VALUE: RawHandle = -1isize as RawHandle;
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_TYPE_BYTE: u32 = 0;
const PIPE_READMODE_BYTE: u32 = 0;
const PIPE_WAIT: u32 = 0;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
const DUPLICATE_SAME_ACCESS: u32 = 0x0000_0002;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x0000_1000;
const TOKEN_DUPLICATE: u32 = 0x0000_0002;
const TOKEN_QUERY: u32 = 0x0000_0008;
const SECURITY_IDENTIFICATION: i32 = 1;
const TOKEN_IMPERSONATION: i32 = 2;
const SDDL_REVISION_1: u32 = 1;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_BROKEN_PIPE: u32 = 109;
const ERROR_SEM_TIMEOUT: u32 = 121;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_NO_DATA: u32 = 232;
const ERROR_PIPE_CONNECTED: u32 = 535;

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn duration_millis_u32(duration: Duration) -> u32 {
    duration.as_millis().clamp(1, u32::MAX as u128) as u32
}

fn is_retryable_connect_error(error: u32) -> bool {
    matches!(
        error,
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_PIPE_BUSY | ERROR_SEM_TIMEOUT
    )
}

fn last_error_code() -> u32 {
    unsafe { GetLastError() }
}

fn last_os_error() -> io::Error {
    io::Error::from_raw_os_error(last_error_code() as i32)
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        max_instances: u32,
        out_buffer_size: u32,
        in_buffer_size: u32,
        default_timeout_ms: u32,
        security_attributes: *const SecurityAttributes,
    ) -> RawHandle;
    fn ConnectNamedPipe(pipe: RawHandle, overlapped: *mut c_void) -> i32;
    fn WaitNamedPipeW(name: *const u16, timeout_ms: u32) -> i32;
    fn CreateFileW(
        name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *const SecurityAttributes,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: RawHandle,
    ) -> RawHandle;
    fn ReadFile(
        file: RawHandle,
        buffer: *mut c_void,
        bytes_to_read: u32,
        bytes_read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn WriteFile(
        file: RawHandle,
        buffer: *const c_void,
        bytes_to_write: u32,
        bytes_written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn PeekNamedPipe(
        pipe: RawHandle,
        buffer: *mut c_void,
        buffer_size: u32,
        bytes_read: *mut u32,
        total_bytes_available: *mut u32,
        bytes_left_this_message: *mut u32,
    ) -> i32;
    fn FlushFileBuffers(file: RawHandle) -> i32;
    fn CloseHandle(object: RawHandle) -> i32;
    fn GetCurrentProcess() -> RawHandle;
    fn DuplicateHandle(
        source_process: RawHandle,
        source_handle: RawHandle,
        target_process: RawHandle,
        target_handle: *mut RawHandle,
        desired_access: u32,
        inherit_handle: i32,
        options: u32,
    ) -> i32;
    fn GetCurrentProcessId() -> u32;
    fn GetNamedPipeClientProcessId(pipe: RawHandle, client_process_id: *mut u32) -> i32;
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> RawHandle;
    fn QueryFullProcessImageNameW(
        process: RawHandle,
        flags: u32,
        image_name: *mut u16,
        size: *mut u32,
    ) -> i32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
    fn GetLastError() -> u32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string_security_descriptor: *const u16,
        string_sd_revision: u32,
        security_descriptor: *mut *mut c_void,
        security_descriptor_size: *mut u32,
    ) -> i32;
    fn ConvertStringSidToSidW(string_sid: *const u16, sid: *mut *mut c_void) -> i32;
    fn OpenProcessToken(
        process_handle: RawHandle,
        desired_access: u32,
        token_handle: *mut RawHandle,
    ) -> i32;
    fn DuplicateTokenEx(
        existing_token: RawHandle,
        desired_access: u32,
        token_attributes: *const SecurityAttributes,
        impersonation_level: i32,
        token_type: i32,
        new_token: *mut RawHandle,
    ) -> i32;
    fn CheckTokenMembership(token_handle: RawHandle, sid: *mut c_void, is_member: *mut i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_have_fixed_versioned_names_and_limits() {
        assert_eq!(PipeRole::Control.name(), r"\\.\pipe\BubblePlay.Control.v1");
        assert_eq!(PipeRole::Media.name(), r"\\.\pipe\BubblePlay.Media.v1");
        assert_ne!(PipeRole::Control.name(), PipeRole::Media.name());

        assert_eq!(PipeRole::Control.max_payload_bytes(), 64 * 1024);
        assert_eq!(PipeRole::Media.max_payload_bytes(), 8 * 1024 * 1024);
        assert!(PipeRole::Control.validate_payload_len(64 * 1024).is_ok());
        assert!(PipeRole::Control
            .validate_payload_len(64 * 1024 + 1)
            .is_err());
        assert!(PipeRole::Media
            .validate_payload_len(8 * 1024 * 1024)
            .is_ok());
        assert!(PipeRole::Media
            .validate_payload_len(8 * 1024 * 1024 + 1)
            .is_err());
    }

    #[test]
    fn server_acl_is_protected_and_limited_to_the_transport_service_sid() {
        assert_eq!(
            SERVER_SDDL,
            concat!(
                "D:P(A;;GA;;;SY)(A;;GRGW;;;",
                "S-1-5-80-36460983-23004156-1113575813-106319344-2112134846",
                ")"
            )
        );
        assert!(SERVER_SDDL.contains(TRANSPORT_SERVICE_SID));
        assert!(!SERVER_SDDL.contains(";;;LS"));
        assert!(!SERVER_SDDL.contains("WD"));
        assert!(!SERVER_SDDL.contains("AU"));
        assert!(!SERVER_SDDL.contains("BA"));
        let descriptor = SecurityDescriptor::from_sddl(SERVER_SDDL).unwrap();
        assert!(!descriptor.0.is_null());
    }

    #[test]
    fn transport_token_source_access_is_query_plus_duplicate_only() {
        assert_eq!(
            transport_token_source_access(),
            TOKEN_QUERY | TOKEN_DUPLICATE
        );
        assert_ne!(transport_token_source_access() & TOKEN_QUERY, 0);
        assert_ne!(transport_token_source_access() & TOKEN_DUPLICATE, 0);
    }

    #[test]
    fn retry_policy_is_bounded_and_rejects_zero_interval() {
        assert!(RetryPolicy::new(
            std::time::Duration::from_secs(2),
            std::time::Duration::from_millis(100)
        )
        .is_ok());
        assert!(
            RetryPolicy::new(std::time::Duration::from_secs(2), std::time::Duration::ZERO).is_err()
        );
        assert!(RetryPolicy::new(
            std::time::Duration::from_secs(61),
            std::time::Duration::from_millis(100)
        )
        .is_err());
    }

    #[test]
    fn media_lane_must_share_the_authenticated_control_process() {
        associate_session_peer(PipeRole::Control, 41).unwrap();
        associate_session_peer(PipeRole::Media, 41).unwrap();
        assert!(associate_session_peer(PipeRole::Media, 42).is_err());
    }

    #[test]
    fn image_path_comparison_is_case_and_extended_prefix_insensitive() {
        assert_eq!(
            normalize_windows_path(std::path::Path::new(
                r"\\?\C:\Program Files\BubblePlay\bubbleplay-host.exe"
            )),
            normalize_windows_path(std::path::Path::new(
                r"c:\program files\bubbleplay\BUBBLEPLAY-HOST.EXE"
            ))
        );
    }

    #[test]
    fn current_process_handle_can_be_duplicated_without_elevation() {
        let _: fn(&PipeStream) -> io::Result<PipeStream> = PipeStream::try_clone;
        let pseudo_handle = unsafe { GetCurrentProcess() };
        let duplicate = duplicate_handle(pseudo_handle).unwrap();
        assert!(!duplicate.0.is_null());
        assert_ne!(duplicate.0, INVALID_HANDLE_VALUE);
    }

    #[test]
    fn available_bytes_distinguishes_idle_and_readable_pipe_states() {
        let (mut client, mut server) = connected_test_pair(PipeRole::Control).unwrap();
        assert_eq!(client.available_bytes().unwrap(), 0);

        server.write_all(b"abc").unwrap();
        assert_eq!(client.available_bytes().unwrap(), 3);

        let mut received = [0u8; 3];
        client.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"abc");
        assert_eq!(client.available_bytes().unwrap(), 0);
    }
}
