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
//! | 128  | cursor state (host -> browser) | u16 ack, x, y, u8 flags |
//!
//! Anti-cheat note: SendInput is the v0 injection method. Games with kernel
//! anti-cheat may ignore synthetic input or flag the process; accepted for v0.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use tracing::{debug, warn};
use windows::Win32::Foundation::{SetLastError, POINT, WIN32_ERROR};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
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
const MAX_TEXT_BYTES: usize = 512;
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
    pressed_keys: BTreeSet<u16>,
    buttons: u8,
    last_motion_sequence: Option<u16>,
    last_absolute_position: Option<(u16, u16)>,
}

impl InputState {
    fn key(&mut self, vk: u16, up: bool) {
        if up {
            self.pressed_keys.remove(&vk);
        } else {
            self.pressed_keys.insert(vk);
        }
    }

    fn take_release_plan(&mut self) -> (Vec<u16>, u8) {
        let keys = std::mem::take(&mut self.pressed_keys).into_iter().collect();
        let buttons = std::mem::replace(&mut self.buttons, 0);
        (keys, buttons)
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

#[cfg(test)]
mod tests {
    use super::{
        cursor_snapshot, decode, map_monitor_absolute_to_virtual, newer_sequence, Command,
        CursorState, DesktopRegion, InputFailureLogGate, InputState, INPUT_FAILURE_LOG_INTERVAL,
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
    fn release_plan_contains_each_held_key_and_pressed_button_once() {
        let mut state = InputState::default();
        state.key(0x41, false);
        state.key(0x10, false);
        state.key(0x41, false);
        state.buttons = 0b101;

        assert_eq!(state.take_release_plan(), (vec![0x10, 0x41], 0b101));
        assert_eq!(state.take_release_plan(), (Vec::new(), 0));
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
        }
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
                self.state.key(vk, up);
                self.key(vk, up);
            }
            Command::Rel(dx, dy) => {
                self.stats.relative_packets += 1;
                self.state.last_absolute_position = None;
                self.mouse_rel(dx, dy);
                outcome.cursor_changed = dx != 0 || dy != 0;
            }
            Command::Abs(x, y) => {
                self.stats.absolute_packets += 1;
                self.state.last_absolute_position = Some((x, y));
                self.mouse_abs(x, y);
                outcome.cursor_changed = true;
            }
            Command::Buttons(mask) => self.buttons(mask),
            Command::Wheel(delta) => self.wheel(delta),
            Command::ReleaseAll => self.release_all(),
            Command::Motion(seq, dx, dy, wx, wy) => {
                if self.state.accept_motion(seq) {
                    self.stats.relative_packets += 1;
                    self.state.last_absolute_position = None;
                    self.mouse_rel(dx, dy);
                    self.wheel_horizontal(wx);
                    self.wheel(wy);
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
                    self.mouse_abs(x, y);
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
                self.mouse_abs(x, y);
                self.buttons(mask);
                if let Some((latest_x, latest_y)) = restore {
                    self.mouse_abs(latest_x, latest_y);
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
            Command::Text(text) => self.text(&text),
        }
        outcome
    }

    pub fn take_stats(&mut self) -> InputStats {
        std::mem::take(&mut self.stats)
    }

    fn record_injection_result(&mut self, operation: &'static str, sent: u32, expected: u32) {
        if sent == expected {
            return;
        }
        // SendInput does not reliably set a useful error for UIPI rejection,
        // but capture it immediately: other desktop/session failures do.
        let error = std::io::Error::last_os_error();
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

    fn key(&mut self, vk: u16, up: bool) {
        let mut flags = windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS::default();
        if up {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SetLastError(WIN32_ERROR(0)) };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        self.record_injection_result(if up { "key-up" } else { "key-down" }, sent, 1);
    }

    fn text(&mut self, text: &str) {
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
        if !inputs.is_empty() {
            unsafe { SetLastError(WIN32_ERROR(0)) };
            let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
            self.record_injection_result("text", sent, inputs.len() as u32);
        }
    }

    fn mouse_rel(&mut self, dx: i16, dy: i16) {
        if dx == 0 && dy == 0 {
            return;
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
        unsafe { SetLastError(WIN32_ERROR(0)) };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        self.record_injection_result("mouse-relative", sent, 1);
    }

    fn mouse_abs(&mut self, x: u16, y: u16) {
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
        unsafe { SetLastError(WIN32_ERROR(0)) };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        self.record_injection_result("mouse-absolute", sent, 1);
    }

    /// Bitmask diff -> press/release events for each button.
    fn buttons(&mut self, mask: u8) {
        let changed = mask ^ self.state.buttons;
        self.state.buttons = mask;
        if changed == 0 {
            return;
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
        if !inputs.is_empty() {
            unsafe { SetLastError(WIN32_ERROR(0)) };
            let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
            self.record_injection_result("mouse-buttons", sent, inputs.len() as u32);
        }
    }

    fn wheel(&mut self, delta: i16) {
        if delta == 0 {
            return;
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
        unsafe { SetLastError(WIN32_ERROR(0)) };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        self.record_injection_result("mouse-wheel", sent, 1);
    }

    fn wheel_horizontal(&mut self, delta: i16) {
        if delta == 0 {
            return;
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
        unsafe { SetLastError(WIN32_ERROR(0)) };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        self.record_injection_result("mouse-wheel-horizontal", sent, 1);
    }

    /// Release input the browser may never get to send when its data channel
    /// or signaling session disappears. This is also called from Drop.
    pub fn release_all(&mut self) {
        let (keys, buttons) = self.state.take_release_plan();
        for vk in keys {
            self.key(vk, true);
        }
        if buttons != 0 {
            self.send_button_releases(buttons);
        }
    }

    fn send_button_releases(&mut self, buttons: u8) {
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
        if !inputs.is_empty() {
            unsafe { SetLastError(WIN32_ERROR(0)) };
            let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
            self.record_injection_result("mouse-button-release", sent, inputs.len() as u32);
        }
    }
}

impl Drop for InputSink {
    fn drop(&mut self) {
        self.release_all();
    }
}
