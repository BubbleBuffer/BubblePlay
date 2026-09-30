//! Win32 helpers for the tray: DPAPI, kill-on-close job, HKCU Run key,
//! named kernel objects and the `\BubblePlay\ElevatedHost` scheduled task.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, HANDLE,
    HLOCAL, WAIT_OBJECT_0,
};
use windows::Win32::Security::Cryptography::{
    CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, OpenMutexW, ResetEvent, SetEvent, WaitForSingleObject,
    SYNCHRONIZATION_SYNCHRONIZE,
};

use super::logic::decode_hex;

pub const TRAY_MUTEX: &str = r"Local\BubblePlayTray.SingleInstance";
/// Manual-reset event owned by the tray; signaled to ask the elevated runner
/// to stop. Its absence also tells the runner that the tray is gone.
pub const ELEVATED_STOP_EVENT: &str = r"Local\BubblePlayTray.ElevatedStop";
pub const ELEVATED_TASK: &str = r"\BubblePlay\ElevatedHost";
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const RUN_VALUE: &str = "BubblePlayTray";

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

/// Owned kernel handle.
#[derive(Debug)]
pub struct Handle(pub HANDLE);

// SAFETY: kernel handles may be used from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: we own the handle and close it exactly once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// Decrypt a hex DPAPI blob (CurrentUser or LocalMachine scope).
pub fn dpapi_unprotect_hex(value: &str) -> Result<Vec<u8>> {
    let mut encrypted = decode_hex(value)?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: encrypted.len().try_into().context("DPAPI blob too large")?,
        pbData: encrypted.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input points at live memory; output is freed with LocalFree below.
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
    .context("DPAPI could not decrypt the stored secret (was it protected for another user?)")?;
    let plain = if output.pbData.is_null() {
        Vec::new()
    } else {
        // SAFETY: DPAPI returned cbData bytes at pbData.
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec()
    };
    if !output.pbData.is_null() {
        // SAFETY: allocated by CryptUnprotectData.
        let _ = unsafe { LocalFree(Some(HLOCAL(output.pbData.cast()))) };
    }
    encrypted.fill(0);
    Ok(plain)
}

/// Job object that kills every assigned process when the last handle closes,
/// including when the owning process dies unexpectedly.
#[derive(Debug)]
pub struct KillOnCloseJob(Handle);

impl KillOnCloseJob {
    pub fn new() -> Result<Self> {
        // SAFETY: plain creation of an anonymous job object.
        let handle =
            unsafe { CreateJobObjectW(None, PCWSTR::null()) }.context("CreateJobObjectW")?;
        let job = Self(Handle(handle));
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: limits is a valid, correctly sized structure.
        unsafe {
            SetInformationJobObject(
                job.0 .0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .context("configuring kill-on-close job")?;
        Ok(job)
    }

    pub fn assign(&self, process: HANDLE) -> Result<()> {
        // SAFETY: both handles are valid for the duration of the call.
        unsafe { AssignProcessToJobObject(self.0 .0, process) }.context("AssignProcessToJobObject")
    }
}

/// Named mutex held for the process lifetime. `None` means another instance owns it.
pub fn acquire_single_instance(name: &str) -> Result<Option<Handle>> {
    let name = wide(name);
    // SAFETY: name is NUL-terminated.
    let handle =
        unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) }.context("CreateMutexW")?;
    // SAFETY: reads the thread's last error set by CreateMutexW.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        drop(Handle(handle));
        return Ok(None);
    }
    Ok(Some(Handle(handle)))
}

pub fn named_mutex_exists(name: &str) -> bool {
    let name = wide(name);
    // SAFETY: name is NUL-terminated; the handle is closed on drop.
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) } {
        Ok(handle) => {
            drop(Handle(handle));
            true
        }
        Err(_) => false,
    }
}

pub fn create_manual_event(name: &str) -> Result<Handle> {
    let name = wide(name);
    // SAFETY: name is NUL-terminated.
    let handle = unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }
        .context("CreateEventW")?;
    Ok(Handle(handle))
}

