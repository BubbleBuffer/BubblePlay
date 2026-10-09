//! Interactive desktop detection and transition policy for the privileged agent.
//!
//! The LocalSystem broker launches the agent into the active console session.
//! A fresh media/input worker binds to whichever desktop currently receives
//! input; no USER or D3D resources survive a Default/Winlogon transition.

use anyhow::{ensure, Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, IsWellKnownSid,
    TokenElevationType, TokenIntegrityLevel, TokenSessionId, TokenUIAccess, TokenUser,
    WinLocalSystemSid, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, CloseWindowStation, GetProcessWindowStation, GetThreadDesktop,
    GetUserObjectInformationW, OpenDesktopW, OpenInputDesktop, OpenWindowStationW,
    SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_JOURNALPLAYBACK,
    DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS, HDESK, UOI_NAME,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    WINSTA_ENUMDESKTOPS, WINSTA_READATTRIBUTES, WINSTA_WRITEATTRIBUTES,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopKind {
    Default,
    Secure,
    Other,
}

impl DesktopKind {
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "default" => Self::Default,
            "winlogon" => Self::Secure,
            _ => Self::Other,
        }
    }

    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Secure => "secure",
            Self::Other => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopTransition {
    pub previous: Option<DesktopKind>,
    pub current: DesktopKind,
    pub generation: u32,
}

#[derive(Debug, Default)]
pub struct DesktopTracker {
    current: Option<DesktopKind>,
    generation: u32,
}

impl DesktopTracker {
    pub fn observe(&mut self, kind: DesktopKind) -> Option<DesktopTransition> {
        if self.current == Some(kind) {
            return None;
        }
        let previous = self.current.replace(kind);
        self.generation = self.generation.wrapping_add(1).max(1);
        Some(DesktopTransition {
            previous,
            current: kind,
            generation: self.generation,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopMediaPolicy {
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub idr_interval_ms: u32,
}

pub const fn media_policy(kind: DesktopKind) -> DesktopMediaPolicy {
    match kind {
        DesktopKind::Secure | DesktopKind::Other => DesktopMediaPolicy {
            max_width: 1920,
            max_height: 1080,
            fps: 15,
            bitrate_bps: 3_000_000,
            idr_interval_ms: 1_000,
        },
        DesktopKind::Default => DesktopMediaPolicy {
            max_width: 1920,
            max_height: 1080,
            fps: 60,
            bitrate_bps: 8_000_000,
            idr_interval_ms: 1_000,
        },
    }
}

pub struct InputDesktop {
    handle: HDESK,
    name: String,
}

/// Thread-affine binding guard. Worker-owned USER/D3D/input objects must be
/// dropped before this guard so the thread can return to its previous desktop
/// and close the target HDESK without leaking one handle per transition.
pub struct BoundInputDesktop {
    target: HDESK,
    previous: HDESK,
    kind: DesktopKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSecurityContext {
    pub token_is_local_system: bool,
    pub token_session_id: u32,
    pub integrity_rid: u32,
    pub integrity: &'static str,
    pub elevation_type: i32,
    pub ui_access: bool,
    pub process_window_station: String,
    pub thread_desktop: String,
    pub winsta_input_access: bool,
    pub desktop_input_access: bool,
}

/// Only the token properties relevant to normal-desktop UIPI. A successful
/// token query does not establish that a particular input event was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessInputPrivilege {
    pub integrity_rid: u32,
    pub ui_access: bool,
}

impl ProcessInputPrivilege {
    pub fn requires_admin_from(self, host: Self) -> bool {
        self.integrity_rid > host.integrity_rid || self.ui_access
    }

    fn from_process(process: HANDLE) -> Result<Self> {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }
            .context("OpenProcessToken for foreground privilege")?;
        let token = TokenHandle(token);
        Ok(Self {
            integrity_rid: token_integrity_rid(token.0)?,
            ui_access: token_u32(token.0, TokenUIAccess)? != 0,
        })
    }
}

/// Called on the existing foreground housekeeping cadence. Token inspection
/// happens only when HWND/PID changes, including windows ignored by following
/// (for example, a modal dialog or a window during a drag).
pub struct ForegroundPrivilegeTracker {
    host: Option<ProcessInputPrivilege>,
    identity: Option<(isize, u32)>,
    target: Option<ProcessInputPrivilege>,
}

impl Default for ForegroundPrivilegeTracker {
    fn default() -> Self {
        Self {
            host: ProcessInputPrivilege::from_process(unsafe { GetCurrentProcess() }).ok(),
            identity: None,
            target: None,
        }
    }
}

impl ForegroundPrivilegeTracker {
    pub fn pid(&self) -> Option<u32> {
        self.identity.map(|(_, pid)| pid)
    }

    pub fn refresh(&mut self) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId,
        };
        let window = unsafe { GetForegroundWindow() };
        let mut pid = 0;
        if !window.is_invalid() {
            unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        }
        let identity = (pid != 0).then_some((window.0 as isize, pid));
        if self.identity == identity {
            return false;
        }
        self.identity = identity;
        self.target = identity.and_then(|(_, pid)| {
            let process =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
            let process = TokenHandle(process);
            ProcessInputPrivilege::from_process(process.0).ok()
        });
        tracing::info!(
            foreground_pid = identity.map(|(_, pid)| pid),
            host_integrity = self.host.map(|token| token.integrity_rid),
            foreground_integrity = self.target.map(|token| token.integrity_rid),
            foreground_ui_access = self.target.map(|token| token.ui_access),
            privilege_known = self.host.is_some() && self.target.is_some(),
            "remote input foreground privilege changed"
        );
        true
    }

    pub fn requires_admin(&self) -> bool {
        self.target
            .zip(self.host)
            .is_some_and(|(target, host)| target.requires_admin_from(host))
    }
}

impl BoundInputDesktop {
    pub fn kind(&self) -> DesktopKind {
        self.kind
    }

