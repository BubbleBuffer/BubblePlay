//! Win32 helpers for the tray: DPAPI, kill-on-close job, HKCU Run key,
//! named kernel objects, the `\BubblePlay\AdminAgent` scheduled task and the
//! one-UAC agent update.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, HANDLE,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    RRF_RT_REG_DWORD,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenMutexW, ResetEvent, SetEvent, WaitForSingleObject,
    SYNCHRONIZATION_SYNCHRONIZE,
};

use super::logic::decode_hex;

// The DPAPI helpers live in the host library so the trusted device list shares them.
pub use bubbleplay_host::dpapi::{dpapi_protect_bytes, dpapi_unprotect_bytes};

pub use bubbleplay_host::admin_agent::protocol::{AGENT_MUTEX, AGENT_STOP_EVENT, TRAY_MUTEX};
/// On-demand task that starts the elevated admin input agent
/// (`%ProgramFiles%\BubblePlay\agent\bubbleplay_admin_agent.exe`).
pub const AGENT_TASK: &str = r"\BubblePlay\AdminAgent";
/// Folder and name halves of `AGENT_TASK`, for the PowerShell task query.
pub const TASK_FOLDER: &str = r"\BubblePlay\";
pub const AGENT_TASK_NAME: &str = "AdminAgent";
/// Files of the per-user install, shared with `setup-tray.ps1` and `update-tray.ps1`
/// (`fixtures/install-names-contract.json`).
pub const HOST_EXE: &str = "bubbleplay-host.exe";
pub const HOST_CONFIG_FILE: &str = "host.json";
pub const UPDATE_STATE_FILE: &str = "update-state.json";
pub const SECURE_HELPER_TASK: &str = r"\BubblePlay\SecureDesktop";
pub const ADMIN_MODE_OFF_TASK: &str = r"\BubblePlay\AdminModeOff";
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
    let plain = dpapi_unprotect_bytes(&encrypted);
    encrypted.fill(0);
    plain
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
        // Exists, but its (e.g. elevated) owner did not grant us SYNCHRONIZE.
        Err(error) => error.code() == windows::Win32::Foundation::E_ACCESSDENIED,
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