pub fn set_event(event: &Handle, signaled: bool) {
    // SAFETY: valid event handle.
    let _ = unsafe {
        if signaled {
            SetEvent(event.0)
        } else {
            ResetEvent(event.0)
        }
    };
}

pub fn open_event(name: &str) -> Option<Handle> {
    let name = wide(name);
    // SAFETY: name is NUL-terminated.
    unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) }
        .ok()
        .map(Handle)
}

pub fn is_signaled(handle: &Handle) -> bool {
    // SAFETY: valid waitable handle; zero timeout.
    (unsafe { WaitForSingleObject(handle.0, 0) }) == WAIT_OBJECT_0
}

fn open_run_key(access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Result<HKEY> {
    let path = wide(RUN_KEY);
    let mut key = HKEY::default();
    // SAFETY: out-pointer is valid; key closed by caller.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            access,
            None,
            &mut key,
            None,
        )
    };
    status.ok().context("opening HKCU Run key")?;
    Ok(key)
}

pub fn read_run_value() -> Option<String> {
    let key = open_run_key(KEY_QUERY_VALUE).ok()?;
    let name = wide(RUN_VALUE);
    let mut buffer = vec![0u16; 1024];
    let mut size = (buffer.len() * 2) as u32;
    // SAFETY: buffer/size describe writable memory.
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    // SAFETY: key was opened above.
    let _ = unsafe { RegCloseKey(key) };
    if status.is_err() {
        return None;
    }
    let len = (size as usize / 2).min(buffer.len());
    let text = String::from_utf16_lossy(&buffer[..len]);
    Some(text.trim_end_matches('\0').to_string())
}

pub fn set_run_value(command: Option<&str>) -> Result<()> {
    let key = open_run_key(KEY_SET_VALUE)?;
    let name = wide(RUN_VALUE);
    // SAFETY: key is open; data slice is valid.
    let status = unsafe {
        match command {
            Some(command) => {
                let data = wide(command);
                let bytes = std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), data.len() * 2);
                RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes))
            }
            None => RegDeleteValueW(key, PCWSTR(name.as_ptr())),
        }
    };
    // SAFETY: key was opened above.
    let _ = unsafe { RegCloseKey(key) };
    if command.is_none() && status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    status.ok().context("writing HKCU Run value")
}

fn schtasks(args: &[&str]) -> Result<std::process::Output> {
    let system = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let exe = PathBuf::from(system).join("System32").join("schtasks.exe");
    Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("running schtasks.exe")
}

pub fn elevated_task_exists() -> bool {
    schtasks(&["/query", "/tn", ELEVATED_TASK])
        .map(|out| out.status.success())
        .unwrap_or(false)
}

pub fn run_elevated_task() -> Result<()> {
    let out = schtasks(&["/run", "/tn", ELEVATED_TASK])?;
    if !out.status.success() {
        bail!(
            "schtasks /run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn end_elevated_task() {
    let _ = schtasks(&["/end", "/tn", ELEVATED_TASK]);
}

pub fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\Default\AppData\Local"))
}

/// `%LOCALAPPDATA%\BubblePlay`, overridable for tests via `BUBBLEPLAY_TRAY_DATA_DIR`.
pub fn data_dir() -> PathBuf {
    std::env::var_os("BUBBLEPLAY_TRAY_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| local_app_data().join("BubblePlay"))
}

pub fn host_exe_beside(current_exe: &Path) -> PathBuf {
    current_exe
        .parent()
        .map(|dir| dir.join("bubbleplay-host.exe"))
        .unwrap_or_else(|| PathBuf::from("bubbleplay-host.exe"))
}

/// Open a folder in Explorer.
pub fn open_folder(path: &Path) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let _ = std::fs::create_dir_all(path);
    let verb = wide("open");
    let target = wide(path.as_os_str());
    // SAFETY: NUL-terminated strings live across the call.
    unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}