    pub fn security_context(&self) -> Result<InputSecurityContext> {
        InputSecurityContext::current(&self.kind)
    }
}

impl InputSecurityContext {
    fn current(kind: &DesktopKind) -> Result<Self> {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .context("OpenProcessToken for input diagnostics")?;
        let token = TokenHandle(token);

        let token_is_local_system = token_is_local_system(token.0)?;
        let token_session_id = token_u32(token.0, TokenSessionId)?;
        let elevation_type = token_i32(token.0, TokenElevationType)?;
        let ui_access = token_u32(token.0, TokenUIAccess)? != 0;
        let integrity_rid = token_integrity_rid(token.0)?;

        let process_window_station = unsafe { GetProcessWindowStation() }
            .context("GetProcessWindowStation for input diagnostics")?;
        let process_window_station = user_object_name(HANDLE(process_window_station.0))?;
        let thread_desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) }
            .context("GetThreadDesktop for input diagnostics")?;
        let thread_desktop = user_object_name(HANDLE(thread_desktop.0))?;

        let winsta_input_access = unsafe {
            OpenWindowStationW(
                windows::core::w!("winsta0"),
                false,
                (WINSTA_ENUMDESKTOPS | WINSTA_READATTRIBUTES | WINSTA_WRITEATTRIBUTES) as u32,
            )
        }
        .map(|handle| {
            let _ = unsafe { CloseWindowStation(handle) };
            true
        })
        .unwrap_or(false);

        let desktop_name = match kind {
            DesktopKind::Default => "default",
            DesktopKind::Secure => "winlogon",
            DesktopKind::Other => thread_desktop.as_str(),
        };
        let desktop_name = desktop_name
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let desktop_access = DESKTOP_ACCESS_FLAGS(
            DESKTOP_READOBJECTS.0 | DESKTOP_WRITEOBJECTS.0 | DESKTOP_SWITCHDESKTOP.0,
        );
        let desktop_input_access = unsafe {
            OpenDesktopW(
                PCWSTR(desktop_name.as_ptr()),
                DESKTOP_CONTROL_FLAGS(0),
                false,
                desktop_access.0,
            )
        }
        .map(|handle| {
            let _ = unsafe { CloseDesktop(handle) };
            true
        })
        .unwrap_or(false);

