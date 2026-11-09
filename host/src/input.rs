//! Browser input -> Windows SendInput.
//!
//! Wire format (binary, little-endian). Control is reliable; motion is lossy.
//!
//! | type | name     | payload                                   |
//! |------|----------|-------------------------------------------|
//! | 0    | key down | u16 vk (Windows VK code)                  |
//! | 1    | key up   | u16 vk                                    |
//! | 2    | mouse rel| i16 dx, i16 dy (PointerLock unadjusted)   |
//! | 3    | mouse abs| u16 x, u16 y (0..65535 over selected mon.) |
//! | 4    | buttons  | u8 bitmask (1=left 2=right 4=middle 8=X1 16=X2) |
//! | 5    | wheel    | i16 delta (WHEEL_DELTA units)             |
//! | 6    | release  | no payload                                |
//! | 7    | motion v2| u16 seq, i16 dx, dy, wheel_x, wheel_y   |
//! | 8    | absolute v2 | u16 seq, u16 x, u16 y                 |
//! | 9    | buttons at | u16 seq barrier, u16 x, u16 y, u8 mask |
//! | 10   | cursor sync | u16 sequence barrier                   |
//! | 11   | text       | UTF-8 bytes (max 512)                  |
//! | 12   | clipboard  | u8 flags(1=paste), u16 id, u16 index, u16 count, UTF-8 chunk (max 8 KiB) |
//! | 13   | scan key   | u8 flags(1=up 2=extended), u16 set-1 scan (0 = use vk), u16 vk |
//! | 128  | cursor state (host -> browser) | u16 ack, x, y, u8 flags |
//! | 129  | cursor kind (host -> client, reliable) | u8 native kind |
//!
//! Anti-cheat note: SendInput is the v0 injection method. Games with kernel
//! anti-cheat may ignore synthetic input or flag the process; accepted for v0.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::admin_agent::client::AdminAgentLink;
use crate::admin_agent::protocol::Record;
use crate::clipboard;
use crate::clipboard_worker::{ClipboardWorker, Command as ClipboardCommand, Completion};
pub use crate::input_diagnostics::{
    InputDiagnosticCounters, InputDropReason, InputRoute, InputRoutingStatus, InputStatusReason,
};
use crate::input_diagnostics::{InputFlowTransitions, InputStatusChanges};
use windows::Win32::Foundation::{
    CloseHandle, SetLastError, ERROR_INVALID_PARAMETER, HANDLE, POINT, WAIT_ABANDONED,
    WAIT_OBJECT_0, WIN32_ERROR,
};
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, ReleaseMutex, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};
use windows::Win32::UI::Accessibility::MOUSEKEYS;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetCursorPos, GetSystemMetrics, LoadCursorW, SystemParametersInfoW, CURSORINFO,
    CURSOR_SHOWING, IDC_APPSTARTING, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO,
    IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_UPARROW, IDC_WAIT,
    MKF_MOUSEKEYSON, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_MOUSEPRESENT, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SPI_GETMOUSEKEYS, SPI_SETMOUSEKEYS, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

const TYPE_KEY_DOWN: u8 = 0;
const TYPE_KEY_UP: u8 = 1;
const TYPE_MOUSE_REL: u8 = 2;
const TYPE_MOUSE_ABS: u8 = 3;
const TYPE_BUTTONS: u8 = 4;
const TYPE_WHEEL: u8 = 5;
const TYPE_RELEASE_ALL: u8 = 6;
const TYPE_MOTION_V2: u8 = 7;
const TYPE_ABSOLUTE_V2: u8 = 8;
const TYPE_BUTTONS_AT: u8 = 9;
const TYPE_CURSOR_SYNC: u8 = 10;
const TYPE_TEXT: u8 = 11;
const TYPE_CLIPBOARD: u8 = 12;
const TYPE_KEY_SCAN: u8 = 13;
const MAX_TEXT_BYTES: usize = 512;
pub const CLIPBOARD_CHUNK_BYTES: usize = 8 * 1024;
pub const CLIPBOARD_MAX_CHUNKS: u16 = 128;
const CLIPBOARD_FLAG_PASTE: u8 = 1;
const SCAN_FLAG_UP: u8 = 1;
const SCAN_FLAG_EXTENDED: u8 = 2;
const VK_CONTROL: u16 = 0x11;
const VK_LCONTROL: u16 = 0xA2;
const VK_RCONTROL: u16 = 0xA3;
const VK_LWIN: u16 = 0x5B;
const VK_RWIN: u16 = 0x5C;
const VK_V: u16 = 0x56;
/// Unassigned VK used (like AutoHotkey's menu mask key) so releasing a held
/// Windows key during a paste does not open the Start menu.
const VK_MASK: u16 = 0xE8;
const SCAN_LCONTROL: u8 = 0x1D;
const SCAN_LWIN: u8 = 0x5B;
const SCAN_RWIN: u8 = 0x5C;
const INPUT_FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(5);

pub const TYPE_CURSOR_STATE: u8 = 128;
pub const TYPE_CURSOR_KIND: u8 = 129;
pub(crate) const CURSOR_ACK_VALID: u8 = 1;
const CURSOR_IN_BOUNDS: u8 = 2;
const CURSOR_VISIBLE: u8 = 4;
// Work mode treats the browser overlay as authoritative. A 10 Hz heartbeat
// lets it detect an OS cursor displaced by local input without flooding the
// control channel; changed positions are still emitted immediately.
const CURSOR_HEARTBEAT: Duration = Duration::from_millis(100);

/// Make Windows expose cursor semantics on a headless PC only while a viewer
/// is active. No registry persistence or mouse movement is requested. MouseKeys
/// retains the user's NumLock policy, so its normal numpad behavior applies
/// until this guard restores the feature on disconnect or Drop.
#[derive(Default)]
pub struct HeadlessCursorSupport {
    state: HeadlessCursorState,
    marker: Option<PathBuf>,
    retry: HeadlessCursorRetry,
}

impl HeadlessCursorSupport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_active(&mut self, active: bool) {
        let now = Instant::now();
        if !self.retry.take_attempt(active, now) {
            return;
        }
        let _lock = match CursorSupportLock::acquire() {
            Ok(lock) => lock,
            Err(error) => {
                // Record the requested edge; retries wait for their deadline.
                self.state.active = active;
                self.retry.finish(false, now);
                warn!(?error, "headless cursor support lifecycle lock unavailable");
                return;
            }
        };
        self.remove_released_marker();
        if active && self.state.owned.is_none() {
            recover_cursor_marker(None);
        }
        let original = std::cell::Cell::new(None);
        let mut claimed = None;
        let enabling = active && self.state.owned.is_none();
        let succeeded = self.state.apply_active_with(
            active,
            unsafe { GetSystemMetrics(SM_MOUSEPRESENT) } != 0,
            || {
                let settings = read_mouse_keys()?;
                original.set(Some(settings));
                Ok(settings)
            },
            |settings| {
                if enabling {
                    let path = create_cursor_marker(MouseKeysLease {
                        original: original.get().expect("settings were read before enabling"),
                        applied: *settings,
                    })?;
                    let result = write_mouse_keys(settings);
                    if result.is_ok() || !remove_cursor_marker(&path, std::process::id()) {
                        claimed = Some(path);
                    }
                    result
                } else {
                    write_mouse_keys(settings)
                }
            },
        );
        if let Some(path) = claimed {
            self.marker = Some(path);
        }
        self.remove_released_marker();
        // A failed marker deletion must also be retried before a later viewer
        // can claim its lease. Successful transitions never poll external edits.
        let released = self.state.owned.is_some() || self.marker.is_none();
        self.retry.finish(succeeded && released, now);
    }

    fn remove_released_marker(&mut self) {
        if self.state.owned.is_none() {
            if let Some(path) = self.marker.as_ref() {
                if remove_cursor_marker(path, std::process::id()) {
                    self.marker = None;
                }
            }
        }
    }
}

#[derive(Default)]
struct HeadlessCursorRetry {
    requested_active: Option<bool>,
    attempts: u8,
    next_attempt: Option<Instant>,
}

impl HeadlessCursorRetry {
    fn take_attempt(&mut self, active: bool, now: Instant) -> bool {
        if self.requested_active != Some(active) {
            self.requested_active = Some(active);
            self.attempts = 0;
            self.next_attempt = None;
        } else if self.next_attempt.is_none_or(|next| now < next) {
            return false;
        }
        self.attempts += 1;
        self.next_attempt = None;
        true
    }

    fn finish(&mut self, succeeded: bool, now: Instant) {
        // One initial attempt plus two bounded recovery attempts. Never retry
        // a successful activation because the user may disable it themselves.
        if !succeeded && self.attempts < 3 {
            self.next_attempt = Some(now + Duration::from_secs(self.attempts as u64));
        }
    }
}

impl Drop for HeadlessCursorSupport {
    fn drop(&mut self) {
        // One final restoration/marker cleanup attempt after bounded retries.
        // A process that never acquired ownership does no settings API work.
        if self.state.owned.is_some() || self.marker.is_some() {
            if let Ok(_lock) = CursorSupportLock::acquire() {
                self.state.restore_with(read_mouse_keys, write_mouse_keys);
                self.remove_released_marker();
            }
        }
    }
}

#[derive(Clone, Copy)]
struct MouseKeysLease {
    original: MOUSEKEYS,
    applied: MOUSEKEYS,
}

// The marker records only the one feature bit we own and its surrounding
// preferences. It is durable before the API write, so tray kill/job teardown
// can restore it even when Rust Drop cannot run.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorRecoveryRecord {
    owner_pid: u32,
    original: [u32; 7],
    applied: [u32; 7],
}

impl CursorRecoveryRecord {
    fn new(owner_pid: u32, lease: MouseKeysLease) -> Self {
        Self {
            owner_pid,
            original: mouse_keys_fields(lease.original),
            applied: mouse_keys_fields(lease.applied),
        }
    }

    fn lease(&self) -> windows::core::Result<MouseKeysLease> {
        let original = mouse_keys_from_fields(self.original);
        let applied = mouse_keys_from_fields(self.applied);
        let mut expected = original;
        expected.dwFlags |= MKF_MOUSEKEYSON;
        if self.owner_pid == 0
            || original.cbSize != std::mem::size_of::<MOUSEKEYS>() as u32
            || original.dwFlags & MKF_MOUSEKEYSON != 0
            || applied != expected
        {
            return Err(cursor_support_error(
                "invalid headless cursor ownership record",
            ));
        }
        Ok(MouseKeysLease { original, applied })
    }
}

fn mouse_keys_fields(settings: MOUSEKEYS) -> [u32; 7] {
    [
        settings.cbSize,
        settings.dwFlags,
        settings.iMaxSpeed,
        settings.iTimeToMaxSpeed,
        settings.iCtrlSpeed,
        settings.dwReserved1,
        settings.dwReserved2,
    ]
}

fn mouse_keys_from_fields(fields: [u32; 7]) -> MOUSEKEYS {
    MOUSEKEYS {
        cbSize: fields[0],
        dwFlags: fields[1],
        iMaxSpeed: fields[2],
        iTimeToMaxSpeed: fields[3],
        iCtrlSpeed: fields[4],
        dwReserved1: fields[5],
        dwReserved2: fields[6],
    }
}

fn cursor_support_error(message: impl AsRef<str>) -> windows::core::Error {
    windows::core::Error::new(windows::Win32::Foundation::E_FAIL, message)
}

fn cursor_marker_path() -> windows::core::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| cursor_support_error("LOCALAPPDATA is unavailable for cursor recovery"))?;
    Ok(PathBuf::from(base)
        .join("BubblePlay")
        .join("headless-cursor-state.json"))
}

fn write_cursor_marker(path: &Path, record: &CursorRecoveryRecord) -> windows::core::Result<()> {
    let bytes =
        serde_json::to_vec(record).map_err(|error| cursor_support_error(error.to_string()))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| cursor_support_error(format!("claim cursor recovery marker: {error}")))?;
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(cursor_support_error(format!(
            "save cursor recovery marker: {error}"
        )));
    }
    Ok(())
}

fn create_cursor_marker(lease: MouseKeysLease) -> windows::core::Result<PathBuf> {
    let path = cursor_marker_path()?;
    std::fs::create_dir_all(path.parent().expect("fixed cursor marker has a parent"))
        .map_err(|error| cursor_support_error(error.to_string()))?;
    write_cursor_marker(&path, &CursorRecoveryRecord::new(std::process::id(), lease))?;
    Ok(path)
}

fn read_cursor_marker(path: &Path) -> windows::core::Result<Option<CursorRecoveryRecord>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(cursor_support_error(error.to_string())),
    };
    let mut bytes = Vec::new();
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|error| cursor_support_error(error.to_string()))?;
    if bytes.len() > 4096 {
        return Err(cursor_support_error("oversized cursor recovery marker"));
    }
    let record: CursorRecoveryRecord =
        serde_json::from_slice(&bytes).map_err(|error| cursor_support_error(error.to_string()))?;
    record.lease()?;
    Ok(Some(record))
}

fn remove_cursor_marker(path: &Path, owner_pid: u32) -> bool {
    match read_cursor_marker(path) {
        Ok(Some(record)) if record.owner_pid == owner_pid => {
            if let Err(error) = std::fs::remove_file(path) {
                warn!(?error, "could not remove restored cursor recovery marker");
                return false;
            }
            true
        }
        Ok(None) => true,
        Ok(Some(_)) => false,
        Err(error) => {
            warn!(
                ?error,
                "could not validate cursor recovery marker for removal"
            );
            false
        }
    }
}

/// Called by the tray only after a host exits. The dead-PID check is independent
/// of the tray's wait result, and a live or mismatched owner is left untouched.
pub fn restore_headless_cursor_after_exit(pid: u32) {
    match CursorSupportLock::acquire() {
        Ok(_lock) => recover_cursor_marker(Some(pid)),
        Err(error) => warn!(?error, "cursor recovery lifecycle lock unavailable"),
    }
}

/// Local tray-host startup recovery, including when no viewer is connected.
pub fn recover_headless_cursor_support() {
    match CursorSupportLock::acquire() {
        Ok(_lock) => recover_cursor_marker(None),
        Err(error) => warn!(?error, "cursor startup recovery lifecycle lock unavailable"),
    }
}

fn recover_cursor_marker(expected_owner: Option<u32>) {
    let result = (|| -> windows::core::Result<()> {
        let path = cursor_marker_path()?;
        let Some(record) = read_cursor_marker(&path)? else {
            return Ok(());
        };
        if expected_owner.is_some_and(|pid| pid != record.owner_pid)
            || cursor_owner_alive(record.owner_pid)
        {
            return Ok(());
        }
        let lease = record.lease()?;
        if let Some(restored) = mouse_keys_restoration(lease, read_mouse_keys()?) {
            write_mouse_keys(&restored)?;
        }
        if !remove_cursor_marker(&path, record.owner_pid) {
            return Err(cursor_support_error(
                "restored settings but could not remove recovery marker",
            ));
        }
        info!(
            owner_pid = record.owner_pid,
            "restored headless cursor support after host exit"
        );
        Ok(())
    })();
    if let Err(error) = result {
        // Keep the durable marker on failure for the next lifecycle attempt.
        warn!(
            ?error,
            "headless cursor recovery could not restore settings"
        );
    }
}

fn cursor_owner_alive(pid: u32) -> bool {
    match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(process) => {
            let exited = unsafe { WaitForSingleObject(process, 0) } == WAIT_OBJECT_0;
            let _ = unsafe { CloseHandle(process) };
            !exited
        }
        Err(error) => error.code().0 as u32 != (0x8007_0000 | ERROR_INVALID_PARAMETER.0),
    }
}

// Serialize lifecycle changes between a starting host and tray exit cleanup.
// A killed process automatically abandons this kernel mutex; no watcher or
// persistent lock file is required.
struct CursorSupportLock(HANDLE);

impl CursorSupportLock {
    fn acquire() -> windows::core::Result<Self> {
        let handle = unsafe {
            CreateMutexW(
                None,
                false,
                windows::core::w!("Local\\BubblePlay.HeadlessCursorSupport"),
            )
        }?;
        let wait = unsafe { WaitForSingleObject(handle, 250) };
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            return Ok(Self(handle));
        }
        let _ = unsafe { CloseHandle(handle) };
        Err(cursor_support_error("headless cursor lifecycle is busy"))
    }
}

