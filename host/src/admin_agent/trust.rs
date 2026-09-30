//! Process trust checks shared by the admin agent, the host's agent client
//! and the tray: elevation, Program Files location and session identity.

use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by us and is closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Whether this process runs with an elevated (full administrator) token.
pub fn is_process_elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: out-pointer is valid; the token is closed by OwnedHandle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    let token = OwnedHandle(token);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: buffer is a TOKEN_ELEVATION of the stated size.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    }
    .is_ok();
    ok && elevation.TokenIsElevated != 0
}

/// `%ProgramFiles%` (the native one on 64-bit Windows).
pub fn program_files() -> Option<PathBuf> {
    std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(PathBuf::from)
}

/// True when `path` lies below `%ProgramFiles%` (admin-only by default).
pub fn is_under_program_files(path: &Path) -> bool {
    let Some(root) = program_files() else {
        return false;
    };
    let (Ok(path), Ok(root)) = (std::fs::canonicalize(path), std::fs::canonicalize(root)) else {
        return false;
    };
    path.starts_with(root)
}

/// Full image path of another process, if it can be queried.
pub fn process_image_path(pid: u32) -> Option<PathBuf> {
    // SAFETY: plain query-only open; the handle is closed by OwnedHandle.
    let process =
        OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let mut buffer = vec![0u16; 32 * 1024];
    let mut len = buffer.len() as u32;
    // SAFETY: buffer/len describe writable memory.
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    Some(PathBuf::from(std::ffi::OsString::from_wide(
        &buffer[..len as usize],
    )))
}

pub fn current_session_id() -> u32 {
    let mut session = 0u32;
    // SAFETY: out-pointer is valid.
    let _ = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    session
}

pub fn process_session_id(pid: u32) -> Option<u32> {
    let mut session = 0u32;
    // SAFETY: out-pointer is valid.
    unsafe { ProcessIdToSessionId(pid, &mut session) }
        .ok()
        .map(|()| session)
}