        Ok(Self {
            token_is_local_system,
            token_session_id,
            integrity_rid,
            integrity: integrity_name(integrity_rid),
            elevation_type,
            ui_access,
            process_window_station,
            thread_desktop,
            winsta_input_access,
            desktop_input_access,
        })
    }
}

struct TokenHandle(HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn token_u32(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<u32> {
    let mut value = 0u32;
    let mut returned = 0u32;
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some((&mut value as *mut u32).cast()),
            std::mem::size_of::<u32>() as u32,
            &mut returned,
        )
    }
    .context("GetTokenInformation(u32)")?;
    ensure!(
        returned >= std::mem::size_of::<u32>() as u32,
        "token scalar was truncated"
    );
    Ok(value)
}

fn token_i32(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<i32> {
    Ok(token_u32(token, class)? as i32)
}

fn token_buffer(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u8>> {
    let mut needed = 0u32;
    let _ = unsafe { GetTokenInformation(token, class, None, 0, &mut needed) };
    ensure!(needed > 0, "token information length was not returned");
    let mut buffer = vec![0u8; needed as usize];
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .context("GetTokenInformation(buffer)")?;
    Ok(buffer)
}

fn token_is_local_system(token: HANDLE) -> Result<bool> {
    let buffer = token_buffer(token, TokenUser)?;
    let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
    Ok(unsafe { IsWellKnownSid(user.User.Sid, WinLocalSystemSid) }.as_bool())
}

fn token_integrity_rid(token: HANDLE) -> Result<u32> {
    let buffer = token_buffer(token, TokenIntegrityLevel)?;
    let label = unsafe { &*(buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()) };
    let count = unsafe { *GetSidSubAuthorityCount(label.Label.Sid) } as u32;
    ensure!(count > 0, "integrity SID has no subauthorities");
    Ok(unsafe { *GetSidSubAuthority(label.Label.Sid, count - 1) })
}

fn integrity_name(rid: u32) -> &'static str {
    match rid {
        0x0000..=0x0fff => "untrusted",
        0x1000..=0x1fff => "low",
        0x2000..=0x2fff => "medium",
        0x3000..=0x3fff => "high",
        0x4000..=0x4fff => "system",
        _ => "protected",
    }
}

impl InputDesktop {
    pub fn open() -> Result<Self> {
        // SendInput on the bound thread fails with ERROR_ACCESS_DENIED unless
        // the desktop handle carries DESKTOP_JOURNALPLAYBACK.
        let access = DESKTOP_ACCESS_FLAGS(
            DESKTOP_READOBJECTS.0
                | DESKTOP_WRITEOBJECTS.0
                | DESKTOP_SWITCHDESKTOP.0
                | DESKTOP_JOURNALPLAYBACK.0,
        );
        let handle = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, access) }
            .context("OpenInputDesktop")?;
        let name = desktop_name(handle).context("read input desktop name")?;
        Ok(Self { handle, name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> DesktopKind {
        DesktopKind::from_name(&self.name)
    }

    /// Bind a newly-created worker thread before it creates any windows,
    /// hooks, cursor state, D3D devices, or duplication objects.
    pub fn bind_thread(self) -> Result<BoundInputDesktop> {
        let kind = self.kind();
        let previous =
            unsafe { GetThreadDesktop(GetCurrentThreadId()) }.context("GetThreadDesktop")?;
        unsafe { SetThreadDesktop(self.handle) }.context("SetThreadDesktop")?;
        let target = self.handle;
        std::mem::forget(self);
        Ok(BoundInputDesktop {
            target,
            previous,
            kind,
        })
    }
}

impl Drop for BoundInputDesktop {
    fn drop(&mut self) {
        // All desktop-bound resources have already been dropped by the worker.
        // Restoring the previous desktop makes the owned target closable.
        if unsafe { SetThreadDesktop(self.previous) }.is_ok() {
            let _ = unsafe { CloseDesktop(self.target) };
        }
    }
}

impl Drop for InputDesktop {
    fn drop(&mut self) {
        let _ = unsafe { CloseDesktop(self.handle) };
    }
}

fn desktop_name(handle: HDESK) -> Result<String> {
    user_object_name(HANDLE(handle.0))
}

/// Read-only probe from the interactive host. Failure (including access denied)
/// cannot establish that normal-desktop capture or input is safe.
pub fn input_desktop_kind() -> Option<DesktopKind> {
    let handle =
        unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) }.ok()?;
    let kind = desktop_name(handle)
        .ok()
        .map(|name| DesktopKind::from_name(&name));
    let _ = unsafe { CloseDesktop(handle) };
    kind
}