impl Drop for CursorSupportLock {
    fn drop(&mut self) {
        let _ = unsafe { ReleaseMutex(self.0) };
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[derive(Default)]
struct HeadlessCursorState {
    active: bool,
    owned: Option<MouseKeysLease>,
}

impl HeadlessCursorState {
    #[cfg(test)]
    fn set_active_with(
        &mut self,
        active: bool,
        mouse_present: bool,
        read: impl FnMut() -> windows::core::Result<MOUSEKEYS>,
        write: impl FnMut(&MOUSEKEYS) -> windows::core::Result<()>,
    ) {
        if self.active == active {
            return;
        }
        self.apply_active_with(active, mouse_present, read, write);
    }

    fn apply_active_with(
        &mut self,
        active: bool,
        mouse_present: bool,
        mut read: impl FnMut() -> windows::core::Result<MOUSEKEYS>,
        mut write: impl FnMut(&MOUSEKEYS) -> windows::core::Result<()>,
    ) -> bool {
        self.active = active;
        if !active {
            self.restore_with(read, write);
            return self.owned.is_none();
        }
        if mouse_present || self.owned.is_some() {
            return true;
        }
        let original = match read() {
            Ok(settings) => settings,
            Err(error) => {
                warn!(?error, "headless cursor support could not read MouseKeys");
                return false;
            }
        };
        if original.dwFlags & MKF_MOUSEKEYSON != 0 {
            // The user owns an already-enabled feature; never disable it later.
            return true;
        }
        let mut applied = original;
        applied.dwFlags |= MKF_MOUSEKEYSON;
        match write(&applied) {
            Ok(()) => {
                self.owned = Some(MouseKeysLease { original, applied });
                info!("headless cursor support enabled MouseKeys for the active viewer");
                true
            }
            Err(error) => {
                warn!(?error, "headless cursor support could not enable MouseKeys");
                false
            }
        }
    }

    fn restore_with(
        &mut self,
        mut read: impl FnMut() -> windows::core::Result<MOUSEKEYS>,
        mut write: impl FnMut(&MOUSEKEYS) -> windows::core::Result<()>,
    ) {
        let Some(lease) = self.owned else {
            return;
        };
        let current = match read() {
            Ok(settings) => settings,
            Err(error) => {
                warn!(
                    ?error,
                    "headless cursor support could not read settings for restoration"
                );
                return;
            }
        };
        let Some(restored) = mouse_keys_restoration(lease, current) else {
            self.owned = None;
            return;
        };
        match write(&restored) {
            Ok(()) => {
                self.owned = None;
                info!("headless cursor support restored MouseKeys after the viewer");
            }
            Err(error) => warn!(
                ?error,
                "headless cursor support could not restore MouseKeys"
            ),
        }
    }
}

fn mouse_keys_restoration(lease: MouseKeysLease, current: MOUSEKEYS) -> Option<MOUSEKEYS> {
    if current.dwFlags & MKF_MOUSEKEYSON == 0 {
        return None; // The user already disabled it; preserve every current field.
    }
    if current == lease.applied {
        return Some(lease.original);
    }
    // Concurrent preference edits belong to the user. Restore only the ON bit
    // acquired by this guard, keeping their current speed and NumLock settings.
    let mut restored = current;
    restored.dwFlags &= !MKF_MOUSEKEYSON;
    Some(restored)
}

fn read_mouse_keys() -> windows::core::Result<MOUSEKEYS> {
    let mut settings = MOUSEKEYS {
        cbSize: std::mem::size_of::<MOUSEKEYS>() as u32,
        ..Default::default()
    };
    unsafe {
        SystemParametersInfoW(
            SPI_GETMOUSEKEYS,
            settings.cbSize,
            Some((&raw mut settings).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )?;
    }
    Ok(settings)
}

fn write_mouse_keys(settings: &MOUSEKEYS) -> windows::core::Result<()> {
    let mut settings = *settings;
    unsafe {
        SystemParametersInfoW(
            SPI_SETMOUSEKEYS,
            settings.cbSize,
            Some((&raw mut settings).cast()),
            // No SPIF_UPDATEINIFILE: the change is limited to this login session.
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
}

/// Stable native cursor semantics shared by browser and Android clients.
/// Custom Windows images deliberately fall back to the client's arrow.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorKind {
    Arrow = 0,
    Text = 1,
    Pointer = 2,
    Crosshair = 3,
    Move = 4,
    ResizeNS = 5,
    ResizeEW = 6,
    ResizeNESW = 7,
    ResizeNWSE = 8,
    Wait = 9,
    Progress = 10,
    NotAllowed = 11,
    Help = 12,
    UpArrow = 13,
    Hidden = 14,
}

impl CursorKind {
    pub fn encode(self) -> [u8; 2] {
        [TYPE_CURSOR_KIND, self as u8]
    }

    pub fn from_id(id: u8) -> Option<Self> {
        Some(match id {
            0 => Self::Arrow,
            1 => Self::Text,
            2 => Self::Pointer,
            3 => Self::Crosshair,
            4 => Self::Move,
            5 => Self::ResizeNS,
            6 => Self::ResizeEW,
            7 => Self::ResizeNESW,
            8 => Self::ResizeNWSE,
            9 => Self::Wait,
            10 => Self::Progress,
            11 => Self::NotAllowed,
            12 => Self::Help,
            13 => Self::UpArrow,
            14 => Self::Hidden,
            _ => return None,
        })
    }
}

fn classify_cursor(handle: usize, visible: bool, standard: &[usize; 14]) -> CursorKind {
    if !visible {
        return CursorKind::Hidden;
    }
    if handle != 0 {
        if let Some(index) = standard
            .iter()
            .position(|&candidate| candidate != 0 && candidate == handle)
        {
            return CursorKind::from_id(index as u8).unwrap_or(CursorKind::Arrow);
        }
    }
    CursorKind::Arrow
}

/// Desktop-bound standard handles are loaded once, with no allocation during
/// movement. A small replay heartbeat covers WSS joins and reconnects even
/// when the pointer is stationary and the client has not requested a sync.
pub struct CursorKindTracker {
    standard: [usize; 14],
    last_kind: Option<CursorKind>,
    last_emit: Instant,
    last_diagnostic: Option<((u32, usize, CursorKind), Instant)>,
    headless_support: HeadlessCursorSupport,
}

impl CursorKindTracker {
    pub fn new(now: Instant) -> Self {
        let ids = [
            IDC_ARROW,
            IDC_IBEAM,
            IDC_HAND,
            IDC_CROSS,
            IDC_SIZEALL,
            IDC_SIZENS,
            IDC_SIZEWE,
            IDC_SIZENESW,
            IDC_SIZENWSE,
            IDC_WAIT,
            IDC_APPSTARTING,
            IDC_NO,
            IDC_HELP,
            IDC_UPARROW,
        ];
        Self {
            standard: ids
                .map(|id| unsafe { LoadCursorW(None, id) }.map_or(0, |cursor| cursor.0 as usize)),
            last_kind: None,
            last_emit: now,
            last_diagnostic: None,
            headless_support: HeadlessCursorSupport::new(),
        }
    }

    pub fn reset(&mut self) {
        self.last_kind = None;
    }

    /// The last sampled cursor kind; `None` before the first sample after a reset.
    pub fn last_kind(&self) -> Option<CursorKind> {
        self.last_kind
    }

    pub fn set_active(&mut self, active: bool) {
        if self.headless_support.state.active != active {
            self.reset();
        }
        self.headless_support.set_active(active);
    }

    pub fn sample(&mut self, force: bool, now: Instant) -> Option<CursorKind> {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // A failed desktop query is not proof that Windows hid the pointer.
        if unsafe { GetCursorInfo(&raw mut info) }.is_err() {
            return None;
        }
        let kind = classify_cursor(
            info.hCursor.0 as usize,
            info.flags.0 & CURSOR_SHOWING.0 != 0,
            &self.standard,
        );
        let snapshot = (info.flags.0, info.hCursor.0 as usize, kind);
        if self.last_diagnostic.is_none_or(|(previous, at)| {
            previous != snapshot && now.saturating_duration_since(at) >= Duration::from_millis(250)
        }) {
            info!(
                flags = info.flags.0,
                handle_present = snapshot.1 != 0,
                standard_match = snapshot.1 != 0 && self.standard.contains(&snapshot.1),
                ?kind,
                mouse_present = unsafe { GetSystemMetrics(SM_MOUSEPRESENT) } != 0,
                "native cursor classification changed"
            );
            self.last_diagnostic = Some((snapshot, now));
        }
        self.observe(kind, force, now)
    }

    fn observe(&mut self, kind: CursorKind, force: bool, now: Instant) -> Option<CursorKind> {
        if !force
            && self.last_kind == Some(kind)
            && now.saturating_duration_since(self.last_emit) < Duration::from_secs(1)
        {
            return None;
        }
        self.last_kind = Some(kind);
        self.last_emit = now;
        Some(kind)
    }
}

#[derive(Debug, PartialEq)]
enum Command {
    Key(u16, bool),
    Rel(i16, i16),
    Abs(u16, u16),
    Buttons(u8),
    Wheel(i16),
    ReleaseAll,
    Motion(u16, i16, i16, i16, i16),
    Absolute(u16, u16, u16),
    ButtonsAt(u16, u16, u16, u8),
    CursorSync(u16),
    Text(String),
    ScanKey(HeldKey, bool),
    Clipboard(ClipboardChunk),
}

fn release_only(command: Command, held_buttons: u8) -> Option<Command> {
    match command {
        Command::Key(_, true)
        | Command::ScanKey(_, true)
        | Command::ReleaseAll
        | Command::CursorSync(_) => Some(command),
        Command::Buttons(mask) | Command::ButtonsAt(_, _, _, mask) => {
            Some(Command::Buttons(held_buttons & mask))
        }
        _ => None,
    }
}

/// Held input carried through a monitor switch. While presentation is gated,
/// remember a release instead of forwarding it to an unavailable desktop.
/// Resume releases both input backends before any new pointer movement.
#[derive(Default)]
pub struct CarriedInput {
    keys: BTreeSet<HeldKey>,
    buttons: u8,
    carrying: bool,
    release_pending: bool,
}

impl CarriedInput {
    pub fn begin_switch(&mut self, carry: bool) {
        if carry {
            self.carrying = true;
        } else {
            self.reset();
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Observe only packets admitted by the producer's input gate.
    pub fn observe_forwarded(&mut self, data: &[u8]) {
        match decode(data) {
            Some(Command::Key(vk, up)) => self.key(HeldKey::Vk(vk), up),
            Some(Command::ScanKey(key, up)) => self.key(key, up),
            Some(Command::Buttons(mask) | Command::ButtonsAt(_, _, _, mask)) => {
                self.buttons = mask;
            }
            Some(Command::ReleaseAll) => self.reset(),
            _ => {}
        }
    }

    fn key(&mut self, key: HeldKey, up: bool) {
        if up {
            self.keys.remove(&key);
        } else {
            self.keys.insert(key);
        }
    }

    pub fn observe_gated(&mut self, data: &[u8]) {
        if !self.carrying {
            return;
        }
        let releases_held_input = match decode(data) {
            Some(Command::Key(vk, true)) => self.keys.contains(&HeldKey::Vk(vk)),
            Some(Command::ScanKey(key, true)) => self.keys.contains(&key),
            Some(Command::Buttons(mask) | Command::ButtonsAt(_, _, _, mask)) => {
                self.buttons & !mask != 0
            }
            Some(Command::ReleaseAll) => !self.keys.is_empty() || self.buttons != 0,
            _ => false,
        };
        self.release_pending |= releases_held_input;
    }

    /// A true result must release both sinks before processing resumed input.
    /// Releasing all ends a carried drag when any carried key/button was let
    /// go, including when the backend could not accept input during the gate.
    pub fn take_resume_release(&mut self, capture_available: bool, input_ready: bool) -> bool {
        if !capture_available || !input_ready {
            return false;
        }
        self.carrying = false;
        if self.release_pending {
            self.reset();
            true
        } else {
            false
        }
    }
}

/// One physical key as the host injected it. Scan-code keys and VK-only keys
/// are tracked separately so release-all repeats exactly what was pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HeldKey {
    Vk(u16),
    Scan { code: u8, extended: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClipboardChunk {
    paste: bool,
    id: u16,
    index: u16,
    count: u16,
    bytes: Vec<u8>,
}

fn decode(data: &[u8]) -> Option<Command> {
    match *data.first()? {
        TYPE_KEY_DOWN if data.len() == 3 => {
            Some(Command::Key(u16::from_le_bytes([data[1], data[2]]), false))
        }
        TYPE_KEY_UP if data.len() == 3 => {
            Some(Command::Key(u16::from_le_bytes([data[1], data[2]]), true))
        }
        TYPE_MOUSE_REL if data.len() == 5 => Some(Command::Rel(
            i16::from_le_bytes([data[1], data[2]]),
            i16::from_le_bytes([data[3], data[4]]),
        )),
        TYPE_MOUSE_ABS if data.len() == 5 => Some(Command::Abs(
            u16::from_le_bytes([data[1], data[2]]),
            u16::from_le_bytes([data[3], data[4]]),
        )),
        TYPE_BUTTONS if data.len() == 2 => Some(Command::Buttons(data[1])),
        TYPE_WHEEL if data.len() == 3 => {
            Some(Command::Wheel(i16::from_le_bytes([data[1], data[2]])))
        }
        TYPE_RELEASE_ALL if data.len() == 1 => Some(Command::ReleaseAll),
        TYPE_MOTION_V2 if data.len() == 11 => Some(Command::Motion(
            u16::from_le_bytes([data[1], data[2]]),
            i16::from_le_bytes([data[3], data[4]]),
            i16::from_le_bytes([data[5], data[6]]),
            i16::from_le_bytes([data[7], data[8]]),
            i16::from_le_bytes([data[9], data[10]]),
        )),
        TYPE_ABSOLUTE_V2 if data.len() == 7 => Some(Command::Absolute(
            u16::from_le_bytes([data[1], data[2]]),
            u16::from_le_bytes([data[3], data[4]]),
            u16::from_le_bytes([data[5], data[6]]),
        )),
        TYPE_BUTTONS_AT if data.len() == 8 => Some(Command::ButtonsAt(
            u16::from_le_bytes([data[1], data[2]]),
            u16::from_le_bytes([data[3], data[4]]),
            u16::from_le_bytes([data[5], data[6]]),
            data[7],
        )),
        TYPE_CURSOR_SYNC if data.len() == 3 => {
            Some(Command::CursorSync(u16::from_le_bytes([data[1], data[2]])))
        }
        TYPE_TEXT if (2..=MAX_TEXT_BYTES + 1).contains(&data.len()) => {
            String::from_utf8(data[1..].to_vec())
                .ok()
                .map(Command::Text)
        }
        TYPE_KEY_SCAN if data.len() == 6 => {
            let flags = data[1];
            let scan = u16::from_le_bytes([data[2], data[3]]);
            let vk = u16::from_le_bytes([data[4], data[5]]);
            if flags & !(SCAN_FLAG_UP | SCAN_FLAG_EXTENDED) != 0 || scan > 0xFF {
                return None;
            }
            let key = match (scan, vk) {
                (0, 0) => return None,
                (0, vk) if vk <= 0xFF => HeldKey::Vk(vk),
                (0, _) => return None,
                (code, _) => HeldKey::Scan {
                    code: code as u8,
                    extended: flags & SCAN_FLAG_EXTENDED != 0,
                },
            };
            Some(Command::ScanKey(key, flags & SCAN_FLAG_UP != 0))
        }
        TYPE_CLIPBOARD if (8..=8 + CLIPBOARD_CHUNK_BYTES).contains(&data.len()) => {
            let flags = data[1];
            let id = u16::from_le_bytes([data[2], data[3]]);
            let index = u16::from_le_bytes([data[4], data[5]]);
            let count = u16::from_le_bytes([data[6], data[7]]);
            if flags & !CLIPBOARD_FLAG_PASTE != 0
                || count == 0
                || count > CLIPBOARD_MAX_CHUNKS
                || index >= count
            {
                return None;
            }
            Some(Command::Clipboard(ClipboardChunk {
                paste: flags & CLIPBOARD_FLAG_PASTE != 0,
                id,
                index,
                count,
                bytes: data[8..].to_vec(),
            }))
        }
        _ => None,
    }
}

pub(crate) fn newer_sequence(last: Option<u16>, next: u16) -> bool {
    last.map(|old| {
        let delta = next.wrapping_sub(old);
        delta != 0 && delta < 0x8000
    })
    .unwrap_or(true)
}

/// Coalesce only admitted producer input. Validate every original sequence,
/// including packets eliminated by coalescing, before accumulating deltas.
/// Different motion kinds and every control packet are ordering barriers.
#[derive(Default)]
pub struct InputPacketCoalescer {
    last_sequence: Option<u16>,
}

impl InputPacketCoalescer {
    pub fn observe_release_only(&mut self, data: &[u8]) {
        if matches!(
            decode(data),
            Some(Command::ButtonsAt(..) | Command::CursorSync(..))
        ) {
            self.observe(data);
        }
    }

    pub fn observe(&mut self, data: &[u8]) {
        let sequence = match decode(data) {
            Some(
                Command::Motion(seq, ..)
                | Command::Absolute(seq, ..)
                | Command::ButtonsAt(seq, ..)
                | Command::CursorSync(seq),
            ) => seq,
            _ => return,
        };
        if newer_sequence(self.last_sequence, sequence) {
            self.last_sequence = Some(sequence);
        }
    }

    pub fn coalesce(&mut self, packets: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut output = Vec::with_capacity(packets.len());
        let mut pending: Option<(Vec<u8>, Option<u16>)> = None;
        for packet in packets {
            let command = decode(&packet);
            let sequence = match command {
                Some(Command::Motion(seq, ..) | Command::Absolute(seq, ..)) => Some(seq),
                _ => None,
            };
            let motion = matches!(
                command,
                Some(
                    Command::Rel(..)
                        | Command::Abs(..)
                        | Command::Wheel(..)
                        | Command::Motion(..)
                        | Command::Absolute(..)
                )
            );
            let before = self.last_sequence;
            let accepted = sequence.is_none_or(|seq| newer_sequence(before, seq));
            if motion && accepted {
                self.observe(&packet);
                if let Some((previous, anchor)) = pending.as_mut() {
                    // The merged sequence must still be acceptable from the
                    // watermark before the run, even across a long/wrapped run.
                    if sequence.is_none_or(|seq| newer_sequence(*anchor, seq))
                        && merge_motion_packet(previous, &packet)
                    {
                        continue;
                    }
                }
                if let Some((previous, _)) = pending.take() {
                    output.push(previous);
                }
                pending = Some((packet, before));
            } else {
                if let Some((previous, _)) = pending.take() {
                    output.push(previous);
                }
                self.observe(&packet);
                // Keep stale and malformed packets for the sink's existing
                // rejection/accounting path; never add their displacement.
                output.push(packet);
            }
        }
        if let Some((previous, _)) = pending {
            output.push(previous);
        }
        output
    }
}

fn merge_motion_packet(previous: &mut Vec<u8>, next: &[u8]) -> bool {
    if previous[0] != next[0] {
        return false;
    }
    // Wheel deltas are additive events, with each original sequence as its
    // replay identity. Merging them would erase identities needed to reject
    // duplicate arrivals while accepting late wheels past a pointer barrier.
    if matches!(decode(previous), Some(Command::Motion(_, _, _, wx, wy)) if wx != 0 || wy != 0)
        || matches!(decode(next), Some(Command::Motion(_, _, _, wx, wy)) if wx != 0 || wy != 0)
    {
        return false;
    }
    let (start, end) = match decode(next) {
        Some(Command::Abs(..) | Command::Absolute(..)) => {
            previous.copy_from_slice(next);
            return true;
        }
        Some(Command::Rel(..)) => (1, 5),
        Some(Command::Wheel(..)) => (1, 3),
        Some(Command::Motion(..)) => (3, 11),
        _ => return false,
    };
    let mut merged = previous.clone();
    for offset in (start..end).step_by(2) {
        let a = i16::from_le_bytes([previous[offset], previous[offset + 1]]);
        let b = i16::from_le_bytes([next[offset], next[offset + 1]]);
        let Some(sum) = a.checked_add(b) else {
            // Flush before overflow: splitting retains all displacement and
            // distinct accepted sequences without clamping or duplication.
            return false;
        };
        merged[offset..offset + 2].copy_from_slice(&sum.to_le_bytes());
    }
    if start == 3 {
        merged[1..3].copy_from_slice(&next[1..3]);
    }
    *previous = merged;
    true
}

#[cfg(test)]
mod packet_coalescing_tests {
    use super::*;

    fn recording_sink() -> InputSink {
        let mut sink = InputSink::new();
        sink.test_injections = Some(Vec::new());
        sink
    }

    fn injected_wheels(sink: &InputSink) -> Vec<(u32, i32)> {
        sink.test_injections
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|record| match record {
                Record::Mouse { data, flags, .. }
                    if *flags == MOUSEEVENTF_WHEEL.0 || *flags == MOUSEEVENTF_HWHEEL.0 =>
                {
                    Some((*flags, *data as i32))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn fresh_client_interleaving_after_warm_viewer_disconnect_injects_every_wheel() {
        // Both real senders allocate 0,1,... from one counter, incrementing
        // after each successful absolute/motion/buttons-at send. These traces
        // are also asserted against InputController and RemoteInput in their
        // client suites. A previous viewer must not poison that fresh counter.
        for client in ["browser", "android"] {
            let mut sink = recording_sink();
            let mut coalescer = InputPacketCoalescer::default();
            let previous = packet(TYPE_ABSOLUTE_V2, &[24_000, 100, 200]);
            coalescer.observe(&previous);
            sink.handle(&previous);
            // Only a real viewer replacement resets the producer watermark.
            sink.reset_viewer_session();
            coalescer = InputPacketCoalescer::default();
            sink.take_diagnostics();
            sink.test_injections.as_mut().unwrap().clear();
            let fixture: serde_json::Value = serde_json::from_str(include_str!(
                "../../worker/test/fixtures/scroll-client-replay.json"
            ))
            .unwrap();
            let trace = fixture["packets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| {
                    let hex = entry["hex"].as_str().unwrap();
                    (0..hex.len())
                        .step_by(2)
                        .map(|offset| u8::from_str_radix(&hex[offset..offset + 2], 16).unwrap())
                        .collect()
                })
                .collect();
            for data in coalescer.coalesce(trace) {
                sink.handle(&data);
            }
            let counters = sink.take_diagnostics();
            assert_eq!(
                counters.drops_by_reason[InputDropReason::StaleSequence as usize],
                0,
                "{client}"
            );
            assert_eq!(
                injected_wheels(&sink),
                vec![
                    (MOUSEEVENTF_HWHEEL.0, -30),
                    (MOUSEEVENTF_WHEEL.0, 120),
                    (MOUSEEVENTF_HWHEEL.0, 60),
                    (MOUSEEVENTF_WHEEL.0, -240),
                    (MOUSEEVENTF_WHEEL.0, 15),
                ],
                "{client}"
            );
            assert_eq!(sink.state.last_motion_sequence, Some(6));
        }
    }

    #[test]
    fn late_wheel_survives_pointer_barrier_but_duplicate_does_not_scroll_twice() {
        let mut sink = recording_sink();
        let mut coalescer = InputPacketCoalescer::default();
        let trace = vec![
            packet(TYPE_ABSOLUTE_V2, &[10, 1000, 2000]),
            motion(11, 3, 4, 0, 120),
            motion(12, 5, 6, 0, 240),
            vec![TYPE_BUTTONS_AT, 14, 0, 232, 3, 208, 7, 0],
            motion(13, 90, 90, -60, -120),
            motion(11, 3, 4, 0, 120),
            motion(13, 90, 90, -60, -120),
            packet(TYPE_ABSOLUTE_V2, &[12, 500, 600]),
        ];
        for data in coalescer.coalesce(trace) {
            sink.handle(&data);
        }
        assert_eq!(sink.state.last_motion_sequence, Some(14));
        assert_eq!(sink.state.last_absolute_position, Some((1000, 2000)));
        assert_eq!(
            injected_wheels(&sink),
            vec![
                (MOUSEEVENTF_WHEEL.0, 120),
                (MOUSEEVENTF_WHEEL.0, 240),
                (MOUSEEVENTF_HWHEEL.0, -60),
                (MOUSEEVENTF_WHEEL.0, -120),
            ]
        );
    }

    fn packet(kind: u8, values: &[i16]) -> Vec<u8> {
        let mut packet = vec![kind];
        for value in values {
            packet.extend_from_slice(&value.to_le_bytes());
        }
        packet
    }

    fn motion(seq: u16, dx: i16, dy: i16, wx: i16, wy: i16) -> Vec<u8> {
        packet(TYPE_MOTION_V2, &[seq as i16, dx, dy, wx, wy])
    }

    #[test]
    fn coalescing_sums_displacement_and_wheels_without_overflow_loss() {
        let mut coalescer = InputPacketCoalescer::default();
        let output = coalescer.coalesce(vec![
            motion(1, 20_000, -5, 7, 120),
            motion(2, 20_000, 8, -3, -40),
            motion(3, -5, 2, 1, 20),
        ]);
        assert_eq!(output.len(), 3);
        assert_eq!(
            decode(&output[0]),
            Some(Command::Motion(1, 20_000, -5, 7, 120))
        );
        assert_eq!(
            decode(&output[1]),
            Some(Command::Motion(2, 20_000, 8, -3, -40))
        );
        assert_eq!(decode(&output[2]), Some(Command::Motion(3, -5, 2, 1, 20)));
        let legacy = coalescer.coalesce(vec![
            packet(TYPE_MOUSE_REL, &[3, 5]),
            packet(TYPE_MOUSE_REL, &[-1, 2]),
            packet(TYPE_WHEEL, &[120]),
            packet(TYPE_WHEEL, &[-40]),
        ]);
        assert_eq!(
            legacy,
            vec![packet(TYPE_MOUSE_REL, &[2, 7]), packet(TYPE_WHEEL, &[80])]
        );
    }

    #[test]
    fn coalescing_keeps_controls_and_mixed_absolute_relative_order() {
        let controls = vec![
            vec![TYPE_BUTTONS, 1],
            packet(TYPE_KEY_DOWN, &[65]),
            vec![TYPE_TEXT, b'x'],
            vec![TYPE_RELEASE_ALL],
            packet(TYPE_CURSOR_SYNC, &[3]),
        ];
        for control in controls {
            let mut coalescer = InputPacketCoalescer::default();
            let a = packet(TYPE_ABSOLUTE_V2, &[1, 100, 200]);
            let b = packet(TYPE_ABSOLUTE_V2, &[2, 300, 400]);
            let c = motion(3, 5, 0, 0, 0);
            let d = packet(TYPE_ABSOLUTE_V2, &[4, 500, 600]);
            let e = packet(TYPE_ABSOLUTE_V2, &[5, 700, 800]);
            assert_eq!(
                coalescer.coalesce(vec![a, b.clone(), c.clone(), control.clone(), d, e.clone()]),
                vec![b, c, control, e]
            );
        }
        let click = vec![TYPE_BUTTONS_AT, 10, 0, 100, 0, 200, 0, 1];
        let mut coalescer = InputPacketCoalescer::default();
        let after = motion(11, 9, 0, 0, 0);
        assert_eq!(
            coalescer.coalesce(vec![motion(9, 3, 0, 0, 0), click.clone(), after.clone()]),
            vec![motion(9, 3, 0, 0, 0), click, after]
        );
    }

    #[test]
    fn coalescing_checks_each_sequence_across_batches_and_wrap() {
        let mut coalescer = InputPacketCoalescer::default();
        coalescer.observe(&motion(u16::MAX - 1, 0, 0, 0, 0));
        let output = coalescer.coalesce(vec![motion(u16::MAX, 2, 0, 0, 0), motion(0, 3, 0, 0, 0)]);
        assert_eq!(output, vec![motion(0, 5, 0, 0, 0)]);
        let output = coalescer.coalesce(vec![
            motion(u16::MAX, 100, 0, 0, 0),
            motion(0, 100, 0, 0, 0),
            motion(1, 4, 0, 0, 0),
            motion(2, 6, 0, 0, 0),
        ]);
        let mut sink_state = InputState {
            last_motion_sequence: Some(0),
            ..Default::default()
        };
        let mut displacement = 0;
        for packet in output {
            if let Some(Command::Motion(seq, dx, ..)) = decode(&packet) {
                if sink_state.accept_motion(seq) {
                    displacement += i32::from(dx);
                }
            }
        }
        assert_eq!(displacement, 10);
        assert_eq!(sink_state.last_motion_sequence, Some(2));
        coalescer.observe_release_only(&packet(TYPE_CURSOR_SYNC, &[10]));
        assert_eq!(coalescer.last_sequence, Some(10));
        coalescer.observe_release_only(&motion(11, 2, 0, 0, 0));
        assert_eq!(coalescer.last_sequence, Some(10));
    }

    #[test]
    fn coalescing_splits_sequence_windows_and_preserves_stale_rejection() {
        let mut coalescer = InputPacketCoalescer {
            last_sequence: Some(0),
        };
        let output = coalescer.coalesce(vec![
            motion(1, 1, 0, 0, 0),
            motion(16_384, 1, 0, 0, 0),
            motion(32_767, 1, 0, 0, 0),
            motion(32_768, 1, 0, 0, 0),
            motion(40_000, 1, 0, 0, 0),
        ]);
        assert_eq!(
            output,
            vec![motion(32_767, 3, 0, 0, 0), motion(40_000, 2, 0, 0, 0)]
        );
        let rejected = motion(40_000u16.wrapping_add(0x8000), 100, 0, 0, 0);
        let accepted = motion(40_001, 5, 0, 0, 0);
        assert_eq!(
            coalescer.coalesce(vec![rejected.clone(), accepted.clone()]),
            vec![rejected, accepted]
        );
    }

    #[test]
    fn cancelled_motion_reaches_the_sink_and_requests_cursor_acknowledgement() {
        let mut coalescer = InputPacketCoalescer::default();
        let packets = coalescer.coalesce(vec![motion(1, 5, 0, 0, 0), motion(2, -5, 0, 0, 0)]);
        assert_eq!(packets, vec![motion(2, 0, 0, 0, 0)]);
        let mut sink = InputSink::new();
        let outcome = sink.handle(&packets[0]);
        assert_eq!(outcome.accepted_sequence, Some(2));
        assert_eq!(outcome.application.requested, 0);
        // These are the production host (main.rs) decisions to sample/send
        // the cursor, not just packet decoding.
        assert!(outcome.cursor_feedback_requested());
        assert!(outcome.cursor_changed || outcome.sync_requested || outcome.feedback_reliable);
        let stale = sink.handle(&packets[0]);
        assert_eq!(stale.accepted_sequence, None);
        assert!(!stale.cursor_feedback_requested());
    }

    #[test]
    fn fully_cancelled_motion_and_wheels_still_acknowledge_sequence_wrap() {
        let mut coalescer = InputPacketCoalescer::default();
        let packets = coalescer.coalesce(vec![
            motion(u16::MAX, 5, -2, 120, -120),
            motion(0, -5, 2, -120, 120),
        ]);
        assert_eq!(packets.len(), 2, "wheel identities survive coalescing");
        let mut sink = recording_sink();
        sink.handle(&packets[0]);
        let outcome = sink.handle(&packets[1]);
        assert_eq!(outcome.accepted_sequence, Some(0));
        assert_eq!(outcome.application.requested, 3);
        assert!(outcome.cursor_feedback_requested());
        assert!(outcome.cursor_changed || outcome.sync_requested || outcome.feedback_reliable);
        assert_eq!(
            injected_wheels(&sink),
            vec![
                (MOUSEEVENTF_HWHEEL.0, 120),
                (MOUSEEVENTF_WHEEL.0, -120),
                (MOUSEEVENTF_HWHEEL.0, -120),
                (MOUSEEVENTF_WHEEL.0, 120),
            ]
        );
    }

    #[test]
    fn wheel_replay_window_wraps_and_expires_ambiguous_sequences() {
        let mut window = WheelSequenceWindow::default();
        assert!(window.accept(u16::MAX - 1));
        window.observe(0);
        assert!(window.accept(u16::MAX));
        assert!(!window.accept(u16::MAX));
        assert!(window.accept(0));
        assert!(!window.accept(0x8000));
        window.observe(0x7fff);
        window.observe(0xfffe);
        assert!(
            window.accept(0xfffe),
            "same numerical sequence in the next cycle is fresh"
        );
        assert!(!window.accept(0xfffe));
    }

    #[test]
    fn ordinary_release_retains_pointer_and_wheel_replay_barriers() {
        let mut sink = recording_sink();
        sink.handle(&motion(10, 0, 0, 0, 120));
        sink.handle(&[TYPE_RELEASE_ALL]);
        sink.handle(&packet(TYPE_CURSOR_SYNC, &[12]));
        let duplicate = sink.handle(&motion(10, 90, 90, 0, 120));
        assert_eq!(duplicate.application.requested, 0);
        let late = sink.handle(&motion(11, 90, 90, 0, -240));
        assert_eq!(late.accepted_sequence, None);
        assert!(!late.cursor_changed);
        assert_eq!(late.application.applied, 1);
        assert_eq!(sink.state.last_motion_sequence, Some(12));
        assert_eq!(
            injected_wheels(&sink),
            vec![(MOUSEEVENTF_WHEEL.0, 120), (MOUSEEVENTF_WHEEL.0, -240),]
        );
    }
}

pub struct InputSink {
    state: InputState,
    stats: InputStats,
    absolute_region: DesktopRegion,
    failure_log: InputFailureLogGate,
    clipboard: Option<ClipboardSync>,
    /// Admin mode: the elevated agent injects on our behalf when reachable.
    admin_agent: Option<AdminAgentLink>,
    secure_helper:
        Option<std::sync::Arc<std::sync::Mutex<crate::secure_helper::client::SecureHelperLink>>>,
    secure_desktop: bool,
    secure_input_allowed: bool,
    foreground_privilege: crate::desktop_session::ForegroundPrivilegeTracker,
    admin_enabled: bool,
    status_changes: InputStatusChanges,
    diagnostics: InputDiagnosticCounters,
    diagnostics_rollup: crate::input_diagnostics::DiagnosticsRollup,
    input_flow: InputFlowTransitions,
    geometry_valid: bool,
    /// Pixel offset added to absolute moves (and clamped to the desktop) while a window drag that
    /// crossed monitors is in progress; see `begin_drag_offset`.
    drag_offset: Option<(i32, i32)>,
    diagnostics_foreground_pid: Option<u32>,
    development_agent: bool,
    #[cfg(test)]
    test_injections: Option<Vec<Record>>,
}

/// Clipboard state exists only where the sink runs as the interactive user
/// (the local host). Elsewhere clipboard packets are
/// ignored.
struct ClipboardSync {
    assembly: ClipboardAssembly,
    worker: Option<ClipboardWorker>,
    epoch: u64,
    next_write: u64,
    poll_pending: bool,
    reset_pending: bool,
    content: Option<clipboard::ClipboardContent>,
    paste: Option<PendingClipboardPaste>,
}

struct PendingClipboardPaste {
    id: u64,
    foreground: (isize, u32),
    expires: Instant,
}

fn foreground_identity() -> (isize, u32) {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let window = unsafe { GetForegroundWindow() };
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    (window.0 as isize, pid)
}

impl Default for ClipboardSync {
    fn default() -> Self {
        let worker = match ClipboardWorker::spawn() {
            Ok(worker) => Some(worker),
            Err(error) => {
                warn!(%error, "clipboard worker unavailable; ordinary input remains enabled");
                None
            }
        };
        Self::with_worker(worker)
    }
}

impl ClipboardSync {
    fn with_worker(worker: Option<ClipboardWorker>) -> Self {
        Self {
            assembly: ClipboardAssembly::default(),
            worker,
            epoch: 0,
            next_write: 0,
            poll_pending: false,
            reset_pending: true,
            content: None,
            paste: None,
        }
    }

    fn reset(&mut self) {
        self.assembly = ClipboardAssembly::default();
        self.epoch = self.epoch.wrapping_add(1);
        if let Some(worker) = self.worker.as_ref() {
            worker.set_epoch(self.epoch);
        }
        self.poll_pending = false;
        self.reset_pending = true;
        self.content = None;
        self.paste = None;
    }
}

#[derive(Debug, Default)]
struct InputFailureLogGate {
    next_log: Option<Instant>,
    suppressed: u64,
}

impl InputFailureLogGate {
    fn admit(&mut self, now: Instant) -> Option<u64> {
        if self.next_log.is_none_or(|next| now >= next) {
            self.next_log = Some(now + INPUT_FAILURE_LOG_INTERVAL);
            return Some(std::mem::take(&mut self.suppressed));
        }
        self.suppressed = self.suppressed.saturating_add(1);
        None
    }
}

/// Pixel bounds in the Windows virtual desktop coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl DesktopRegion {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn sanitized(self) -> Self {
        Self {
            width: self.width.max(1),
            height: self.height.max(1),
            ..self
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InputStats {
    pub relative_packets: u64,
    pub absolute_packets: u64,
    pub stale_motion_dropped: u64,
    pub cursor_sync_requests: u64,
}

impl InputStats {
    pub fn is_empty(self) -> bool {
        self.relative_packets == 0
            && self.absolute_packets == 0
            && self.stale_motion_dropped == 0
            && self.cursor_sync_requests == 0
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InputOutcome {
    pub accepted_sequence: Option<u16>,
    pub cursor_changed: bool,
    pub sync_requested: bool,
    pub feedback_reliable: bool,
    pub application: InputApplication,
}

impl InputOutcome {
    /// An accepted sequence needs a sampled acknowledgement even if motion
    /// coalesced to zero or Windows did not move the pointer.
    pub fn cursor_feedback_requested(self) -> bool {
        self.accepted_sequence.is_some()
            || self.cursor_changed
            || self.sync_requested
            || self.feedback_reliable
    }
}

/// Unlike ordinary relative motion, this intentional zero move must survive
/// coalescing/no-op filtering so Windows receives a display wake stimulus.
fn display_wake_input() -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: MOUSEEVENTF_MOVE,
                ..Default::default()
            },
        },
    }
}

/// What Windows accepted at the `SendInput` boundary for one browser packet.
/// Packet parsing and motion sequencing are intentionally tracked elsewhere:
/// neither is proof that Windows applied the requested input.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InputApplication {
    pub requested: u32,
    pub applied: u32,
    pub failed_calls: u32,
    pub last_error_code: Option<i32>,
}

impl InputApplication {
    fn observe(&mut self, requested: u32, applied: u32, error_code: i32) {
        self.requested = self.requested.saturating_add(requested);
        self.applied = self.applied.saturating_add(applied.min(requested));
        if applied != requested {
            self.failed_calls = self.failed_calls.saturating_add(1);
            self.last_error_code = Some(error_code);
        }
    }

    fn merge(&mut self, other: Self) {
        self.requested = self.requested.saturating_add(other.requested);
        self.applied = self.applied.saturating_add(other.applied);
        self.failed_calls = self.failed_calls.saturating_add(other.failed_calls);
        if other.last_error_code.is_some() {
            self.last_error_code = other.last_error_code;
        }
    }

    pub fn succeeded(self) -> bool {
        self.requested > 0 && self.requested == self.applied && self.failed_calls == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorState {
    pub acknowledged_sequence: Option<u16>,
    pub x: u16,
    pub y: u16,
    pub in_bounds: bool,
    pub visible: bool,
}

impl CursorState {
    pub fn encode(self) -> [u8; 8] {
        let mut data = [0u8; 8];
        data[0] = TYPE_CURSOR_STATE;
        data[1..3].copy_from_slice(&self.acknowledged_sequence.unwrap_or(0).to_le_bytes());
        data[3..5].copy_from_slice(&self.x.to_le_bytes());
        data[5..7].copy_from_slice(&self.y.to_le_bytes());
        data[7] = (u8::from(self.acknowledged_sequence.is_some()) * CURSOR_ACK_VALID)
            | (u8::from(self.in_bounds) * CURSOR_IN_BOUNDS)
            | (u8::from(self.visible) * CURSOR_VISIBLE);
        data
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CursorSnapshot {
    x: u16,
    y: u16,
    in_bounds: bool,
    visible: bool,
}

pub struct CursorTracker {
    region: DesktopRegion,
    last_snapshot: Option<CursorSnapshot>,
    last_emit: Instant,
}

impl CursorTracker {
    pub fn region(&self) -> DesktopRegion {
        self.region
    }

    pub fn new(width: u32, height: u32, now: Instant) -> Self {
        Self::for_region(DesktopRegion::new(0, 0, width, height), now)
    }

    pub fn for_region(region: DesktopRegion, now: Instant) -> Self {
        Self {
            region: region.sanitized(),
            last_snapshot: None,
            last_emit: now.checked_sub(CURSOR_HEARTBEAT).unwrap_or(now),
        }
    }

    pub fn sample(
        &mut self,
        acknowledged_sequence: Option<u16>,
        now: Instant,
    ) -> Option<CursorState> {
        let mut point = POINT::default();
        if unsafe { GetCursorPos(&raw mut point) }.is_err() {
            warn!("GetCursorPos failed while sampling authoritative cursor");
            return None;
        }
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        let visible =
            unsafe { GetCursorInfo(&raw mut info) }.is_ok() && info.flags.0 & CURSOR_SHOWING.0 != 0;
        let snapshot = cursor_snapshot(point.x, point.y, self.region, visible);
        let changed = self.last_snapshot != Some(snapshot);
        if acknowledged_sequence.is_none()
            && !changed
            && now.saturating_duration_since(self.last_emit) < CURSOR_HEARTBEAT
        {
            return None;
        }
        self.last_snapshot = Some(snapshot);
        self.last_emit = now;
        Some(CursorState {
            acknowledged_sequence,
            x: snapshot.x,
            y: snapshot.y,
            in_bounds: snapshot.in_bounds,
            visible: snapshot.visible,
        })
    }
}

fn normalize_cursor_axis(value: i32, extent: i32) -> u16 {
    if extent <= 1 {
        return 0;
    }
    let value = value.clamp(0, extent - 1) as u64;
    ((value * u16::MAX as u64 + ((extent - 1) as u64 / 2)) / (extent - 1) as u64) as u16
}

fn cursor_snapshot(x: i32, y: i32, region: DesktopRegion, visible: bool) -> CursorSnapshot {
    let region = region.sanitized();
    let relative_x = x.saturating_sub(region.x);
    let relative_y = y.saturating_sub(region.y);
    let width = region.width.min(i32::MAX as u32) as i32;
    let height = region.height.min(i32::MAX as u32) as i32;
    CursorSnapshot {
        x: normalize_cursor_axis(relative_x, width),
        y: normalize_cursor_axis(relative_y, height),
        in_bounds: relative_x >= 0 && relative_y >= 0 && relative_x < width && relative_y < height,
        visible,
    }
}

fn normalized_axis_to_pixel(value: u16, origin: i32, extent: u32) -> i32 {
    let extent = extent.max(1);
    let offset = if extent <= 1 {
        0
    } else {
        ((value as u64 * (extent - 1) as u64 + u16::MAX as u64 / 2) / u16::MAX as u64) as i64
    };
    (origin as i64 + offset).clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

fn pixel_to_normalized_axis(pixel: i32, origin: i32, extent: u32) -> u16 {
    let extent = extent.max(1);
    if extent <= 1 {
        return 0;
    }
    let relative = (pixel as i64 - origin as i64).clamp(0, (extent - 1) as i64) as u64;
    ((relative * u16::MAX as u64 + (extent - 1) as u64 / 2) / (extent - 1) as u64) as u16
}

fn map_monitor_absolute_to_virtual(
    x: u16,
    y: u16,
    monitor: DesktopRegion,
    virtual_desktop: DesktopRegion,
) -> (u16, u16) {
    let monitor = monitor.sanitized();
    let virtual_desktop = virtual_desktop.sanitized();
    let pixel_x = normalized_axis_to_pixel(x, monitor.x, monitor.width);
    let pixel_y = normalized_axis_to_pixel(y, monitor.y, monitor.height);
    (
        pixel_to_normalized_axis(pixel_x, virtual_desktop.x, virtual_desktop.width),
        pixel_to_normalized_axis(pixel_y, virtual_desktop.y, virtual_desktop.height),
    )
}

/// Like [`map_monitor_absolute_to_virtual`], with the pixel shifted by `offset` and clamped to
/// the desktop (a window drag that crossed monitors keeps the cursor where it was carried to).
fn offset_absolute_to_virtual(
    x: u16,
    y: u16,
    monitor: DesktopRegion,
    virtual_desktop: DesktopRegion,
    offset: (i32, i32),
) -> (u16, u16) {
    let monitor = monitor.sanitized();
    let virtual_desktop = virtual_desktop.sanitized();
    let pixel_x = normalized_axis_to_pixel(x, monitor.x, monitor.width).saturating_add(offset.0);
    let pixel_y = normalized_axis_to_pixel(y, monitor.y, monitor.height).saturating_add(offset.1);
    (
        pixel_to_normalized_axis(pixel_x, virtual_desktop.x, virtual_desktop.width),
        pixel_to_normalized_axis(pixel_y, virtual_desktop.y, virtual_desktop.height),
    )
}

fn absolute_coordinates_for_input(
    secure: bool,
    x: u16,
    y: u16,
    monitor: DesktopRegion,
    normal_virtual: DesktopRegion,
) -> (u16, u16) {
    if secure {
        // Secure protocol v2 carries capture-region coordinates. Only the
        // desktop-bound SYSTEM worker knows the target virtual-screen geometry.
        (x, y)
    } else {
        map_monitor_absolute_to_virtual(x, y, monitor, normal_virtual)
    }
}

fn virtual_desktop_region() -> Option<DesktopRegion> {
    let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    (width > 0 && height > 0).then_some(DesktopRegion::new(x, y, width as u32, height as u32))
}

#[derive(Default)]
struct InputState {
    pressed_keys: BTreeSet<HeldKey>,
    /// Keys the host already released on the browser's behalf (a held Windows
    /// key during paste). Their eventual browser key-up must not be injected.
    swallowed_releases: BTreeSet<HeldKey>,
    buttons: u8,
    last_motion_sequence: Option<u16>,
    last_absolute_position: Option<(u16, u16)>,
    wheel_sequences: WheelSequenceWindow,
}

/// At-most-once wheel events within the wire's unambiguous u16 half-range.
/// Position/control watermarks advance this window without consuming wheels.
/// A stale pointer can therefore still carry an unseen, safely identified wheel.
struct WheelSequenceWindow {
    newest: Option<u16>,
    seen: [u64; 512],
}

impl Default for WheelSequenceWindow {
    fn default() -> Self {
        Self {
            newest: None,
            seen: [0; 512],
        }
    }
}

impl WheelSequenceWindow {
    fn observe(&mut self, sequence: u16) {
        if let Some(previous) = self.newest {
            if !newer_sequence(Some(previous), sequence) {
                return;
            }
            // The slot about to enter the window belonged to an event exactly
            // half a sequence cycle earlier. Clear it before reusing it.
            for step in 1..=sequence.wrapping_sub(previous) {
                let slot = usize::from(previous.wrapping_add(step) & 0x7fff);
                self.seen[slot / 64] &= !(1u64 << (slot % 64));
            }
        }
        self.newest = Some(sequence);
    }

    fn accept(&mut self, sequence: u16) -> bool {
        self.observe(sequence);
        if self.newest.unwrap().wrapping_sub(sequence) >= 0x8000 {
            // Older than half a cycle is indistinguishable from a future/reused
            // sequence. Applying such a wheel could replay previous distance.
            return false;
        }
        let slot = usize::from(sequence & 0x7fff);
        let bit = 1u64 << (slot % 64);
        let fresh = self.seen[slot / 64] & bit == 0;
        self.seen[slot / 64] |= bit;
        fresh
    }
}

fn is_control_key(key: HeldKey) -> bool {
    matches!(
        key,
        HeldKey::Vk(VK_CONTROL | VK_LCONTROL | VK_RCONTROL)
            | HeldKey::Scan {
                code: SCAN_LCONTROL,
                ..
            }
    )
}

fn is_windows_key(key: HeldKey) -> bool {
    matches!(
        key,
        HeldKey::Vk(VK_LWIN | VK_RWIN)
            | HeldKey::Scan {
                code: SCAN_LWIN | SCAN_RWIN,
                extended: true
            }
    )
}

impl InputState {
    /// Track a key transition; returns whether it should be injected.
    fn key(&mut self, key: HeldKey, up: bool) -> bool {
        if up {
            if self.swallowed_releases.remove(&key) {
                return false;
            }
            self.pressed_keys.remove(&key);
        } else {
            self.swallowed_releases.remove(&key);
            self.pressed_keys.insert(key);
        }
        true
    }

    fn take_release_plan(&mut self) -> (Vec<HeldKey>, u8) {
        let keys = std::mem::take(&mut self.pressed_keys).into_iter().collect();
        self.swallowed_releases.clear();
        let buttons = std::mem::replace(&mut self.buttons, 0);
        (keys, buttons)
    }

    /// Key transitions that paste the (already replaced) clipboard with
    /// Ctrl+V, respecting modifiers the browser user is physically holding.
    fn paste_plan(&mut self) -> Vec<(HeldKey, bool)> {
        let control_held = self.pressed_keys.iter().copied().any(is_control_key);
        let mut plan = self.release_paste_windows_keys();
        if !control_held {
            plan.push((HeldKey::Vk(VK_CONTROL), false));
        }
        plan.push((HeldKey::Vk(VK_V), false));
        plan.push((HeldKey::Vk(VK_V), true));
        if !control_held {
            plan.push((HeldKey::Vk(VK_CONTROL), true));
        }
        plan
    }

    /// Mask/release Meta before waiting for a clipboard write. A browser key-up
    /// arriving during that wait must not open Start and change the paste target.
    fn release_paste_windows_keys(&mut self) -> Vec<(HeldKey, bool)> {
        let windows_keys: Vec<HeldKey> = self
            .pressed_keys
            .iter()
            .copied()
            .filter(|key| is_windows_key(*key))
            .collect();
        let mut plan = Vec::with_capacity(8);
        if !windows_keys.is_empty() {
            plan.push((HeldKey::Vk(VK_MASK), false));
            plan.push((HeldKey::Vk(VK_MASK), true));
            for key in windows_keys {
                self.pressed_keys.remove(&key);
                self.swallowed_releases.insert(key);
                plan.push((key, true));
            }
        }
        plan
    }

    fn accept_motion(&mut self, sequence: u16) -> bool {
        if !newer_sequence(self.last_motion_sequence, sequence) {
            return false;
        }
        self.last_motion_sequence = Some(sequence);
        self.wheel_sequences.observe(sequence);
        true
    }

    fn advance_motion_barrier(&mut self, sequence: u16) -> bool {
        if newer_sequence(self.last_motion_sequence, sequence) {
            self.last_motion_sequence = Some(sequence);
            self.wheel_sequences.observe(sequence);
            true
        } else {
            false
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ClipboardProgress {
    Pending,
    Complete { text: String, paste: bool },
    Rejected,
}

/// Reassembles chunked browser clipboard text. Chunks arrive in order on a
/// reliable channel; anything else discards the transfer.
#[derive(Debug, Default)]
struct ClipboardAssembly {
    active: bool,
    id: u16,
    next: u16,
    count: u16,
    paste: bool,
    bytes: Vec<u8>,
}

impl ClipboardAssembly {
    fn push(&mut self, chunk: ClipboardChunk) -> ClipboardProgress {
        if chunk.index == 0 {
            self.active = true;
            self.id = chunk.id;
            self.count = chunk.count;
            self.paste = chunk.paste;
            self.next = 0;
            self.bytes.clear();
        } else if !self.active
            || chunk.id != self.id
            || chunk.count != self.count
            || chunk.index != self.next
            || chunk.paste != self.paste
        {
            self.reset();
            return ClipboardProgress::Rejected;
        }
        if self.bytes.len() + chunk.bytes.len() > clipboard::MAX_CLIPBOARD_BYTES {
            self.reset();
            return ClipboardProgress::Rejected;
        }
        self.bytes.extend_from_slice(&chunk.bytes);
        self.next += 1;
        if self.next < self.count {
            return ClipboardProgress::Pending;
        }
        let bytes = std::mem::take(&mut self.bytes);
        let paste = self.paste;
        self.reset();
        match String::from_utf8(bytes) {
            Ok(text) if !text.is_empty() => ClipboardProgress::Complete { text, paste },
            _ => ClipboardProgress::Rejected,
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The agent-protocol form of one `INPUT` (time/extra info are always 0).
fn input_record(input: &INPUT) -> Option<Record> {
    // SAFETY: the union member read matches the INPUT type tag.
    unsafe {
        if input.r#type == INPUT_MOUSE {
            let mi = input.Anonymous.mi;
            Some(Record::Mouse {
                dx: mi.dx,
                dy: mi.dy,
                data: mi.mouseData,
                flags: mi.dwFlags.0,
            })
        } else if input.r#type == INPUT_KEYBOARD {
            let ki = input.Anonymous.ki;
            Some(Record::Key {
                vk: ki.wVk.0,
                scan: ki.wScan,
                flags: ki.dwFlags.0,
            })
        } else {
            None
        }
    }
}

/// The exact keyboard event injected for a tracked key. Scan-code keys let
/// the host's own layout translate the physical key (AltGr, non-US layouts);
/// VK-only keys remain for codes without a scan code (Pause, legacy packets).
fn keyboard_input(key: HeldKey, up: bool) -> KEYBDINPUT {
    let mut flags = KEYBD_EVENT_FLAGS::default();
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    let (vk, scan) = match key {
        HeldKey::Vk(vk) => (vk, 0),
        HeldKey::Scan { code, extended } => {
            flags |= KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            (0, u16::from(code))
        }
    };
    KEYBDINPUT {
        wVk: VIRTUAL_KEY(vk),
        wScan: scan,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn diagnostics_count_receipts_before_secure_gate_and_do_not_double_count() {
        let mut sink = super::InputSink::new();
        sink.set_secure_desktop(true, false);
        sink.note_packet_received(&[super::TYPE_KEY_DOWN, 65, 0]);
        sink.handle_counted(&[super::TYPE_KEY_DOWN, 65, 0]);
        sink.handle(&[super::TYPE_TEXT, b'x']);
        let counters = sink.take_diagnostics();
        assert_eq!(counters.received_by_kind[super::TYPE_KEY_DOWN as usize], 1);
        assert_eq!(counters.received_by_kind[super::TYPE_TEXT as usize], 1);
        assert_eq!(
            counters.drops_by_reason[super::InputDropReason::SecurePolicy as usize],
            2
        );
        assert_eq!(counters.route_calls, [0, 0, 0]);
    }

    #[test]
    fn invalid_absolute_mapping_is_diagnosed_before_injection() {
        let mut sink = super::InputSink::new();
        sink.set_absolute_region(super::DesktopRegion::new(0, 0, 0, 1080));
        let outcome = sink.handle(&[super::TYPE_MOUSE_ABS, 0, 0, 0, 0]);
        assert_eq!(outcome.application.requested, 0);
        let counters = sink.take_diagnostics();
        assert_eq!(
            counters.drops_by_reason[super::InputDropReason::MappingInvalid as usize],
            1
        );
        assert_eq!(counters.route_calls, [0, 0, 0]);
    }

    #[test]
    fn display_wake_is_one_zero_delta_relative_move_and_never_routes_to_secure_helper() {
        let input = super::display_wake_input();
        assert_eq!(input.r#type, super::INPUT_MOUSE);
        let mouse = unsafe { input.Anonymous.mi };
        assert_eq!((mouse.dx, mouse.dy, mouse.mouseData), (0, 0, 0));
        assert_eq!(mouse.dwFlags, super::MOUSEEVENTF_MOVE);
        let mut sink = super::InputSink::new();
        sink.set_secure_desktop(true, true);
        assert_eq!(sink.wake_display(), super::InputApplication::default());
        assert_eq!(sink.take_diagnostics().route_calls, [0, 0, 0]);
    }

    #[test]
    fn secure_absolute_coordinates_are_not_normalized_against_default_desktop() {
        let selected = super::DesktopRegion::new(1920, 0, 1920, 1080);
        let normal = super::DesktopRegion::new(0, 0, 3840, 1080);
        for point in [(0, 0), (32768, 32768), (65535, 65535)] {
            assert_eq!(
                super::absolute_coordinates_for_input(true, point.0, point.1, selected, normal),
                point
            );
        }
        assert_ne!(
            super::absolute_coordinates_for_input(false, 32768, 32768, selected, normal),
            (32768, 32768)
        );
    }
    #[test]
    fn secure_input_is_dropped_without_capability_and_never_falls_back() {
        let mut sink = super::InputSink::new();
        sink.set_secure_desktop(true, false);
        for packet in [
            vec![super::TYPE_KEY_DOWN, 65, 0],
            vec![super::TYPE_TEXT, b'x'],
        ] {
            assert_eq!(sink.handle(&packet), super::InputOutcome::default());
        }
        sink.set_secure_desktop(true, true);
        let result = sink.handle(&[super::TYPE_TEXT, b'x']);
        assert_eq!(result.application.applied, 0);
        assert!(result.application.requested > 0);
        assert_eq!(
            sink.handle(&[super::TYPE_CLIPBOARD]),
            super::InputOutcome::default()
        );
        assert!(sink.poll_clipboard(false).is_none());
    }
    use super::{
        cursor_snapshot, decode, keyboard_input, map_monitor_absolute_to_virtual, newer_sequence,
        normalized_axis_to_pixel, offset_absolute_to_virtual, ClipboardAssembly, ClipboardChunk,
        ClipboardProgress, Command, CursorState, DesktopRegion, HeldKey, InputApplication,
        InputFailureLogGate, InputSink, InputState, CLIPBOARD_CHUNK_BYTES, CLIPBOARD_MAX_CHUNKS,
        INPUT_FAILURE_LOG_INTERVAL, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
        SCAN_LCONTROL, SCAN_LWIN, VK_CONTROL, VK_MASK, VK_V,
    };
    use std::time::Instant;

    #[test]
    fn carried_button_release_is_reconciled_before_resumed_motion() {
        let mut carry = super::CarriedInput::default();
        let mut sink_state = InputState {
            buttons: 1,
            ..Default::default()
        };
        carry.observe_forwarded(&[4, 1]);
        carry.begin_switch(true);
        // A positioned release must not move the pointer during the gate.
        carry.observe_gated(&[9, 7, 0, 100, 0, 200, 0, 0]);
        assert!(!carry.take_resume_release(true, false));
        assert_eq!(sink_state.buttons, 1, "carry still owns the native drag");
        assert!(carry.take_resume_release(true, true));
        let (keys, released_buttons) = sink_state.take_release_plan();
        assert!(keys.is_empty());
        assert_eq!(released_buttons, 1);
        // This is the same release plan consumed by InputSink::release_all;
        // exercise its state without invoking Windows SendInput.
        carry.observe_forwarded(&[7, 8, 0, 1, 0, 2, 0, 0, 0, 0, 0]);
        assert!(sink_state.accept_motion(8));
        assert_eq!(sink_state.buttons, 0, "resumed motion cannot keep dragging");
        assert!(
            !carry.take_resume_release(true, true),
            "release is consumed once"
        );
    }

    #[test]
    fn carried_release_waits_for_available_capture_and_presented_frame() {
        // Capture availability may return before presentation acknowledgement.
        for (available, ready) in [(false, false), (false, true), (true, false)] {
            let mut carry = super::CarriedInput::default();
            carry.observe_forwarded(&[4, 1]);
            carry.begin_switch(true);
            carry.observe_gated(&[4, 0]);
            assert!(!carry.take_resume_release(available, ready));
            assert!(carry.take_resume_release(true, true));
        }
    }

    #[test]
    fn carried_vk_and_scan_key_releases_reconcile_held_input() {
        for (down, up) in [
            (vec![0, 65, 0], vec![1, 65, 0]),
            (vec![13, 2, 29, 0, 0, 0], vec![13, 3, 29, 0, 0, 0]),
        ] {
            let mut carry = super::CarriedInput::default();
            carry.observe_forwarded(&down);
            carry.begin_switch(true);
            carry.observe_gated(&up);
            assert!(carry.take_resume_release(true, true));
            assert!(carry.keys.is_empty());
        }
    }

    #[test]
    fn carry_without_a_release_keeps_held_input_and_ignores_gated_actions() {
        let mut carry = super::CarriedInput::default();
        carry.observe_forwarded(&[4, 1]);
        carry.observe_forwarded(&[0, 65, 0]);
        carry.begin_switch(true);
        for packet in [
            vec![4, 3], // New press while left is still held.
            vec![0, 66, 0],
            vec![1, 66, 0], // Release of a key that was never admitted.
            vec![13, 1, 30, 0, 0, 0],
            vec![7, 8, 0, 1, 0, 2, 0, 0, 0, 0, 0],
            vec![3, 100, 0, 200, 0],
            vec![5, 120, 0],
            vec![11, b'x'],
            vec![9], // Malformed positioned button packet.
        ] {
            carry.observe_gated(&packet);
        }
        assert!(!carry.take_resume_release(true, true));
        assert_eq!(carry.buttons, 1);
        assert_eq!(carry.keys, [HeldKey::Vk(65)].into_iter().collect());
    }

    #[test]
    fn partial_button_release_and_release_all_reconcile_but_manual_switch_resets() {
        for release in [vec![4, 2], vec![6]] {
            let mut carry = super::CarriedInput::default();
            carry.observe_forwarded(&[4, 3]);
            carry.begin_switch(true);
            carry.observe_gated(&release);
            assert!(carry.take_resume_release(true, true));
            assert_eq!(carry.buttons, 0);
        }
        let mut carry = super::CarriedInput::default();
        carry.observe_forwarded(&[4, 1]);
        carry.begin_switch(true);
        carry.observe_gated(&[4, 0]);
        carry.begin_switch(false);
        carry.observe_gated(&[4, 0]);
        assert!(!carry.take_resume_release(true, true));
        assert_eq!(carry.buttons, 0);
    }

    #[test]
    fn native_drag_input_keeps_releases_without_new_presses_or_motion() {
        assert_eq!(
            super::release_only(Command::Buttons(0b110), 0b101),
            Some(Command::Buttons(0b100))
        );
        assert_eq!(
            super::release_only(Command::ButtonsAt(7, 900, 800, 0b110), 0b101),
            Some(Command::Buttons(0b100))
        );
        for command in [
            Command::Key(65, true),
            Command::ScanKey(
                HeldKey::Scan {
                    code: 30,
                    extended: false,
                },
                true,
            ),
            Command::ReleaseAll,
            Command::CursorSync(7),
        ] {
            assert!(super::release_only(command, 0).is_some());
        }
        for command in [
            Command::Key(65, false),
            Command::ScanKey(HeldKey::Vk(65), false),
            Command::Abs(900, 800),
            Command::Rel(1, 2),
            Command::Wheel(120),
            Command::Motion(7, 1, 2, 0, 0),
            Command::Absolute(7, 900, 800),
            Command::Text("x".into()),
            Command::Clipboard(ClipboardChunk {
                paste: true,
                id: 1,
                index: 0,
                count: 1,
                bytes: vec![b'x'],
            }),
        ] {
            assert!(super::release_only(command, 0).is_none());
        }
    }

    struct FakeMouseKeys {
        settings: std::cell::Cell<super::MOUSEKEYS>,
        reads: std::cell::Cell<usize>,
        writes: std::cell::Cell<usize>,
        fail_read: std::cell::Cell<bool>,
        fail_write: std::cell::Cell<bool>,
    }

    impl FakeMouseKeys {
        fn new() -> Self {
            Self {
                settings: std::cell::Cell::new(super::MOUSEKEYS {
                    cbSize: std::mem::size_of::<super::MOUSEKEYS>() as u32,
                    dwFlags: 0x1000_003e,
                    iMaxSpeed: 80,
                    iTimeToMaxSpeed: 3000,
                    iCtrlSpeed: 2,
                    dwReserved1: 0,
                    dwReserved2: 0,
                }),
                reads: std::cell::Cell::new(0),
                writes: std::cell::Cell::new(0),
                fail_read: std::cell::Cell::new(false),
                fail_write: std::cell::Cell::new(false),
            }
        }

        fn read(&self) -> windows::core::Result<super::MOUSEKEYS> {
            self.reads.set(self.reads.get() + 1);
            if self.fail_read.get() {
                Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_ACCESSDENIED,
                ))
            } else {
                Ok(self.settings.get())
            }
        }

        fn write(&self, settings: &super::MOUSEKEYS) -> windows::core::Result<()> {
            self.writes.set(self.writes.get() + 1);
            if self.fail_write.get() {
                Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_ACCESSDENIED,
                ))
            } else {
                self.settings.set(*settings);
                Ok(())
            }
        }
    }

    #[test]
    fn headless_cursor_support_changes_only_on_bit_and_restores_on_disconnect() {
        let api = FakeMouseKeys::new();
        let original = api.settings.get();
        let mut state = super::HeadlessCursorState::default();
        state.set_active_with(true, false, || api.read(), |settings| api.write(settings));
        let mut expected = original;
        expected.dwFlags |= super::MKF_MOUSEKEYSON;
        assert_eq!(api.settings.get(), expected);
        assert!(state.owned.is_some());
        for _ in 0..100 {
            state.set_active_with(true, false, || api.read(), |settings| api.write(settings));
        }
        assert_eq!((api.reads.get(), api.writes.get()), (1, 1));
        state.set_active_with(false, false, || api.read(), |settings| api.write(settings));
        assert_eq!(api.settings.get(), original);
        assert!(state.owned.is_none());
    }

    #[test]
    fn headless_cursor_support_leaves_real_mouse_and_user_enabled_mousekeys_alone() {
        let api = FakeMouseKeys::new();
        let mut state = super::HeadlessCursorState::default();
        state.set_active_with(true, true, || api.read(), |settings| api.write(settings));
        state.set_active_with(false, true, || api.read(), |settings| api.write(settings));
        assert_eq!((api.reads.get(), api.writes.get()), (0, 0));
        let mut user_settings = api.settings.get();
        user_settings.dwFlags |= super::MKF_MOUSEKEYSON;
        api.settings.set(user_settings);
        state.set_active_with(true, false, || api.read(), |settings| api.write(settings));
        state.set_active_with(false, false, || api.read(), |settings| api.write(settings));
        assert_eq!(api.writes.get(), 0);
        assert_eq!(api.settings.get(), user_settings);
        assert!(state.owned.is_none());
    }

    #[test]
    fn headless_cursor_support_preserves_concurrent_preferences_and_user_disable() {
        let api = FakeMouseKeys::new();
        let mut state = super::HeadlessCursorState::default();
        state.set_active_with(true, false, || api.read(), |settings| api.write(settings));
        let mut edited = api.settings.get();
        edited.dwFlags |= 0x80; // User changes NumLock policy while streaming.
        edited.iMaxSpeed = 133;
        api.settings.set(edited);
        state.set_active_with(false, false, || api.read(), |settings| api.write(settings));
        edited.dwFlags &= !super::MKF_MOUSEKEYSON;
        assert_eq!(api.settings.get(), edited);
        state.set_active_with(true, false, || api.read(), |settings| api.write(settings));
        edited.iMaxSpeed = 200;
        api.settings.set(edited); // User disables the feature themselves.
        let writes = api.writes.get();
        state.set_active_with(false, false, || api.read(), |settings| api.write(settings));
        assert_eq!(api.writes.get(), writes);
        assert_eq!(api.settings.get(), edited);
        assert!(state.owned.is_none());
    }

    #[test]
    fn headless_cursor_support_api_failure_does_not_retry_during_polling() {
        let api = FakeMouseKeys::new();
        api.fail_read.set(true);
        let mut state = super::HeadlessCursorState::default();
        for _ in 0..100 {
            state.set_active_with(true, false, || api.read(), |settings| api.write(settings));
        }
        assert_eq!((api.reads.get(), api.writes.get()), (1, 0));
        state.set_active_with(false, false, || api.read(), |settings| api.write(settings));
        api.fail_read.set(false);
        api.fail_write.set(true);
        for _ in 0..100 {
            state.set_active_with(true, false, || api.read(), |settings| api.write(settings));
        }
        assert_eq!(api.writes.get(), 1);
        assert!(state.owned.is_none());
    }

    #[test]
    fn headless_cursor_support_failed_restore_retains_ownership_for_drop_retry() {
        let api = FakeMouseKeys::new();
        let original = api.settings.get();
        let mut state = super::HeadlessCursorState::default();
        state.set_active_with(true, false, || api.read(), |settings| api.write(settings));
        api.fail_write.set(true);
        state.set_active_with(false, false, || api.read(), |settings| api.write(settings));
        assert!(state.owned.is_some());
        api.fail_write.set(false);
        state.restore_with(|| api.read(), |settings| api.write(settings));
        assert_eq!(api.settings.get(), original);
        assert!(state.owned.is_none());
    }

    #[test]
    fn headless_cursor_support_retries_failed_activation_with_bounded_backoff() {
        let api = FakeMouseKeys::new();
        api.fail_read.set(true);
        let mut state = super::HeadlessCursorState::default();
        let mut retry = super::HeadlessCursorRetry::default();
        let start = Instant::now();
        for millis in (0..10_000).step_by(16) {
            let now = start + std::time::Duration::from_millis(millis);
            if retry.take_attempt(true, now) {
                let succeeded = state.apply_active_with(
                    true,
                    false,
                    || api.read(),
                    |settings| api.write(settings),
                );
                retry.finish(succeeded, now);
            }
        }
        assert_eq!(api.reads.get(), 3);
        assert_eq!(api.writes.get(), 0);
        // A new viewer-state edge gets a fresh bounded budget.
        assert!(retry.take_attempt(false, start + std::time::Duration::from_secs(11)));
        retry.finish(true, start + std::time::Duration::from_secs(11));
        api.fail_read.set(false);
        assert!(retry.take_attempt(true, start + std::time::Duration::from_secs(12)));
        let succeeded =
            state.apply_active_with(true, false, || api.read(), |settings| api.write(settings));
        retry.finish(succeeded, start + std::time::Duration::from_secs(12));
        assert!(succeeded);
        assert!(state.owned.is_some());
    }

    #[test]
    fn headless_cursor_support_retries_restore_but_never_overrides_user_disable() {
        let api = FakeMouseKeys::new();
        let original = api.settings.get();
        let mut state = super::HeadlessCursorState::default();
        let mut retry = super::HeadlessCursorRetry::default();
        let start = Instant::now();
        assert!(retry.take_attempt(true, start));
        retry.finish(
            state.apply_active_with(true, false, || api.read(), |settings| api.write(settings)),
            start,
        );
        let mut disabled = api.settings.get();
        disabled.dwFlags &= !super::MKF_MOUSEKEYSON;
        api.settings.set(disabled);
        for seconds in 1..100 {
            assert!(!retry.take_attempt(true, start + std::time::Duration::from_secs(seconds)));
        }
        assert_eq!(api.writes.get(), 1);
        assert!(retry.take_attempt(false, start + std::time::Duration::from_secs(100)));
        retry.finish(
            state.apply_active_with(false, false, || api.read(), |settings| api.write(settings)),
            start,
        );
        assert_eq!(api.settings.get(), original);
        assert!(retry.take_attempt(true, start + std::time::Duration::from_secs(101)));
        retry.finish(
            state.apply_active_with(true, false, || api.read(), |settings| api.write(settings)),
            start,
        );
        api.fail_write.set(true);
        let disconnect = start + std::time::Duration::from_secs(102);
        assert!(retry.take_attempt(false, disconnect));
        retry.finish(
            state.apply_active_with(false, false, || api.read(), |settings| api.write(settings)),
            disconnect,
        );
        assert!(state.owned.is_some());
        assert!(!retry.take_attempt(false, disconnect + std::time::Duration::from_millis(999)));
        api.fail_write.set(false);
        let next = disconnect + std::time::Duration::from_secs(1);
        assert!(retry.take_attempt(false, next));
        retry.finish(
            state.apply_active_with(false, false, || api.read(), |settings| api.write(settings)),
            next,
        );
        assert!(state.owned.is_none());
        assert_eq!(api.settings.get(), original);
        assert!(!retry.take_attempt(false, next + std::time::Duration::from_secs(60)));
    }

    #[test]
    fn headless_cursor_support_retains_marker_until_failed_delete_can_succeed() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = std::env::temp_dir().join(format!(
            "bubbleplay-cursor-delete-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("state.json");
        let original = FakeMouseKeys::new().settings.get();
        let mut applied = original;
        applied.dwFlags |= super::MKF_MOUSEKEYSON;
        let record = super::CursorRecoveryRecord::new(
            std::process::id(),
            super::MouseKeysLease { original, applied },
        );
        super::write_cursor_marker(&path, &record).unwrap();
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1 | 2)
            .open(&path)
            .unwrap();
        let mut guard = super::HeadlessCursorSupport::default();
        guard.marker = Some(path.clone());
        guard.remove_released_marker();
        assert_eq!(guard.marker.as_ref(), Some(&path));
        assert!(path.exists());
        drop(locked);
        guard.remove_released_marker();
        assert!(guard.marker.is_none());
        assert!(!path.exists());
        std::fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn headless_cursor_marker_accepts_only_the_owned_feature_bit() {
        let original = FakeMouseKeys::new().settings.get();
        let mut applied = original;
        applied.dwFlags |= super::MKF_MOUSEKEYSON;
        let record =
            super::CursorRecoveryRecord::new(42, super::MouseKeysLease { original, applied });
        let encoded = serde_json::to_vec(&record).unwrap();
        let mut restored: super::CursorRecoveryRecord = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(restored.lease().unwrap().original, original);
        restored.applied[2] += 1;
        assert!(restored.lease().is_err());
        restored.applied = record.applied;
        restored.original[1] |= super::MKF_MOUSEKEYSON;
        assert!(restored.lease().is_err());
    }

    #[test]
    fn headless_cursor_marker_cannot_overwrite_an_owner_and_bounds_its_input() {
        let directory = std::env::temp_dir().join(format!(
            "bubbleplay-cursor-marker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("state.json");
        let original = FakeMouseKeys::new().settings.get();
        let mut applied = original;
        applied.dwFlags |= super::MKF_MOUSEKEYSON;
        let record =
            super::CursorRecoveryRecord::new(42, super::MouseKeysLease { original, applied });
        super::write_cursor_marker(&path, &record).unwrap();
        assert!(super::write_cursor_marker(&path, &record).is_err());
        assert_eq!(
            super::read_cursor_marker(&path).unwrap().unwrap().owner_pid,
            42
        );
        super::remove_cursor_marker(&path, 43);
        assert!(path.exists());
        super::remove_cursor_marker(&path, 42);
        assert!(!path.exists());
        std::fs::write(&path, b"malformed").unwrap();
        assert!(super::read_cursor_marker(&path).is_err());
        std::fs::write(&path, vec![b' '; 4097]).unwrap();
        assert!(super::read_cursor_marker(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn cursor_kind_contract_matches_the_shared_native_mapping() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/cursor-types.json")).unwrap();
        assert_eq!(fixture["packetType"], super::TYPE_CURSOR_KIND);
        assert_eq!(fixture["packetBytes"], 2);
        let types = fixture["types"].as_array().unwrap();
        assert_eq!(types.len(), 15);
        for ty in types {
            let id = ty["id"].as_u64().unwrap() as u8;
            let kind = super::CursorKind::from_id(id).unwrap();
            assert_eq!(format!("{kind:?}"), ty["name"].as_str().unwrap());
            assert_eq!(kind.encode(), [129, id]);
        }
        assert!(super::CursorKind::from_id(15).is_none());
        assert!(super::CursorKind::from_id(255).is_none());
    }

    #[test]
    fn standard_cursor_handles_map_to_native_kinds_with_hidden_and_custom_fallbacks() {
        let handles = std::array::from_fn(|index| index + 100);
        for (index, handle) in handles.iter().enumerate() {
            assert_eq!(
                super::classify_cursor(*handle, true, &handles),
                super::CursorKind::from_id(index as u8).unwrap()
            );
            assert_eq!(
                super::classify_cursor(*handle, false, &handles),
                super::CursorKind::Hidden
            );
        }
        assert_eq!(
            super::classify_cursor(9999, true, &handles),
            super::CursorKind::Arrow
        );
        assert_eq!(
            super::classify_cursor(0, true, &[0; 14]),
            super::CursorKind::Arrow
        );
        assert_eq!(
            super::classify_cursor(0, false, &handles),
            super::CursorKind::Hidden
        );
    }

    #[test]
    fn stationary_kind_changes_and_forced_sync_bypass_motion_and_heartbeat() {
        use super::CursorKind::{Arrow, Hidden, Text};
        let now = Instant::now();
        let mut tracker = super::CursorKindTracker {
            standard: [0; 14],
            last_kind: None,
            last_emit: now,
            last_diagnostic: None,
            headless_support: super::HeadlessCursorSupport::new(),
        };
        assert_eq!(tracker.observe(Arrow, false, now), Some(Arrow));
        assert_eq!(tracker.observe(Arrow, false, now), None);
        assert_eq!(tracker.observe(Text, false, now), Some(Text));
        assert_eq!(tracker.observe(Hidden, false, now), Some(Hidden));
        assert_eq!(tracker.observe(Hidden, true, now), Some(Hidden));
        assert_eq!(
            tracker.observe(Hidden, false, now + std::time::Duration::from_secs(1)),
            Some(Hidden)
        );
        tracker.reset();
        assert_eq!(tracker.observe(Hidden, false, now), Some(Hidden));
    }

    #[test]
    fn windows_standard_cursor_handles_are_loaded_once_and_classified() {
        let mut tracker = super::CursorKindTracker::new(Instant::now());
        for (id, handle) in tracker.standard.iter().enumerate() {
            assert_ne!(*handle, 0, "standard Windows cursor {id} failed to load");
            assert_eq!(
                super::classify_cursor(*handle, true, &tracker.standard),
                super::CursorKind::from_id(id as u8).unwrap()
            );
        }
        // Read-only smoke of the current cursor; does not move it or set its shape.
        let kind = tracker.sample(true, Instant::now());
        eprintln!("GetCursorInfo native-kind sample: {kind:?}");
    }
    #[test]
    fn repeated_windows_injection_failures_are_rate_limited_with_a_suppressed_count() {
        let start = Instant::now();
        let mut gate = InputFailureLogGate::default();
        assert_eq!(gate.admit(start), Some(0));
        assert_eq!(gate.admit(start + INPUT_FAILURE_LOG_INTERVAL / 2), None);
        assert_eq!(gate.admit(start + INPUT_FAILURE_LOG_INTERVAL), Some(1));
    }

    #[test]
    fn input_application_distinguishes_parsed_packets_from_windows_applied_events() {
        let mut application = InputApplication::default();
        application.observe(1, 0, 5);
        application.observe(2, 2, 0);

        assert_eq!(application.requested, 3);
        assert_eq!(application.applied, 2);
        assert_eq!(application.failed_calls, 1);
        assert_eq!(application.last_error_code, Some(5));
        assert!(!application.succeeded());
    }

    #[test]
    fn release_plan_contains_each_held_key_and_pressed_button_once() {
        let mut state = InputState::default();
        state.key(HeldKey::Vk(0x41), false);
        state.key(HeldKey::Vk(0x10), false);
        state.key(HeldKey::Vk(0x41), false);
        state.buttons = 0b101;

        assert_eq!(
            state.take_release_plan(),
            (vec![HeldKey::Vk(0x10), HeldKey::Vk(0x41)], 0b101)
        );
        assert_eq!(state.take_release_plan(), (Vec::new(), 0));
    }

    fn contract() -> serde_json::Value {
        serde_json::from_str(include_str!("../../fixtures/input-contract.json")).unwrap()
    }

    fn bytes(value: &serde_json::Value) -> Vec<u8> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|byte| byte.as_u64().unwrap() as u8)
            .collect()
    }

    #[test]
    fn contract_scan_codes_decode_to_scan_keys_with_extended_flags() {
        let contract = contract();
        let table = contract["scanCodes"].as_object().unwrap();
        assert!(table.len() > 100);
        for (code, entry) in table {
            let scan = entry["scan"].as_u64().unwrap() as u16;
            let extended = entry["extended"].as_bool().unwrap();
            let packet = [13, if extended { 2 } else { 0 }, scan as u8, 0, 0, 0];
            assert_eq!(
                decode(&packet),
                Some(Command::ScanKey(
                    HeldKey::Scan {
                        code: scan as u8,
                        extended
                    },
                    false
                )),
                "{code}"
            );
        }
        for code in contract["mustBeExtended"].as_array().unwrap() {
            assert_eq!(
                table[code.as_str().unwrap()]["extended"],
                serde_json::json!(true),
                "{code}"
            );
        }
        // Left/right modifiers are physically distinct keys.
        assert_ne!(table["ControlLeft"], table["ControlRight"]);
        assert_ne!(table["AltLeft"], table["AltRight"]);
        assert_ne!(table["ShiftLeft"], table["ShiftRight"]);
    }

    #[test]
    fn contract_packets_decode_and_invalid_packets_are_rejected() {
        let contract = contract();
        for case in contract["packets"].as_array().unwrap() {
            let packet = bytes(&case["bytes"]);
            let decoded = decode(&packet);
            if let Some(key) = case.get("key") {
                let expected = match key.get("vk") {
                    Some(vk) => HeldKey::Vk(vk.as_u64().unwrap() as u16),
                    None => HeldKey::Scan {
                        code: key["scan"].as_u64().unwrap() as u8,
                        extended: key["extended"].as_bool().unwrap(),
                    },
                };
                assert_eq!(
                    decoded,
                    Some(Command::ScanKey(expected, case["up"].as_bool().unwrap())),
                    "{}",
                    case["name"]
                );
            } else {
                let clip = &case["clipboard"];
                assert_eq!(
                    decoded,
                    Some(Command::Clipboard(ClipboardChunk {
                        paste: clip["paste"].as_bool().unwrap(),
                        id: clip["id"].as_u64().unwrap() as u16,
                        index: clip["index"].as_u64().unwrap() as u16,
                        count: clip["count"].as_u64().unwrap() as u16,
                        bytes: clip["text"].as_str().unwrap().as_bytes().to_vec(),
                    }))
                );
            }
        }
        for packet in contract["invalidPackets"].as_array().unwrap() {
            assert_eq!(decode(&bytes(packet)), None, "{packet}");
        }
        assert_eq!(
            contract["clipboardPacket"]["chunkBytes"],
            serde_json::json!(CLIPBOARD_CHUNK_BYTES)
        );
        assert_eq!(
            contract["clipboardPacket"]["maxChunks"],
            serde_json::json!(CLIPBOARD_MAX_CHUNKS)
        );
        assert_eq!(
            contract["hostClipboardMessage"]["chunkBytes"],
            serde_json::json!(crate::clipboard::HOST_CHUNK_BYTES)
        );
        let mut oversized = vec![12, 0, 0, 0, 0, 0, 1, 0];
        oversized.resize(8 + CLIPBOARD_CHUNK_BYTES + 1, b'x');
        assert_eq!(decode(&oversized), None);
    }

    #[test]
    fn scan_keys_inject_scancode_with_extended_flag_and_vk_fallback_without() {
        let arrow = keyboard_input(
            HeldKey::Scan {
                code: 0x4B,
                extended: true,
            },
            true,
        );
        assert_eq!(arrow.wVk.0, 0);
        assert_eq!(arrow.wScan, 0x4B);
        assert_eq!(
            arrow.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
        );
        let letter = keyboard_input(
            HeldKey::Scan {
                code: 0x1E,
                extended: false,
            },
            false,
        );
        assert_eq!(letter.dwFlags, KEYEVENTF_SCANCODE);
        let pause = keyboard_input(HeldKey::Vk(0x13), false);
        assert_eq!((pause.wVk.0, pause.wScan, pause.dwFlags.0), (0x13, 0, 0));
    }

    #[test]
    fn release_all_repeats_the_scan_codes_that_were_pressed() {
        let mut state = InputState::default();
        let right_ctrl = HeldKey::Scan {
            code: 0x1D,
            extended: true,
        };
        let left_ctrl = HeldKey::Scan {
            code: 0x1D,
            extended: false,
        };
        state.key(right_ctrl, false);
        state.key(left_ctrl, false);
        state.key(left_ctrl, true);
        let (keys, _) = state.take_release_plan();
        assert_eq!(keys, vec![right_ctrl]);
        let release = keyboard_input(keys[0], true);
        assert_eq!(
            release.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
        );
    }

    #[test]
    fn paste_plan_adds_ctrl_only_when_not_held_and_masks_windows_keys() {
        let v = [(HeldKey::Vk(VK_V), false), (HeldKey::Vk(VK_V), true)];
        let mut idle = InputState::default();
        let plan = idle.paste_plan();
        assert_eq!(plan.first(), Some(&(HeldKey::Vk(VK_CONTROL), false)));
        assert_eq!(&plan[1..3], &v);
        assert_eq!(plan.last(), Some(&(HeldKey::Vk(VK_CONTROL), true)));

        let mut ctrl = InputState::default();
        ctrl.key(
            HeldKey::Scan {
                code: SCAN_LCONTROL,
                extended: false,
            },
            false,
        );
        assert_eq!(ctrl.paste_plan(), v.to_vec());

        let win = HeldKey::Scan {
            code: SCAN_LWIN,
            extended: true,
        };
        let mut meta = InputState::default();
        meta.key(win, false);
        let plan = meta.paste_plan();
        assert_eq!(
            &plan[..3],
            &[
                (HeldKey::Vk(VK_MASK), false),
                (HeldKey::Vk(VK_MASK), true),
                (win, true)
            ]
        );
        assert!(
            !meta.key(win, true),
            "the browser's later Win key-up is swallowed"
        );
        assert!(meta.key(win, false));
        assert!(meta.key(win, true));
    }

    fn chunk(id: u16, index: u16, count: u16, bytes: &[u8]) -> ClipboardChunk {
        ClipboardChunk {
            paste: true,
            id,
            index,
            count,
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn clipboard_assembly_requires_ordered_complete_utf8() {
        let mut assembly = ClipboardAssembly::default();
        assert_eq!(
            assembly.push(chunk(1, 0, 2, &[0xC3])),
            ClipboardProgress::Pending
        );
        assert_eq!(
            assembly.push(chunk(1, 1, 2, &[0xA9])),
            ClipboardProgress::Complete {
                text: "é".into(),
                paste: true
            }
        );
        assert_eq!(
            assembly.push(chunk(2, 1, 2, b"x")),
            ClipboardProgress::Rejected
        );
        assert_eq!(
            assembly.push(chunk(3, 0, 3, b"a")),
            ClipboardProgress::Pending
        );
        assert_eq!(
            assembly.push(chunk(3, 2, 3, b"c")),
            ClipboardProgress::Rejected
        );
        assert_eq!(
            assembly.push(chunk(4, 0, 1, &[0xFF])),
            ClipboardProgress::Rejected
        );
        assert_eq!(
            assembly.push(chunk(5, 0, 1, b"")),
            ClipboardProgress::Rejected
        );
        // A new transfer restarts cleanly after an abandoned one.
        assembly.push(chunk(6, 0, 2, b"a"));
        assert_eq!(
            assembly.push(chunk(7, 0, 1, b"b")),
            ClipboardProgress::Complete {
                text: "b".into(),
                paste: true
            }
        );
    }

    #[test]
    fn clipboard_assembly_rejects_more_than_one_mebibyte() {
        let mut assembly = ClipboardAssembly::default();
        let full = vec![b'x'; CLIPBOARD_CHUNK_BYTES];
        for index in 0..CLIPBOARD_MAX_CHUNKS - 1 {
            assert_eq!(
                assembly.push(chunk(1, index, CLIPBOARD_MAX_CHUNKS, &full)),
                ClipboardProgress::Pending
            );
        }
        assert!(matches!(
            assembly.push(chunk(
                1,
                CLIPBOARD_MAX_CHUNKS - 1,
                CLIPBOARD_MAX_CHUNKS,
                &full
            )),
            ClipboardProgress::Complete { .. }
        ));
        let mut over = ClipboardAssembly {
            active: true,
            id: 1,
            next: 1,
            count: 2,
            paste: true,
            bytes: vec![b'x'; crate::clipboard::MAX_CLIPBOARD_BYTES],
        };
        assert_eq!(over.push(chunk(1, 1, 2, b"y")), ClipboardProgress::Rejected);
    }

    #[test]
    fn clipboard_packets_are_ignored_by_sinks_without_clipboard_access() {
        let mut sink = super::InputSink::new();
        let outcome = sink.handle(&[12, 1, 7, 0, 0, 0, 1, 0, b'h', b'i']);
        assert_eq!(outcome.application.requested, 0);
        assert_eq!(sink.poll_clipboard(false), None);
    }
    #[test]
    fn protocol_rejects_malformed_and_accepts_release_and_motion_wrap() {
        assert_eq!(decode(&[6]), Some(Command::ReleaseAll));
        assert_eq!(decode(&[7, 0]), None);
        assert_eq!(decode(&[4, 24]), Some(Command::Buttons(24)));
        assert!(newer_sequence(Some(u16::MAX), 0));
        assert!(!newer_sequence(Some(0), u16::MAX));
        assert_eq!(
            decode(&[7, 5, 0, 1, 0, 2, 0, 3, 0, 4, 0]),
            Some(Command::Motion(5, 1, 2, 3, 4))
        );
        assert_eq!(
            decode(&[8, 6, 0, 9, 0, 10, 0]),
            Some(Command::Absolute(6, 9, 10))
        );
        assert_eq!(
            decode(&[9, 7, 0, 11, 0, 12, 0, 1]),
            Some(Command::ButtonsAt(7, 11, 12, 1))
        );
        assert!(newer_sequence(Some(5), 6));
        assert!(!newer_sequence(Some(6), 5));
    }

    #[test]
    fn reliable_pointer_barrier_rejects_older_motion_but_accepts_newer_motion() {
        let mut state = InputState::default();
        assert!(state.accept_motion(4));
        assert!(state.advance_motion_barrier(7));
        assert!(!state.accept_motion(5));
        assert!(state.accept_motion(8));
        assert!(!state.advance_motion_barrier(7));
        assert_eq!(state.last_motion_sequence, Some(8));
    }

    #[test]
    fn cursor_sync_barrier_is_a_valid_input_command() {
        assert_eq!(decode(&[10, 7, 0]), Some(Command::CursorSync(7)));
        assert!(decode(&[10, 7]).is_none());
    }

    #[test]
    fn text_packet_accepts_utf8_and_rejects_invalid_or_oversized_payloads() {
        assert_eq!(
            decode(&[11, b'h', 0xC3, 0xA9]),
            Some(Command::Text("hé".into()))
        );
        assert!(decode(&[11]).is_none());
        assert!(decode(&[11, 0xFF]).is_none());
        assert!(decode(&vec![11; 514]).is_none());
    }

    #[test]
    fn cursor_state_encodes_ack_position_and_visibility_flags() {
        assert_eq!(
            CursorState {
                acknowledged_sequence: Some(7),
                x: 16_384,
                y: 49_152,
                in_bounds: true,
                visible: true,
            }
            .encode(),
            [128, 7, 0, 0, 64, 0, 192, 7]
        );
    }

    #[test]
    fn cursor_coordinates_normalize_primary_monitor_edges_and_outside_state() {
        assert_eq!(
            cursor_snapshot(0, 0, DesktopRegion::new(0, 0, 1920, 1080), true),
            super::CursorSnapshot {
                x: 0,
                y: 0,
                in_bounds: true,
                visible: true
            }
        );
        assert_eq!(
            cursor_snapshot(1919, 1079, DesktopRegion::new(0, 0, 1920, 1080), true),
            super::CursorSnapshot {
                x: u16::MAX,
                y: u16::MAX,
                in_bounds: true,
                visible: true
            }
        );
        assert_eq!(
            cursor_snapshot(-10, 2000, DesktopRegion::new(0, 0, 1920, 1080), false),
            super::CursorSnapshot {
                x: 0,
                y: u16::MAX,
                in_bounds: false,
                visible: false
            }
        );
    }

    #[test]
    fn cursor_coordinates_are_relative_to_the_selected_secondary_monitor() {
        let region = DesktopRegion::new(1920, -120, 2560, 1440);
        assert_eq!(
            cursor_snapshot(1920, -120, region, true),
            super::CursorSnapshot {
                x: 0,
                y: 0,
                in_bounds: true,
                visible: true,
            }
        );
        assert_eq!(
            cursor_snapshot(4479, 1319, region, true),
            super::CursorSnapshot {
                x: u16::MAX,
                y: u16::MAX,
                in_bounds: true,
                visible: true,
            }
        );
        assert!(!cursor_snapshot(100, 100, region, true).in_bounds);
    }

    #[test]
    fn absolute_input_maps_selected_monitor_into_full_virtual_desktop() {
        let selected = DesktopRegion::new(1920, 0, 1920, 1080);
        let desktop = DesktopRegion::new(0, 0, 3840, 1080);
        assert_eq!(
            map_monitor_absolute_to_virtual(0, 0, selected, desktop),
            (32_776, 0)
        );
        assert_eq!(
            map_monitor_absolute_to_virtual(u16::MAX, u16::MAX, selected, desktop),
            (u16::MAX, u16::MAX)
        );
    }

    #[test]
    fn absolute_input_supports_monitors_left_of_the_primary() {
        let selected = DesktopRegion::new(-1280, 0, 1280, 1024);
        let desktop = DesktopRegion::new(-1280, 0, 3200, 1080);
        assert_eq!(
            map_monitor_absolute_to_virtual(0, 0, selected, desktop),
            (0, 0)
        );
        let (right, bottom) =
            map_monitor_absolute_to_virtual(u16::MAX, u16::MAX, selected, desktop);
        assert!(right > 26_000 && right < 26_300);
        assert!(bottom > 62_000 && bottom < 62_200);
    }

    #[test]
    fn drag_offset_keeps_the_carried_cursor_and_clamps_to_the_desktop() {
        let mon2 = DesktopRegion::new(1920, 0, 2560, 1440);
        let desktop = DesktopRegion::new(0, 0, 4480, 1440);
        let pixel_of = |(x, y): (u16, u16)| {
            let w = u64::from(u16::MAX);
            (
                ((u64::from(x) * 4479 + w / 2) / w) as i32,
                ((u64::from(y) * 1439 + w / 2) / w) as i32,
            )
        };
        // The client pointer sat at the right edge of monitor 1 (pixel 2559 of 2560 on monitor 2
        // once the view switched). The cursor was carried to (1936, 540).
        let client = (u16::MAX, 20_000u16);
        let mapped_x = normalized_axis_to_pixel(client.0, mon2.x, mon2.width);
        let mapped_y = normalized_axis_to_pixel(client.1, mon2.y, mon2.height);
        let offset = (1936 - mapped_x, 540 - mapped_y);
        // The very same client position lands on the carried cursor.
        let (nx, ny) = offset_absolute_to_virtual(client.0, client.1, mon2, desktop, offset);
        let (px, py) = pixel_of((nx, ny));
        assert!((px - 1936).abs() <= 1 && (py - 540).abs() <= 1, "{px},{py}");
        // A client move of -100 px on monitor 2 moves the cursor by about -100 px.
        let moved = client.0 - (100u64 * u64::from(u16::MAX) / 2559) as u16;
        let (nx, _) = offset_absolute_to_virtual(moved, client.1, mon2, desktop, offset);
        assert!((pixel_of((nx, 0)).0 - 1836).abs() <= 2);
        // Far past the desktop edge clamps to it.
        let (nx, ny) = offset_absolute_to_virtual(0, 0, mon2, desktop, (-10_000, -10_000));
        assert_eq!((nx, ny), (0, 0));
        let (nx, ny) =
            offset_absolute_to_virtual(u16::MAX, u16::MAX, mon2, desktop, (9_000, 9_000));
        assert_eq!((nx, ny), (u16::MAX, u16::MAX));
    }

    #[test]
    fn drag_offset_lifecycle() {
        let mut sink = InputSink::new();
        sink.set_absolute_region(DesktopRegion::new(1920, 0, 2560, 1440));
        // A relative client (no absolute position) needs no offset.
        sink.begin_drag_offset(None, (1936, 540));
        assert_eq!(sink.drag_offset, None);
        sink.begin_drag_offset(Some((u16::MAX, 20_000)), (1936, 540));
        assert!(sink.drag_offset.is_some());
        // A later switch replaces it, a geometry change and the end of the loop clear it.
        sink.begin_drag_offset(Some((0, 0)), (100, 100));
        assert_eq!(sink.drag_offset, Some((100 - 1920, 100)));
        sink.clear_drag_offset();
        assert_eq!(sink.drag_offset, None);
        sink.begin_drag_offset(Some((0, 0)), (100, 100));
        sink.set_absolute_region(DesktopRegion::new(0, 0, 1920, 1080));
        assert_eq!(sink.drag_offset, None);
    }
}

impl Default for InputSink {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSink {
    pub fn new() -> Self {
        Self {
            state: InputState::default(),
            stats: InputStats::default(),
            absolute_region: DesktopRegion::new(0, 0, 1920, 1080),
            failure_log: InputFailureLogGate::default(),
            clipboard: None,
            admin_agent: None,
            secure_helper: None,
            secure_desktop: false,
            secure_input_allowed: false,
            foreground_privilege: Default::default(),
            admin_enabled: false,
            status_changes: Default::default(),
            diagnostics: Default::default(),
            diagnostics_rollup: crate::input_diagnostics::DiagnosticsRollup::new(
                std::time::Instant::now(),
                crate::input_diagnostics::DIAGNOSTICS_INFO_INTERVAL,
            ),
            input_flow: Default::default(),
            geometry_valid: true,
            drag_offset: None,
            diagnostics_foreground_pid: None,
            development_agent: false,
            #[cfg(test)]
            test_injections: None,
        }
    }

    /// Route injection through the Admin-mode agent when it is available;
    /// Ordinary targets fall back locally when unavailable. A known target
    /// requiring Admin mode never falls back from an unavailable admin agent.
    pub fn with_admin_agent(mut self, link: Option<AdminAgentLink>) -> Self {
        // Existing explicitly opted-in development agents run unelevated in
        // isolated integration tests. Production agents retain protected
        // console-bound policy enforcement on every request.
        self.development_agent = link
            .as_ref()
            .is_some_and(AdminAgentLink::allows_development_agent);
        self.admin_agent = link;
        self
    }

    pub fn with_secure_helper(
        mut self,
        link: Option<
            std::sync::Arc<std::sync::Mutex<crate::secure_helper::client::SecureHelperLink>>,
        >,
    ) -> Self {
        self.secure_helper = link;
        self
    }

    pub fn set_secure_desktop(&mut self, secure: bool, input_allowed: bool) {
        if self.secure_desktop != secure || (self.secure_input_allowed && !input_allowed) {
            self.release_all();
            self.state = InputState::default();
        }
        self.secure_desktop = secure;
        self.secure_input_allowed = input_allowed;
    }

    /// Enable text clipboard sync. Only call from a process running as the
    /// interactive user on the default desktop.
    pub fn with_clipboard(mut self, enabled: bool) -> Self {
        self.clipboard = enabled.then(ClipboardSync::default);
        self
    }

    /// New host clipboard text since the previous poll (never echoes text
    /// this sink set from the browser). `reset` starts a new baseline.
    pub fn poll_clipboard(&mut self, reset: bool) -> Option<clipboard::ClipboardContent> {
        if self.secure_desktop {
            return None;
        }
        if reset {
            // Establish an outgoing baseline without cancelling clipboard
            // writes already accepted from this viewer. Session/release gates
            // invalidate their own epochs separately.
            let sync = self.clipboard.as_mut()?;
            sync.content = None;
            sync.reset_pending = true;
        }
        self.finish_clipboard();
        let sync = self.clipboard.as_mut()?;
        if !sync.poll_pending {
            sync.poll_pending = sync.worker.as_ref().is_some_and(|worker| {
                worker.submit(ClipboardCommand::Poll {
                    epoch: sync.epoch,
                    reset: sync.reset_pending,
                })
            });
            if sync.poll_pending {
                sync.reset_pending = false;
            }
        }
        sync.content.take()
    }

    /// Only nonblocking queue reads run on the input owner. A delayed clipboard
    /// renderer must never prevent clicking another window or pressing Alt+Tab.
    pub fn finish_clipboard(&mut self) {
        self.finish_clipboard_with_identity(foreground_identity);
    }

    fn finish_clipboard_with_identity(&mut self, foreground: impl Fn() -> (isize, u32)) {
        let mut paste = false;
        if let Some(sync) = self.clipboard.as_mut() {
            while let Some(completion) = sync.worker.as_ref().and_then(ClipboardWorker::try_recv) {
                match completion {
                    Completion::Polled { epoch, content } if epoch == sync.epoch => {
                        sync.poll_pending = false;
                        if !sync.reset_pending {
                            sync.content = content;
                        }
                    }
                    Completion::Written { epoch, id, success } if epoch == sync.epoch => {
                        if sync.paste.as_ref().is_some_and(|pending| pending.id == id) {
                            let pending = sync.paste.take().unwrap();
                            paste = success
                                && Instant::now() < pending.expires
                                && pending.foreground.0 != 0
                                && foreground() == pending.foreground;
                            if !paste {
                                debug!("delayed clipboard paste cancelled (write, expiry or focus changed)");
                            }
                        }
                        if !success {
                            warn!("host clipboard write failed; paste cancelled");
                        }
                    }
                    _ => {}
                }
            }
        }
        if paste && !self.secure_desktop && !crate::native_file_drop::drag_active() {
            self.paste_with_operation("clipboard-paste");
        }
    }

    pub fn set_absolute_region(&mut self, region: DesktopRegion) {
        if let Some(sync) = self.clipboard.as_mut() {
            sync.paste = None;
        }
        self.geometry_valid = region.width > 0 && region.height > 0;
        self.absolute_region = region.sanitized();
        self.state.last_absolute_position = None;
        self.drag_offset = None;
    }

    /// The client pointer of the latest absolute move; `None` for relative clients.
    pub fn last_absolute_position(&self) -> Option<(u16, u16)> {
        self.state.last_absolute_position
    }

    /// After an edge hold moved the host cursor to `translated` (desktop pixels), keep absolute
    /// moves relative to that: `client_pointer` (the client's last absolute position before the
    /// switch) now maps to `translated`, and later moves keep that offset. A relative client
    /// (`None`) needs none. Replaces any earlier offset; cleared by [`Self::clear_drag_offset`],
    /// by the left button's release and by a geometry change.
    pub fn begin_drag_offset(
        &mut self,
        client_pointer: Option<(u16, u16)>,
        translated: (i32, i32),
    ) {
        self.drag_offset = if self.secure_desktop {
            None
        } else {
            client_pointer.map(|(x, y)| {
                let region = self.absolute_region.sanitized();
                (
                    translated.0 - normalized_axis_to_pixel(x, region.x, region.width),
                    translated.1 - normalized_axis_to_pixel(y, region.y, region.height),
                )
            })
        };
    }

    pub fn clear_drag_offset(&mut self) {
        self.drag_offset = None;
    }

    pub fn absolute_region(&self) -> DesktopRegion {
        self.absolute_region
    }

    /// Reuse the foreground sampler's 250 ms tick; no new polling timer.
    /// Connection attempts remain lazy on input, so this cannot block capture
    /// waiting for an agent handshake. Idle means pending, not observed failure.
    pub fn refresh_foreground_status(&mut self) {
        if self.foreground_privilege.refresh() {
            self.log_diagnostics();
            self.diagnostics_foreground_pid = self.foreground_privilege.pid();
        }
        self.admin_enabled = crate::admin_mode::effective_for_console();
    }

    pub fn take_status_change(&mut self) -> Option<InputRoutingStatus> {
        let status = if self.secure_desktop {
            InputRoutingStatus {
                reason: None,
                route: InputRoute::Secure,
            }
        } else {
            let unavailable = self.admin_agent.as_ref().is_none_or(|link| {
                matches!(
                    link.state(),
                    crate::admin_agent::client::LinkState::Fallback(_)
                )
            });
            InputRoutingStatus::normal(
                self.foreground_privilege.requires_admin(),
                self.admin_enabled,
                unavailable,
            )
        };
        self.status_changes.observe(status)
    }

    /// A fresh viewer needs the current warning even when focus did not change.
    pub fn reset_status_delivery(&mut self) {
        self.status_changes.reset();
    }

    /// Call at transport receipt, before readiness, desktop or native-drag gates.
    /// The normal handle methods do this for callers without an outer gate.
    pub fn note_packet_received(&mut self, data: &[u8]) {
        self.diagnostics.received(data.first().copied());
    }

    pub fn note_packet_drop(&mut self, reason: InputDropReason) {
        self.diagnostics.dropped(reason);
        self.note_input_flow(Some(reason));
    }

    /// Coalescing reduces injection work; it is not a blocked-input transition.
    pub fn note_packets_coalesced(&mut self, count: usize) {
        self.diagnostics.coalesced_packets = self
            .diagnostics
            .coalesced_packets
            .saturating_add(count as u64);
    }

    fn note_input_flow(&mut self, reason: Option<InputDropReason>) {
        let previous_reason = self.input_flow.reason();
        if let Some(transition) = self.input_flow.observe(reason) {
            match transition {
                // Pausing input for a native file drag is by design, not a fault.
                Some(InputDropReason::NativeDrag) => {
                    info!(reason = "native-drag", "remote input started dropping")
                }
                Some(reason) => warn!(reason = reason.name(), "remote input started dropping"),
                None => info!(
                    reason = previous_reason.map(InputDropReason::name),
                    "remote input flowing again"
                ),
            }
        }
    }

    pub fn take_diagnostics(&mut self) -> InputDiagnosticCounters {
        std::mem::take(&mut self.diagnostics)
    }

    /// Every window goes to debug; the sum of a minute (and the rest at session end) to info.
    pub fn log_diagnostics(&mut self) {
        let counters = self.take_diagnostics();
        if counters.is_empty() {
            return;
        }
        self.write_diagnostics(&counters, false);
        if let Some(total) = self
            .diagnostics_rollup
            .add(std::time::Instant::now(), &counters)
        {
            self.write_diagnostics(&total, true);
        }
    }

    fn write_diagnostics(&self, counters: &InputDiagnosticCounters, info: bool) {
        macro_rules! line {
            ($level:ident) => {
                tracing::$level!(
                    foreground_pid = self.diagnostics_foreground_pid,
                    received_by_kind = ?counters.named_receipts(),
                    drops_by_reason = ?counters.named_drops(),
                    local_calls = counters.route_calls[0],
                    admin_calls = counters.route_calls[1],
                    secure_submissions = counters.route_calls[2],
                    requested_events = counters.requested_events,
                    applied_events = counters.applied_events,
                    short_calls = counters.short_calls,
                    error_calls = counters.error_calls,
                    coalesced_packets = counters.coalesced_packets,
                    "remote input diagnostics"
                )
            };
        }
        if info {
            line!(info);
        } else {
            line!(debug);
        }
    }

    /// Handle one data-channel packet. Malformed packets are logged and dropped.
    pub fn handle(&mut self, data: &[u8]) -> InputOutcome {
        self.note_packet_received(data);
        self.handle_counted(data)
    }

    /// Receipt was already counted before a caller's gates/coalescing.
    pub fn handle_counted(&mut self, data: &[u8]) -> InputOutcome {
        if self.secure_desktop
            && (!self.secure_input_allowed || data.first() == Some(&TYPE_CLIPBOARD))
        {
            self.note_packet_drop(InputDropReason::SecurePolicy);
            if !self.secure_input_allowed {
                self.note_secure_input_drop(data, "no_input_capability");
            }
            return InputOutcome::default();
        }
        let Some(command) = decode(data) else {
            self.note_packet_drop(InputDropReason::Malformed);
            debug!(kind = ?data.first(), bytes = data.len(), "malformed or unknown input packet");
            return InputOutcome::default();
        };
        self.apply(command)
    }

    /// During a native drag, preserve releases without introducing another
    /// press, paste or pointer move into the attached Windows input queue.
    pub fn handle_release_only(&mut self, data: &[u8]) -> InputOutcome {
        self.note_packet_received(data);
        self.handle_release_only_counted(data)
    }

    pub fn handle_release_only_counted(&mut self, data: &[u8]) -> InputOutcome {
        if self.secure_desktop && !self.secure_input_allowed {
            self.note_packet_drop(InputDropReason::SecurePolicy);
            return InputOutcome::default();
        }
        let Some(command) = decode(data) else {
            self.note_packet_drop(InputDropReason::Malformed);
            return InputOutcome::default();
        };
        if let Command::ButtonsAt(seq, _, _, _) = &command {
            self.state.advance_motion_barrier(*seq);
        }
        match release_only(command, self.state.buttons) {
            Some(command) => self.apply(command),
            None => InputOutcome::default(),
        }
    }

    fn apply(&mut self, command: Command) -> InputOutcome {
        // Releases of the original Ctrl+V/Meta+V shortcut may arrive while the
        // write finishes. Preserve those releases and synthesize the eventual
        // paste from the then-current held state. New actions cancel it.
        let cancels_paste = match &command {
            Command::Key(_, up) | Command::ScanKey(_, up) => !up,
            Command::Buttons(mask) | Command::ButtonsAt(_, _, _, mask) => {
                mask & !self.state.buttons != 0
            }
            Command::Rel(..)
            | Command::Abs(..)
            | Command::Absolute(..)
            | Command::CursorSync(_) => false,
            Command::Motion(_, _, _, wx, wy) => *wx != 0 || *wy != 0,
            _ => true,
        };
        if cancels_paste {
            if let Some(sync) = self.clipboard.as_mut() {
                sync.paste = None;
            }
        }
        let mut outcome = InputOutcome::default();
        match command {
            Command::Key(vk, up) => {
                if self.state.key(HeldKey::Vk(vk), up) {
                    outcome.application.merge(self.key(HeldKey::Vk(vk), up));
                }
            }
            Command::ScanKey(key, up) => {
                if self.state.key(key, up) {
                    outcome.application.merge(self.key(key, up));
                }
            }
            Command::Clipboard(chunk) => outcome.application.merge(self.clipboard_chunk(chunk)),
            Command::Rel(dx, dy) => {
                self.stats.relative_packets += 1;
                self.state.last_absolute_position = None;
                outcome.application.merge(self.mouse_rel(dx, dy));
                outcome.cursor_changed = dx != 0 || dy != 0;
            }
            Command::Abs(x, y) => {
                self.stats.absolute_packets += 1;
                self.state.last_absolute_position = Some((x, y));
                outcome.application.merge(self.mouse_abs(x, y));
                outcome.cursor_changed = true;
            }
            Command::Buttons(mask) => outcome.application.merge(self.buttons(mask)),
            Command::Wheel(delta) => outcome.application.merge(self.wheel(delta)),
            Command::ReleaseAll => outcome.application.merge(self.release_all_report()),
            Command::Motion(seq, dx, dy, wx, wy) => {
                if self.state.accept_motion(seq) {
                    self.stats.relative_packets += 1;
                    self.state.last_absolute_position = None;
                    outcome.application.merge(self.mouse_rel(dx, dy));
                    outcome.accepted_sequence = Some(seq);
                    // Accepted sequencing itself requires cursor feedback:
                    // coalesced opposing movements can have no net displacement,
                    // but the receiver still needs this sequence acknowledged.
                    outcome.cursor_changed = true;
                } else {
                    self.stats.stale_motion_dropped += 1;
                    self.note_packet_drop(InputDropReason::StaleSequence);
                }
                // Pointer order and wheel distance have different semantics.
                // Retain unique late wheels, but never replay a duplicate or an
                // ambiguous event outside the u16 half-range window.
                if self.state.wheel_sequences.accept(seq) {
                    outcome.application.merge(self.wheel_horizontal(wx));
                    outcome.application.merge(self.wheel(wy));
                }
            }
            Command::Absolute(seq, x, y) => {
                if self.state.accept_motion(seq) {
                    self.stats.absolute_packets += 1;
                    self.state.last_absolute_position = Some((x, y));
                    outcome.application.merge(self.mouse_abs(x, y));
                    outcome.accepted_sequence = Some(seq);
                    outcome.cursor_changed = true;
                } else {
                    self.stats.stale_motion_dropped += 1;
                    self.note_packet_drop(InputDropReason::StaleSequence);
                }
            }
            Command::ButtonsAt(seq, x, y, mask) => {
                // Reliable control establishes a watermark across both data
                // channels. Motion packets older than this click/drag start
                // cannot arrive late and pull the pointer away again.
                let barrier_advanced = self.state.advance_motion_barrier(seq);
                let restore = (!barrier_advanced)
                    .then_some(self.state.last_absolute_position)
                    .flatten();
                self.stats.absolute_packets += 1;
                outcome.application.merge(self.mouse_abs(x, y));
                outcome.application.merge(self.buttons(mask));
                if let Some((latest_x, latest_y)) = restore {
                    outcome
                        .application
                        .merge(self.mouse_abs(latest_x, latest_y));
                } else if barrier_advanced {
                    self.state.last_absolute_position = Some((x, y));
                }
                outcome.accepted_sequence = self.state.last_motion_sequence;
                outcome.cursor_changed = true;
                outcome.feedback_reliable = true;
            }
            Command::CursorSync(seq) => {
                self.state.advance_motion_barrier(seq);
                self.stats.cursor_sync_requests += 1;
                outcome.accepted_sequence = self.state.last_motion_sequence;
                outcome.sync_requested = true;
                outcome.feedback_reliable = true;
            }
            Command::Text(text) => outcome.application.merge(self.text(&text)),
        }
        outcome
    }

    pub fn take_stats(&mut self) -> InputStats {
        std::mem::take(&mut self.stats)
    }

    /// Count rejected injection records without retaining or logging values.
    pub fn note_secure_input_drop(&mut self, data: &[u8], reason: &'static str) {
        let count = match decode(data) {
            Some(
                Command::Key(..) | Command::ScanKey(..) | Command::Abs(..) | Command::Absolute(..),
            ) => 1,
            Some(Command::Rel(dx, dy)) => usize::from(dx != 0 || dy != 0),
            Some(Command::Wheel(delta)) => usize::from(delta != 0),
            Some(Command::Text(text)) => text.encode_utf16().count().saturating_mul(2),
            Some(Command::Motion(_, dx, dy, wx, wy)) => {
                usize::from(dx != 0 || dy != 0) + usize::from(wx != 0) + usize::from(wy != 0)
            }
            Some(Command::Buttons(mask)) => (self.state.buttons ^ mask).count_ones() as usize,
            Some(Command::ButtonsAt(_, _, _, mask)) => {
                1 + (self.state.buttons ^ mask).count_ones() as usize
            }
            Some(Command::ReleaseAll) => {
                self.state.pressed_keys.len() + self.state.buttons.count_ones() as usize
            }
            _ => 0,
        };
        if let Some(link) = &self.secure_helper {
            link.lock()
                .unwrap_or_else(|error| error.into_inner())
                .note_input_drop(count as u64, reason);
        }
    }

    fn send_inputs(&mut self, operation: &'static str, inputs: &[INPUT]) -> InputApplication {
        self.send_inputs_with_route(operation, inputs, false)
    }

    /// Wake input stays on the guarded local SendInput route, even with Admin
    /// mode enabled. It never enters the secure helper or admin agent.
    pub fn wake_display(&mut self) -> InputApplication {
        let input = display_wake_input();
        self.send_inputs_with_route("display-wake", &[input], true)
    }

    fn send_inputs_with_route(
        &mut self,
        operation: &'static str,
        inputs: &[INPUT],
        local_only: bool,
    ) -> InputApplication {
        if inputs.is_empty() {
            return InputApplication::default();
        }
        #[cfg(test)]
        if let Some(records) = self.test_injections.as_mut() {
            records.extend(inputs.iter().map(|input| input_record(input).unwrap()));
            let expected = inputs.len() as u32;
            self.diagnostics.route(InputRoute::Local);
            self.diagnostics.receipt(expected, expected, 0);
            let mut application = InputApplication::default();
            application.observe(expected, expected, 0);
            return application;
        }
        let expected = inputs.len().min(u32::MAX as usize) as u32;
        if local_only && self.secure_desktop {
            return InputApplication::default();
        }
        if self.secure_desktop {
            let accepted = self.secure_input_allowed
                && self.secure_helper.as_ref().is_some_and(|link| {
                    let Some(records) = inputs.iter().map(input_record).collect::<Option<Vec<_>>>()
                    else {
                        return false;
                    };
                    link.lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .send_input(&records)
                });
            let mut application = InputApplication::default();
            // Submission is asynchronous. Never fall back to user/admin-agent
            // injection for secure input, even when the helper disappears.
            if !accepted {
                self.note_packet_drop(if self.secure_input_allowed {
                    InputDropReason::SecureHelperUnavailable
                } else {
                    InputDropReason::SecurePolicy
                });
                application.observe(expected, 0, crate::admin_agent::client::ERROR_AGENT_LOST);
            } else {
                // This is a submission route, not a Windows receipt. Actual
                // secure application counts remain the helper's v2 receipts.
                self.diagnostics.route(InputRoute::Secure);
                self.note_input_flow(None);
            }
            return application;
        }
        // The desktop can switch between the serve-loop observation and this
        // input batch. Never send secure-desktop input through either normal
        // user SendInput or the elevated normal-desktop admin agent.
        if !crate::desktop_session::input_desktop_is_default() {
            self.note_packet_drop(InputDropReason::NonDefaultDesktop);
            if let Some(suppressed) = self.failure_log.admit(Instant::now()) {
                warn!(
                    reason = InputDropReason::NonDefaultDesktop.name(),
                    suppressed, "remote input blocked by desktop guard"
                );
            }
            return InputApplication::default();
        }
        let requires_admin = !local_only && self.foreground_privilege.requires_admin();
        let via_agent = if !local_only && (self.admin_enabled || self.development_agent) {
            self.admin_agent.as_mut()
        } else {
            None
        }
        .and_then(|link| {
            let records = inputs
                .iter()
                .map(input_record)
                .collect::<Option<Vec<_>>>()?;
            link.inject(&records)
        });
        let (sent, error_code) = match via_agent {
            Some(receipt) => {
                self.diagnostics.route(InputRoute::Admin);
                receipt
            }
            None if requires_admin && self.admin_enabled => {
                self.note_packet_drop(InputDropReason::AdminUnavailable);
                let mut application = InputApplication::default();
                application.observe(expected, 0, crate::admin_agent::client::ERROR_AGENT_LOST);
                return application;
            }
            None => {
                self.diagnostics.route(InputRoute::Local);
                unsafe { SetLastError(WIN32_ERROR(0)) };
                let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
                // SendInput does not reliably set a useful error for UIPI
                // rejection, but capture it immediately: other desktop/session
                // failures do.
                let error = std::io::Error::last_os_error();
                (sent, error.raw_os_error().unwrap_or(0))
            }
        };
        let error = std::io::Error::from_raw_os_error(error_code);
        self.diagnostics.receipt(expected, sent, error_code);
        if sent != expected {
            self.note_packet_drop(if requires_admin && !self.admin_enabled {
                InputDropReason::AdminRequired
            } else if via_agent.is_some()
                && error_code == crate::admin_agent::client::ERROR_AGENT_LOST
            {
                InputDropReason::AdminUnavailable
            } else {
                InputDropReason::InjectionFailed
            });
            if let Some(suppressed) = self.failure_log.admit(Instant::now()) {
                warn!(
                    operation,
                    sent,
                    expected,
                    ?error,
                    suppressed,
                    "Windows input injection failed"
                );
            }
        } else {
            self.note_input_flow(None);
        }
        let mut application = InputApplication::default();
        application.observe(expected, sent, error_code);
        application
    }

    fn key(&mut self, key: HeldKey, up: bool) -> InputApplication {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: keyboard_input(key, up),
            },
        };
        self.send_inputs(if up { "key-up" } else { "key-down" }, &[input])
    }

    fn clipboard_chunk(&mut self, chunk: ClipboardChunk) -> InputApplication {
        let Some(sync) = self.clipboard.as_mut() else {
            self.note_packet_drop(InputDropReason::ClipboardDisabled);
            debug!("clipboard packet ignored: clipboard sync is disabled for this sink");
            return InputApplication::default();
        };
        let (text, paste) = match sync.assembly.push(chunk) {
            ClipboardProgress::Pending => return InputApplication::default(),
            ClipboardProgress::Rejected => {
                self.note_packet_drop(InputDropReason::ClipboardRejected);
                warn!("browser clipboard transfer rejected (out of order, oversized or not UTF-8)");
                return InputApplication::default();
            }
            ClipboardProgress::Complete { text, paste } => (text, paste),
        };
        sync.next_write = sync.next_write.wrapping_add(1);
        let id = sync.next_write;
        let expires = Instant::now() + crate::clipboard_worker::WRITE_LIFETIME;
        sync.paste = None;
        let accepted = sync.worker.as_ref().is_some_and(|worker| {
            worker.submit(ClipboardCommand::Write {
                epoch: sync.epoch,
                id,
                text,
                deadline: expires,
            })
        });
        if !accepted {
            self.note_packet_drop(InputDropReason::ClipboardRejected);
            warn!("clipboard queue unavailable; ordinary input remains enabled");
        } else if paste {
            sync.paste = Some(PendingClipboardPaste {
                id,
                foreground: foreground_identity(),
                expires,
            });
            let plan = self.state.release_paste_windows_keys();
            return self.send_key_plan("clipboard-modifiers", plan);
        }
        InputApplication::default()
    }

    /// Inject Ctrl+V for whatever the host clipboard holds now (file
    /// transfer: a paste-purpose upload already placed its files there).
    pub fn paste(&mut self) -> InputApplication {
        self.paste_with_operation("file-paste")
    }

    fn paste_with_operation(&mut self, operation: &'static str) -> InputApplication {
        if let Some(sync) = self.clipboard.as_mut() {
            sync.paste = None;
        }
        if self.secure_desktop {
            return InputApplication::default();
        }
        let plan = self.state.paste_plan();
        self.send_key_plan(operation, plan)
    }

    fn send_key_plan(
        &mut self,
        operation: &'static str,
        plan: Vec<(HeldKey, bool)>,
    ) -> InputApplication {
        let inputs: Vec<INPUT> = plan
            .into_iter()
            .map(|(key, up)| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: keyboard_input(key, up),
                },
            })
            .collect();
        self.send_inputs(operation, &inputs)
    }

    fn text(&mut self, text: &str) -> InputApplication {
        let mut inputs = Vec::new();
        for unit in text.encode_utf16() {
            for up in [false, true] {
                let mut flags = KEYEVENTF_UNICODE;
                if up {
                    flags |= KEYEVENTF_KEYUP;
                }
                inputs.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0),
                            wScan: unit,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }
        self.send_inputs("text", &inputs)
    }

    fn mouse_rel(&mut self, dx: i16, dy: i16) -> InputApplication {
        if dx == 0 && dy == 0 {
            return InputApplication::default();
        }
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: dx as i32,
                    dy: dy as i32,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        self.send_inputs("mouse-relative", &[input])
    }

    fn mouse_abs(&mut self, x: u16, y: u16) -> InputApplication {
        if !self.geometry_valid {
            self.note_packet_drop(InputDropReason::MappingInvalid);
            return InputApplication::default();
        }
        let virtual_desktop = if self.secure_desktop {
            self.absolute_region
        } else {
            virtual_desktop_region().unwrap_or(self.absolute_region)
        };
        let (x, y) = match self.drag_offset.filter(|_| !self.secure_desktop) {
            Some(offset) => {
                offset_absolute_to_virtual(x, y, self.absolute_region, virtual_desktop, offset)
            }
            None => absolute_coordinates_for_input(
                self.secure_desktop,
                x,
                y,
                self.absolute_region,
                virtual_desktop,
            ),
        };
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: x as i32,
                    dy: y as i32,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        self.send_inputs("mouse-absolute", &[input])
    }

    /// Bitmask diff -> press/release events for each button.
    fn buttons(&mut self, mask: u8) -> InputApplication {
        let changed = mask ^ self.state.buttons;
        self.state.buttons = mask;
        if changed & 1 != 0 && mask & 1 == 0 {
            // The drag ended: the next absolute move lands at the client's true pointer.
            self.drag_offset = None;
        }
        if changed == 0 {
            return InputApplication::default();
        }
        let mut inputs = Vec::with_capacity(5);
        for (bit, down_flag, up_flag) in [
            (1u8, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            (2u8, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            (4u8, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
            (8u8, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP),
            (16u8, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP),
        ] {
            if changed & bit != 0 {
                let flags = if mask & bit != 0 { down_flag } else { up_flag };
                inputs.push(INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: if bit == 8 {
                                1
                            } else if bit == 16 {
                                2
                            } else {
                                0
                            },
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }
        self.send_inputs("mouse-buttons", &inputs)
    }

    fn wheel(&mut self, delta: i16) -> InputApplication {
        if delta == 0 {
            return InputApplication::default();
        }
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: delta as u32,
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        self.send_inputs("mouse-wheel", &[input])
    }

    fn wheel_horizontal(&mut self, delta: i16) -> InputApplication {
        if delta == 0 {
            return InputApplication::default();
        }
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: delta as u32,
                    dwFlags: MOUSEEVENTF_HWHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        self.send_inputs("mouse-wheel-horizontal", &[input])
    }

    /// Release input the browser may never get to send when its data channel
    /// or signaling session disappears. This is also called from Drop.
    pub fn release_all(&mut self) {
        let _ = self.release_all_report();
    }

    /// Only the transport's viewer lifecycle may start a fresh producer's
    /// sequence space. Ordinary release-all/gating must retain stale barriers.
    pub fn reset_viewer_session(&mut self) {
        self.release_all();
        self.state = InputState::default();
    }

    pub fn release_all_report(&mut self) -> InputApplication {
        if let Some(sync) = self.clipboard.as_mut() {
            // Blur/disconnect and presentation gates also invalidate clipboard
            // requests queued before them, without waiting for native work.
            sync.reset();
        }
        self.drag_offset = None;
        let (keys, buttons) = self.state.take_release_plan();
        let mut application = InputApplication::default();
        for key in keys {
            application.merge(self.key(key, true));
        }
        if buttons != 0 {
            application.merge(self.send_button_releases(buttons));
        }
        application
    }

    fn send_button_releases(&mut self, buttons: u8) -> InputApplication {
        let mut inputs = Vec::with_capacity(5);
        for (bit, up_flag) in [
            (1u8, MOUSEEVENTF_LEFTUP),
            (2u8, MOUSEEVENTF_RIGHTUP),
            (4u8, MOUSEEVENTF_MIDDLEUP),
            (8u8, MOUSEEVENTF_XUP),
            (16u8, MOUSEEVENTF_XUP),
        ] {
            if buttons & bit != 0 {
                inputs.push(INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: if bit == 8 {
                                1
                            } else if bit == 16 {
                                2
                            } else {
                                0
                            },
                            dwFlags: up_flag,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }
        self.send_inputs("mouse-button-release", &inputs)
    }
}

impl Drop for InputSink {
    fn drop(&mut self) {
        self.release_all();
        let rest = self.take_diagnostics();
        self.diagnostics_rollup
            .add(std::time::Instant::now(), &rest);
        if let Some(total) = self.diagnostics_rollup.take() {
            self.write_diagnostics(&total, true);
        }
    }
}

#[cfg(test)]
mod local_wheel_regression_tests {
    use super::*;

    #[test]
    fn local_motion_v2_emits_vertical_wheel_with_signed_mouse_data() {
        for delta in [120i16, -120, i16::MIN, i16::MAX] {
            let mut sink = InputSink::new();
            assert!(!sink.admin_enabled);
            assert!(sink.admin_agent.is_none());
            // Capture the INPUT conversion at the injection seam without
            // injecting test scrolls into the owner's foreground application.
            sink.test_injections = Some(Vec::new());
            let mut packet = vec![TYPE_MOTION_V2];
            for value in [0u16, 0, 0, 0, delta as u16] {
                packet.extend_from_slice(&value.to_le_bytes());
            }
            let outcome = sink.handle(&packet);
            assert_eq!(outcome.application.requested, 1);
            assert_eq!(
                sink.test_injections.as_ref().unwrap().as_slice(),
                &[Record::Mouse {
                    dx: 0,
                    dy: 0,
                    data: i32::from(delta) as u32,
                    flags: MOUSEEVENTF_WHEEL.0
                },]
            );
            assert_eq!(sink.take_diagnostics().route_calls, [1, 0, 0]);
        }
    }
}

#[cfg(test)]
mod clipboard_responsiveness_tests {
    use super::*;
    use std::sync::mpsc::{Receiver, SyncSender};

    fn sink() -> (
        InputSink,
        Receiver<ClipboardCommand>,
        SyncSender<Completion>,
    ) {
        let (worker, commands, completions) = ClipboardWorker::test_channels();
        let mut sink = InputSink::new();
        sink.test_injections = Some(Vec::new());
        sink.clipboard = Some(ClipboardSync::with_worker(Some(worker)));
        (sink, commands, completions)
    }

    fn write(sink: &mut InputSink) {
        // Complete one paste-bearing clipboard transfer. No OS clipboard call.
        sink.handle(&[TYPE_CLIPBOARD, 1, 7, 0, 0, 0, 1, 0, b'h', b'i']);
        if let Some(paste) = sink.clipboard.as_mut().unwrap().paste.as_mut() {
            paste.foreground = (123, 99);
        }
    }

    fn completed() -> Completion {
        Completion::Written {
            epoch: 0,
            id: 1,
            success: true,
        }
    }

    #[test]
    fn blocked_clipboard_queues_never_hold_mouse_or_alt_tab_packets() {
        let (mut sink, _commands, _completions) = sink();
        let started = Instant::now();
        // No backend consumes these requests: the clipboard queue fills.
        for _ in 0..8 {
            write(&mut sink);
        }
        sink.handle(&[TYPE_MOUSE_REL, 1, 0, 0, 0]);
        sink.handle(&[TYPE_KEY_DOWN, 0x12, 0]); // Alt
        sink.handle(&[TYPE_KEY_DOWN, 9, 0]); // Tab
        sink.handle(&[TYPE_KEY_UP, 9, 0]);
        sink.handle(&[TYPE_KEY_UP, 0x12, 0]);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(sink.test_injections.as_ref().unwrap().len(), 5);
        assert!(sink.clipboard.as_ref().unwrap().paste.is_none());
    }

    #[test]
    fn successful_async_paste_survives_shortcut_keyup_and_happens_once() {
        let (mut sink, commands, completions) = sink();
        write(&mut sink);
        assert!(matches!(
            commands.try_recv(),
            Ok(ClipboardCommand::Write {
                epoch: 0,
                id: 1,
                ..
            })
        ));
        sink.handle(&[TYPE_KEY_UP, VK_CONTROL as u8, 0]);
        sink.test_injections.as_mut().unwrap().clear();
        completions
            .send(completed())
            .unwrap_or_else(|_| panic!("completion receiver gone"));
        sink.finish_clipboard_with_identity(|| (123, 99));
        let records = sink.test_injections.as_ref().unwrap();
        assert_eq!(records.len(), 4); // Ctrl down, V down/up, Ctrl up
        assert!(matches!(
            records[1],
            Record::Key {
                vk: VK_V,
                flags: 0,
                ..
            }
        ));
        sink.finish_clipboard_with_identity(|| (123, 99));
        assert_eq!(sink.test_injections.as_ref().unwrap().len(), 4);
    }

    #[test]
    fn delayed_paste_is_cancelled_by_new_actions_focus_expiry_or_failed_write() {
        for reason in [
            "key", "button", "scroll", "release", "geometry", "focus", "expired", "failure",
            "secure", "session",
        ] {
            let (mut sink, _commands, completions) = sink();
            write(&mut sink);
            match reason {
                "key" => {
                    sink.handle(&[TYPE_KEY_DOWN, 65, 0]);
                    sink.handle(&[TYPE_KEY_UP, 65, 0]);
                }
                "button" => {
                    sink.handle(&[TYPE_BUTTONS, 1]);
                    sink.handle(&[TYPE_BUTTONS, 0]);
                }
                "scroll" => {
                    sink.handle(&[TYPE_WHEEL, 120, 0]);
                }
                "release" => sink.release_all(),
                "geometry" => sink.set_absolute_region(DesktopRegion::new(1, 0, 1920, 1080)),
                "expired" => {
                    sink.clipboard
                        .as_mut()
                        .unwrap()
                        .paste
                        .as_mut()
                        .unwrap()
                        .expires = Instant::now() - Duration::from_secs(1)
                }
                "secure" => sink.set_secure_desktop(true, false),
                "session" => sink.reset_viewer_session(),
                _ => {}
            }
            sink.test_injections.as_mut().unwrap().clear();
            let completion = if reason == "failure" {
                Completion::Written {
                    epoch: 0,
                    id: 1,
                    success: false,
                }
            } else {
                completed()
            };
            completions
                .send(completion)
                .unwrap_or_else(|_| panic!("completion receiver gone"));
            sink.finish_clipboard_with_identity(|| {
                if reason == "focus" {
                    (456, 88)
                } else {
                    (123, 99)
                }
            });
            assert!(
                sink.test_injections.as_ref().unwrap().is_empty(),
                "{reason}"
            );
        }
    }

    #[test]
    fn reset_discards_old_clipboard_content_and_retries_a_refused_baseline() {
        let (mut sink, commands, completions) = sink();
        // Full queue must not consume the next session's reset/baseline.
        for _ in 0..4 {
            write(&mut sink);
        }
        sink.reset_viewer_session();
        assert_eq!(sink.poll_clipboard(true), None);
        assert!(sink.clipboard.as_ref().unwrap().reset_pending);
        for _ in 0..4 {
            commands
                .try_recv()
                .unwrap_or_else(|_| panic!("queued write missing"));
        }
        completions
            .send(Completion::Polled {
                epoch: 0,
                content: Some(clipboard::ClipboardContent::Text("old session".into())),
            })
            .unwrap_or_else(|_| panic!("completion receiver gone"));
        assert_eq!(sink.poll_clipboard(false), None);
        assert!(matches!(
            commands.try_recv(),
            Ok(ClipboardCommand::Poll {
                epoch: 1,
                reset: true
            })
        ));
    }

    #[test]
    fn first_baseline_poll_does_not_cancel_a_current_viewers_paste() {
        let (mut sink, _commands, completions) = sink();
        write(&mut sink);
        assert_eq!(sink.poll_clipboard(true), None);
        completions
            .send(completed())
            .unwrap_or_else(|_| panic!("completion receiver gone"));
        sink.finish_clipboard_with_identity(|| (123, 99));
        assert_eq!(sink.test_injections.as_ref().unwrap().len(), 4);
    }

    #[test]
    fn async_meta_paste_masks_and_releases_windows_before_the_write_wait() {
        let (mut sink, _commands, completions) = sink();
        sink.handle(&[TYPE_KEY_DOWN, VK_LWIN as u8, 0]);
        sink.test_injections.as_mut().unwrap().clear();
        write(&mut sink);
        sink.handle(&[TYPE_KEY_UP, VK_LWIN as u8, 0]);
        let records = sink.test_injections.as_ref().unwrap();
        assert_eq!(records.len(), 3, "mask tap and one Windows-key release");
        assert!(matches!(
            records[0],
            Record::Key {
                vk: VK_MASK,
                flags: 0,
                ..
            }
        ));
        assert!(matches!(
            records[2],
            Record::Key { vk: VK_LWIN, flags, .. } if flags == KEYEVENTF_KEYUP.0
        ));
        completions
            .send(completed())
            .unwrap_or_else(|_| panic!("completion receiver gone"));
        sink.finish_clipboard_with_identity(|| (123, 99));
        assert_eq!(sink.test_injections.as_ref().unwrap().len(), 7);
    }
}
