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
//!
//! Anti-cheat note: SendInput is the v0 injection method. Games with kernel
//! anti-cheat may ignore synthetic input or flag the process; accepted for v0.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::clipboard;
use windows::Win32::Foundation::{SetLastError, POINT, WIN32_ERROR};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetCursorPos, GetSystemMetrics, CURSORINFO, CURSOR_SHOWING, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
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
const CURSOR_ACK_VALID: u8 = 1;
const CURSOR_IN_BOUNDS: u8 = 2;
const CURSOR_VISIBLE: u8 = 4;
// Work mode treats the browser overlay as authoritative. A 10 Hz heartbeat
// lets it detect an OS cursor displaced by local input without flooding the
// control channel; changed positions are still emitted immediately.
const CURSOR_HEARTBEAT: Duration = Duration::from_millis(100);

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

fn newer_sequence(last: Option<u16>, next: u16) -> bool {
    last.map(|old| {
        let delta = next.wrapping_sub(old);
        delta != 0 && delta < 0x8000
    })
    .unwrap_or(true)
}

pub struct InputSink {
    state: InputState,
    stats: InputStats,
    absolute_region: DesktopRegion,
    failure_log: InputFailureLogGate,
    clipboard: Option<ClipboardSync>,
}

/// Clipboard state exists only where the sink runs as the interactive user
/// (input proxy, unprivileged local host). Elsewhere clipboard packets are
/// ignored.
#[derive(Debug, Default)]
struct ClipboardSync {
    assembly: ClipboardAssembly,
    watcher: clipboard::ClipboardWatcher,
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
        data[7] = u8::from(self.acknowledged_sequence.is_some()) * CURSOR_ACK_VALID
            | u8::from(self.in_bounds) * CURSOR_IN_BOUNDS
            | u8::from(self.visible) * CURSOR_VISIBLE;
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

    fn accept_motion(&mut self, sequence: u16) -> bool {
        if !newer_sequence(self.last_motion_sequence, sequence) {
            return false;
        }
        self.last_motion_sequence = Some(sequence);
        true
    }

    fn advance_motion_barrier(&mut self, sequence: u16) -> bool {
        if newer_sequence(self.last_motion_sequence, sequence) {
            self.last_motion_sequence = Some(sequence);
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
    use super::{
        cursor_snapshot, decode, keyboard_input, map_monitor_absolute_to_virtual, newer_sequence,
        ClipboardAssembly, ClipboardChunk, ClipboardProgress, Command, CursorState, DesktopRegion,
        HeldKey, InputApplication, InputFailureLogGate, InputState, CLIPBOARD_CHUNK_BYTES,
        CLIPBOARD_MAX_CHUNKS, INPUT_FAILURE_LOG_INTERVAL, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
        KEYEVENTF_SCANCODE, SCAN_LCONTROL, SCAN_LWIN, VK_CONTROL, VK_MASK, VK_V,
    };
    use std::time::Instant;

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
}

impl InputSink {
    pub fn new() -> Self {
        Self {
            state: InputState::default(),
            stats: InputStats::default(),
            absolute_region: DesktopRegion::new(0, 0, 1920, 1080),
            failure_log: InputFailureLogGate::default(),
            clipboard: None,
        }
    }

    /// Enable text clipboard sync. Only call from a process running as the
    /// interactive user on the default desktop.
    pub fn with_clipboard(mut self, enabled: bool) -> Self {
        self.clipboard = enabled.then(ClipboardSync::default);
        self
    }

    /// New host clipboard text since the previous poll (never echoes text
    /// this sink set from the browser). `reset` starts a new baseline.
    pub fn poll_clipboard(&mut self, reset: bool) -> Option<String> {
        self.clipboard.as_mut()?.watcher.poll(reset)
    }

    pub fn set_absolute_region(&mut self, region: DesktopRegion) {
        self.absolute_region = region.sanitized();
        self.state.last_absolute_position = None;
    }

    /// Handle one data-channel packet. Malformed packets are logged and dropped.
    pub fn handle(&mut self, data: &[u8]) -> InputOutcome {
        let Some(command) = decode(data) else {
            debug!(?data, "malformed or unknown input packet");
            return InputOutcome::default();
        };
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
                    outcome.application.merge(self.wheel_horizontal(wx));
                    outcome.application.merge(self.wheel(wy));
                    outcome.accepted_sequence = Some(seq);
                    outcome.cursor_changed = dx != 0 || dy != 0;
                } else {
                    self.stats.stale_motion_dropped += 1;
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

    fn send_inputs(&mut self, operation: &'static str, inputs: &[INPUT]) -> InputApplication {
        if inputs.is_empty() {
            return InputApplication::default();
        }
        unsafe { SetLastError(WIN32_ERROR(0)) };
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        let expected = inputs.len().min(u32::MAX as usize) as u32;
        // SendInput does not reliably set a useful error for UIPI rejection,
        // but capture it immediately: other desktop/session failures do.
        let error = std::io::Error::last_os_error();
        let error_code = error.raw_os_error().unwrap_or(0);
        if sent != expected {
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
            debug!("clipboard packet ignored: clipboard sync is disabled for this sink");
            return InputApplication::default();
        };
        let (text, paste) = match sync.assembly.push(chunk) {
            ClipboardProgress::Pending => return InputApplication::default(),
            ClipboardProgress::Rejected => {
                warn!("browser clipboard transfer rejected (out of order, oversized or not UTF-8)");
                return InputApplication::default();
            }
            ClipboardProgress::Complete { text, paste } => (text, paste),
        };
        match clipboard::set_text(&text) {
            Ok(sequence) => sync.watcher.note_own_change(sequence),
            Err(error) => {
                warn!(?error, "failed to set host clipboard from browser");
                let code = error
                    .downcast_ref::<windows::core::Error>()
                    .map_or(0, |error| error.code().0);
                let mut application = InputApplication::default();
                application.observe(1, 0, code);
                return application;
            }
        }
        if !paste {
            return InputApplication::default();
        }
        let inputs: Vec<INPUT> = self
            .state
            .paste_plan()
            .into_iter()
            .map(|(key, up)| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: keyboard_input(key, up),
                },
            })
            .collect();
        self.send_inputs("clipboard-paste", &inputs)
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
        let virtual_desktop = virtual_desktop_region().unwrap_or(self.absolute_region);
        let (x, y) = map_monitor_absolute_to_virtual(x, y, self.absolute_region, virtual_desktop);
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

    pub fn release_all_report(&mut self) -> InputApplication {
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
    }
}