pub fn input_desktop_is_default() -> bool {
    input_desktop_kind() == Some(DesktopKind::Default)
}

pub fn normal_frame_is_verified(
    frame_acquired: bool,
    desktop: Option<DesktopKind>,
    helper_secure: bool,
) -> bool {
    frame_acquired && desktop == Some(DesktopKind::Default) && !helper_secure
}

/// Best-effort classification from the interactive host. Access to Winlogon
/// normally fails at medium integrity; never guess that such a prompt is UAC.
pub fn secure_desktop_kind() -> Option<&'static str> {
    let desktop = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) };
    match desktop {
        Ok(handle) => {
            let kind = desktop_name(handle)
                .ok()
                .map(|name| DesktopKind::from_name(&name));
            let consent = kind == Some(DesktopKind::Secure) && desktop_has_consent(handle);
            let _ = unsafe { CloseDesktop(handle) };
            match kind {
                Some(DesktopKind::Secure) => Some(if consent {
                    "uac"
                } else if session_locked() {
                    "lock"
                } else {
                    "other"
                }),
                Some(DesktopKind::Default) => None,
                _ => Some("other"),
            }
        }
        Err(error) if error.code() == windows::Win32::Foundation::E_ACCESSDENIED => {
            Some(if session_locked() { "lock" } else { "other" })
        }
        Err(_) => None,
    }
}

pub fn session_locked() -> bool {
    use windows::core::PWSTR;
    use windows::Win32::System::RemoteDesktop::{
        WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW,
        WTS_SESSIONSTATE_LOCK,
    };
    let session = crate::admin_agent::trust::current_session_id();
    let mut buffer = PWSTR::null();
    let mut bytes = 0;
    if unsafe {
        WTSQuerySessionInformationW(None, session, WTSSessionInfoEx, &mut buffer, &mut bytes)
    }
    .is_err()
    {
        return false;
    }
    let locked = if !buffer.is_null() && bytes as usize >= std::mem::size_of::<WTSINFOEXW>() {
        // WTS allocates an aligned, sized WTSINFOEXW for this query.
        let info = unsafe { &*buffer.0.cast::<WTSINFOEXW>() };
        info.Level == 1
            && unsafe {
                info.Data.WTSInfoExLevel1.SessionId == session
                    && info.Data.WTSInfoExLevel1.SessionFlags == WTS_SESSIONSTATE_LOCK as i32
            }
    } else {
        false
    };
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    locked
}

fn desktop_has_consent(desktop: HDESK) -> bool {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::System::StationsAndDesktops::EnumDesktopWindows;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    unsafe extern "system" fn visit(window: HWND, param: LPARAM) -> BOOL {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        let expected = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .map(|root| root.join("System32").join("consent.exe"));
        if crate::admin_agent::trust::process_session_id(pid)
            == Some(crate::admin_agent::trust::current_session_id())
            && expected
                .zip(crate::admin_agent::trust::process_image_path(pid))
                .is_some_and(|(expected, actual)| {
                    expected
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&actual.to_string_lossy())
                })
        {
            unsafe { *(param.0 as *mut bool) = true };
        }
        true.into()
    }
    let mut found = false;
    let result = unsafe {
        EnumDesktopWindows(
            Some(desktop),
            Some(visit),
            LPARAM((&mut found as *mut bool) as isize),
        )
    };
    result.is_ok() && found
}