/// HKCU `...\Themes\Personalize` `SystemUsesLightTheme` (taskbar/notification-area theme),
/// or `None` when the value is missing or unreadable.
pub fn system_uses_light_theme() -> Option<u32> {
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let name = wide("SystemUsesLightTheme");
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: the strings are NUL-terminated; value/size describe a writable DWORD.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    status.is_ok().then_some(value)
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

pub fn ensure_agent_task() -> Result<()> {
    let out = schtasks(&["/query", "/tn", AGENT_TASK])?;
    if !out.status.success() {
        bail!(
            "admin agent task {AGENT_TASK} missing or unavailable (query exit {:?}): {} {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn run_agent_task() -> Result<()> {
    let out = schtasks(&["/run", "/tn", AGENT_TASK])?;
    if !out.status.success() {
        bail!(
            "schtasks /run failed (exit {:?}): {} {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Read the task's execution result after a failed hello. This is diagnostic
/// only; querying the task cannot launch or stop it. PowerShell supplies the
/// numeric result without relying on localized schtasks column labels.
pub fn agent_task_result() -> String {
    let system = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let exe = PathBuf::from(system).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    match Command::new(exe)
        .args(["-NoProfile", "-NonInteractive", "-Command",
            r"$ErrorActionPreference='Stop'; (Get-ScheduledTaskInfo -TaskPath '\BubblePlay\' -TaskName 'AdminAgent').LastTaskResult"])
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(out) if out.status.success() => format!("; task result {}", String::from_utf8_lossy(&out.stdout).trim()),
        Ok(out) => format!("; task result query failed (exit {:?}): {}", out.status.code(), String::from_utf8_lossy(&out.stderr).trim()),
        Err(error) => format!("; task result query failed: {error}"),
    }
}

/// The SYSTEM task launches an independent session process. The host pipe
/// and helper timeout own its lifetime; never use /end on the launcher.
pub fn secure_helper_task_exists() -> bool {
    schtasks(&["/query", "/tn", SECURE_HELPER_TASK])
        .map(|out| out.status.success())
        .unwrap_or(false)
}

pub fn run_secure_helper_task() -> Result<()> {
    let out = schtasks(&["/run", "/tn", SECURE_HELPER_TASK])?;
    if !out.status.success() {
        bail!(
            "SecureDesktop task launch failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn run_admin_mode_off_task() -> Result<()> {
    let out = schtasks(&["/run", "/tn", ADMIN_MODE_OFF_TASK])?;
    if !out.status.success() {
        bail!(
            "AdminModeOff task launch failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn end_agent_task() {
    let _ = schtasks(&["/end", "/tn", AGENT_TASK]);
}

/// Run `program params` elevated (one UAC prompt) and wait for it. Returns
/// the exit code; an error when the prompt was declined or the start failed.
pub fn run_elevated_and_wait(program: &Path, params: &str, dir: &Path) -> Result<u32> {
    use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let verb = wide("runas");
    let file = wide(program.as_os_str());
    let parameters = wide(params);
    let directory = wide(dir.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        lpDirectory: PCWSTR(directory.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: all strings outlive the call; hProcess is closed by Handle.
    unsafe { ShellExecuteExW(&mut info) }.context("elevated start declined or failed")?;
    if info.hProcess.is_invalid() {
        bail!("elevated start returned no process");
    }
    let process = Handle(info.hProcess);
    // SAFETY: valid process handle.
    unsafe { WaitForSingleObject(process.0, INFINITE) };
    let mut code = 0u32;
    // SAFETY: valid process handle and out-pointer.
    unsafe { GetExitCodeProcess(process.0, &mut code) }.context("GetExitCodeProcess")?;
    Ok(code)
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

/// SHA-256 (uppercase hex, like PowerShell) of a file, via CNG.
pub fn sha256_file(path: &Path) -> Option<String> {
    use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let data = std::fs::read(path).ok()?;
    let mut digest = [0u8; 32];
    // SAFETY: slices are valid for the call; pseudo-handle needs no cleanup.
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, &data, &mut digest) };
    if status.0 != 0 {
        return None;
    }
    Some(digest.iter().map(|b| format!("{b:02X}")).collect())
}

pub fn host_exe_beside(current_exe: &Path) -> PathBuf {
    current_exe
        .parent()
        .map(|dir| dir.join(HOST_EXE))
        .unwrap_or_else(|| PathBuf::from(HOST_EXE))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_value(name: &str) -> String {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/install-names-contract.json")).unwrap();
        fixture["values"][name]["value"].as_str().unwrap_or_else(|| panic!("fixture lacks {name}")).to_owned()
    }

    /// The names the install scripts register must be the names the tray starts and queries.
    #[test]
    fn install_names_match_the_shared_fixture() {
        let folder = fixture_value("taskFolder");
        assert_eq!(TASK_FOLDER, folder);
        assert_eq!(AGENT_TASK_NAME, fixture_value("agentTask"));
        assert_eq!(AGENT_TASK, format!("{folder}{}", fixture_value("agentTask")));
        assert_eq!(SECURE_HELPER_TASK, format!("{folder}{}", fixture_value("secureDesktopTask")));
        assert_eq!(ADMIN_MODE_OFF_TASK, format!("{folder}{}", fixture_value("adminModeOffTask")));
        assert_eq!(RUN_VALUE, fixture_value("runValue"));
        assert_eq!(HOST_EXE, fixture_value("hostExe"));
        assert_eq!(HOST_CONFIG_FILE, fixture_value("hostConfigFile"));
        assert_eq!(UPDATE_STATE_FILE, fixture_value("updateStateFile"));
    }
}