fn user_object_name(handle: HANDLE) -> Result<String> {
    let mut needed = 0u32;
    let _ = unsafe { GetUserObjectInformationW(handle, UOI_NAME, None, 0, Some(&mut needed)) };
    if needed < 2 {
        anyhow::bail!("desktop name length was not returned");
    }
    let mut buffer = vec![0u16; (needed as usize).div_ceil(2)];
    unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            Some(&mut needed),
        )
    }
    .context("GetUserObjectInformationW(UOI_NAME)")?;
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    Ok(String::from_utf16_lossy(&buffer[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_privilege_compares_integrity_and_ui_access_to_host() {
        let medium = ProcessInputPrivilege {
            integrity_rid: 0x2000,
            ui_access: false,
        };
        let high = ProcessInputPrivilege {
            integrity_rid: 0x3000,
            ui_access: false,
        };
        let low = ProcessInputPrivilege {
            integrity_rid: 0x1000,
            ui_access: false,
        };
        let ui_access = ProcessInputPrivilege {
            ui_access: true,
            ..medium
        };
        assert!(!medium.requires_admin_from(medium));
        assert!(!low.requires_admin_from(medium));
        assert!(high.requires_admin_from(medium));
        assert!(ui_access.requires_admin_from(medium));
        assert!(!medium.requires_admin_from(high));
        assert!(!high.requires_admin_from(high));
    }

    #[test]
    fn secure_or_unknown_input_desktop_blocks_a_dda_recovery_frame() {
        for desktop in [Some(DesktopKind::Secure), Some(DesktopKind::Other), None] {
            // None includes OpenInputDesktop access denied and name query failure.
            assert!(!normal_frame_is_verified(true, desktop, false));
            assert!(!normal_frame_is_verified(true, desktop, true));
        }
        assert!(!normal_frame_is_verified(
            true,
            Some(DesktopKind::Default),
            true
        ));
        assert!(!normal_frame_is_verified(
            false,
            Some(DesktopKind::Default),
            false
        ));
        assert!(normal_frame_is_verified(
            true,
            Some(DesktopKind::Default),
            false
        ));
    }

    #[test]
    fn classifies_windows_desktop_names_without_case_sensitivity() {
        assert_eq!(DesktopKind::from_name("Default"), DesktopKind::Default);
        assert_eq!(DesktopKind::from_name("WINLOGON"), DesktopKind::Secure);
        assert_eq!(DesktopKind::from_name("Disconnect"), DesktopKind::Other);
    }

    #[test]
    fn transition_generation_changes_once_per_real_desktop_switch() {
        let mut tracker = DesktopTracker::default();
        let initial = tracker.observe(DesktopKind::Default).unwrap();
        assert_eq!(initial.generation, 1);
        assert!(tracker.observe(DesktopKind::Default).is_none());
        let secure = tracker.observe(DesktopKind::Secure).unwrap();
        assert_eq!(secure.previous, Some(DesktopKind::Default));
        assert_eq!(secure.generation, 2);
    }

    #[test]
    fn secure_desktop_policy_is_bounded_for_static_system_ui() {
        let policy = media_policy(DesktopKind::Secure);
        assert_eq!((policy.max_width, policy.max_height), (1920, 1080));
        assert_eq!((policy.fps, policy.bitrate_bps), (15, 3_000_000));
        assert_eq!(policy.idr_interval_ms, 1_000);
    }

    #[test]
    fn integrity_rids_are_reported_without_exposing_token_material() {
        assert_eq!(integrity_name(0x1000), "low");
        assert_eq!(integrity_name(0x2000), "medium");
        assert_eq!(integrity_name(0x3000), "high");
        assert_eq!(integrity_name(0x4000), "system");
        assert_eq!(integrity_name(0x5000), "protected");
    }
}
