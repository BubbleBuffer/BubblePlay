//! "Follow active window" (`docs/architecture/follow-active-window.md`).
//!
//! The host samples the foreground window every 250 ms and moves the stream to
//! the monitor that holds most of it. [`FollowWindow`] is the pure decision:
//! edge-triggered on the (window, monitor) pair, confirmed by two consecutive
//! identical samples, skipped while a window is moved/resized or the left
//! button is down, and limited to one automatic switch per second.
//! [`sample_foreground`] is the thin Win32 sampler and [`selector_for`] maps
//! the sampled monitor to a capture `MonitorInfo::selector`.
//!
//! Edge hold ("Carrying a window to another monitor"): while a window is
//! being moved, [`observe_edge`] switches the stream to the monitor behind the
//! edge the cursor is held against, and [`carry_cursor`] places the cursor at
//! the same relative position on that monitor so the window comes along.
//! [`sample_move`] is its Win32 sampler.

use std::time::{Duration, Instant};

use crate::capture::MonitorInfo;

/// Version advertised as `followWindow` in `host-capabilities`.
pub const PROTOCOL_VERSION: u8 = 1;
/// Foreground sampling cadence.
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
/// At most one automatic switch per this interval.
pub const MIN_SWITCH_INTERVAL: Duration = Duration::from_secs(1);
/// Edge-hold sampling cadence, used only while a move loop is active.
pub const EDGE_SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
/// How long the cursor must stay at the edge before the stream switches.
pub const EDGE_HOLD: Duration = Duration::from_millis(250);
/// The edge zone: the cursor is at most this many pixels from the edge.
pub const EDGE_ZONE_PX: i32 = 2;

/// The monitor holding most of the foreground window, as Win32 reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowMonitor {
    /// `MONITORINFOEXW::szDevice`, e.g. `\\.\DISPLAY2`.
    pub device: String,
    pub origin_x: i32,
    pub origin_y: i32,
}

/// What identifies the foreground window in the log: its class name, the file name of the
/// owning process's executable and its pid. Never the title (private). Empty when a part
/// could not be read. Not part of the follow decision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowDetails {
    pub class: String,
    pub exe: String,
    pub pid: u32,
}

/// One foreground sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sample {
    /// A window is being moved or resized, or the left button is down: the
    /// decision waits for the drop.
    Busy,
    /// No window, or one the rules ignore (invisible, minimised, cloaked,
    /// desktop or taskbar).
    Ignored,
    Window {
        window: isize,
        monitor: WindowMonitor,
        details: WindowDetails,
    },
}

type Pair = (isize, WindowMonitor);

/// Per-session follow decision state.
#[derive(Debug, Default)]
pub struct FollowWindow {
    /// A new pair seen once, waiting for its confirming sample.
    candidate: Option<Pair>,
    /// The last confirmed pair. A pair is acted on once; a manual pick made
    /// after that therefore stays until the pair changes.
    handled: Option<Pair>,
    /// The details of the window whose pair was last confirmed.
    confirmed_details: WindowDetails,
    last_switch: Option<Instant>,
    edge: EdgeHold,
}

/// A monitor rectangle in desktop coordinates; `right`/`bottom` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, width: i32, height: i32) -> Self {
        Self {
            left,
            top,
            right: left + width,
            bottom: top + height,
        }
    }

    pub const fn contains(&self, (x, y): (i32, i32)) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    pub fn of(monitor: &MonitorInfo) -> Self {
        Self::new(
            monitor.origin_x,
            monitor.origin_y,
            i32::try_from(monitor.width).unwrap_or(i32::MAX),
            i32::try_from(monitor.height).unwrap_or(i32::MAX),
        )
    }
}

/// One edge-hold sample of the foreground move/size loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveSample {
    /// The window in the move/size loop.
    pub window: isize,
    /// Its size (width, height).
    pub size: (i32, i32),
    pub cursor: (i32, i32),
}

/// Edge-hold state for one move loop.
#[derive(Debug, Default)]
struct EdgeHold {
    /// The window and its size when the current move loop was first seen.
    loop_start: Option<(isize, (i32, i32))>,
    /// The size changed during this loop: a resize, never an edge switch.
    resized: bool,
    /// The cursor entered the edge zone towards this monitor at this time.
    zone: Option<(Instant, Rect)>,
    /// An edge switch happened; no other until the cursor left the zone.
    disarmed: bool,
}

/// The monitor directly behind the edge of `current` that `cursor` is within
/// [`EDGE_ZONE_PX`] of, at the cursor's position along that edge.
pub fn edge_target(cursor: (i32, i32), current: Rect, monitors: &[Rect]) -> Option<Rect> {
    if !current.contains(cursor) {
        return None;
    }
    let (x, y) = cursor;
    // The point just beyond each edge the cursor is near.
    let probes = [
        (x - current.left <= EDGE_ZONE_PX).then_some((current.left - 1, y)),
        (current.right - 1 - x <= EDGE_ZONE_PX).then_some((current.right, y)),
        (y - current.top <= EDGE_ZONE_PX).then_some((x, current.top - 1)),
        (current.bottom - 1 - y <= EDGE_ZONE_PX).then_some((x, current.bottom)),
    ];
    probes.into_iter().flatten().find_map(|probe| {
        monitors
            .iter()
            .find(|m| **m != current && m.contains(probe))
            .copied()
    })
}

/// Feed one edge-hold sample (only taken while a move loop may be active).
/// `follow` is `None` when follow active window is off. `moving` is `None`
/// when no move/size loop is active. Returns the monitor to switch to; the
/// caller switches, moves the cursor with [`carry_cursor`] and calls
/// [`FollowWindow::switched`].
pub fn observe_edge(
    follow: Option<&mut FollowWindow>,
    moving: Option<MoveSample>,
    monitors: &[Rect],
    current: Rect,
    now: Instant,
) -> Option<Rect> {
    let follow = follow?;
    let last_switch = follow.last_switch;
    let edge = &mut follow.edge;
    let Some(sample) = moving else {
        edge.loop_start = None;
        edge.resized = false;
        edge.zone = None;
        return None;
    };
    match edge.loop_start {
        Some((window, size)) if window == sample.window => {
            if size != sample.size {
                edge.resized = true;
            }
        }
        _ => {
            edge.loop_start = Some((sample.window, sample.size));
            edge.resized = false;
            edge.zone = None;
        }
    }
    let Some(target) = edge_target(sample.cursor, current, monitors) else {
        edge.zone = None;
        edge.disarmed = false;
        return None;
    };
    if edge.resized || edge.disarmed {
        edge.zone = None;
        return None;
    }
    let since = match edge.zone {
        Some((since, zone_target)) if zone_target == target => since,
        _ => {
            edge.zone = Some((now, target));
            now
        }
    };
    if now.saturating_duration_since(since) < EDGE_HOLD
        || last_switch.is_some_and(|at| now.saturating_duration_since(at) < MIN_SWITCH_INTERVAL)
    {
        return None;
    }
    edge.zone = None;
    edge.disarmed = true;
    // The drop that ends this loop is judged afresh by `observe`.
    follow.candidate = None;
    Some(target)
}

/// The cursor at the same relative position on `to` as it was on `from`,
/// clamped inside `to`.
pub fn carry_cursor(cursor: (i32, i32), from: Rect, to: Rect) -> (i32, i32) {
    fn axis(value: i32, from_lo: i32, from_hi: i32, to_lo: i32, to_hi: i32) -> i32 {
        let from_len = i64::from(from_hi - from_lo).max(1);
        let to_len = i64::from(to_hi - to_lo).max(1);
        let offset = i64::from(value - from_lo).clamp(0, from_len - 1);
        let mapped = i64::from(to_lo) + offset * to_len / from_len;
        mapped.clamp(i64::from(to_lo), i64::from(to_hi) - 1) as i32
    }
    (
        axis(cursor.0, from.left, from.right, to.left, to.right),
        axis(cursor.1, from.top, from.bottom, to.top, to.bottom),
    )
}

impl FollowWindow {
    /// Feed one sample. Returns the monitor to show when a new pair has been
    /// confirmed and the rate limit allows a switch. The caller compares it
    /// with the current capture monitor and calls [`Self::switched`] after an
    /// automatic switch.
    pub fn observe(&mut self, sample: Sample, now: Instant) -> Option<WindowMonitor> {
        let Sample::Window {
            window,
            monitor,
            details,
        } = sample
        else {
            self.candidate = None;
            return None;
        };
        let pair = (window, monitor);
        if self.handled.as_ref() == Some(&pair) {
            self.candidate = None;
            return None;
        }
        if self.candidate.as_ref() != Some(&pair) {
            self.candidate = Some(pair);
            return None;
        }
        if self
            .last_switch
            .is_some_and(|at| now.saturating_duration_since(at) < MIN_SWITCH_INTERVAL)
        {
            // Keep the candidate: the switch happens on a later sample if the
            // pair still holds.
            return None;
        }
        self.candidate = None;
        let target = pair.1.clone();
        self.handled = Some(pair);
        self.confirmed_details = details;
        Some(target)
    }

    /// The class, executable and pid of the window behind the monitor the last
    /// [`Self::observe`] returned.
    pub fn confirmed_details(&self) -> &WindowDetails {
        &self.confirmed_details
    }

    /// Record an automatic switch for the rate limit.
    pub fn switched(&mut self, now: Instant) {
        self.last_switch = Some(now);
    }
}

/// Window classes the rules ignore: the desktop and the taskbars.
pub fn is_shell_class(class: &str) -> bool {
    matches!(
        class,
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

/// Map a sampled monitor to a capture selector. Capture selectors are the
/// monitor device names (`enumerate_monitors`), so the device name decides;
/// the origin is a fallback for a monitor whose name could not be matched.
pub fn selector_for(monitors: &[MonitorInfo], target: &WindowMonitor) -> Option<String> {
    monitors
        .iter()
        .find(|m| !m.selector.is_empty() && m.selector.eq_ignore_ascii_case(&target.device))
        .or_else(|| {
            monitors
                .iter()
                .find(|m| m.origin_x == target.origin_x && m.origin_y == target.origin_y)
        })
        .map(|m| m.selector.clone())
}

/// Sample the foreground window (Win32; interactive session only).
pub fn sample_foreground() -> Sample {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, IsIconic, IsWindowVisible,
        GUITHREADINFO, GUI_INMOVESIZE,
    };

    // SAFETY: plain Win32 queries with valid out-pointers sized by cbSize.
    unsafe {
        if GetAsyncKeyState(i32::from(VK_LBUTTON.0)) as u16 & 0x8000 != 0 {
            return Sample::Busy;
        }
        let mut gui = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        // Thread 0 = the foreground thread, which owns any move/size loop.
        if GetGUIThreadInfo(0, &raw mut gui).is_ok() && gui.flags.contains(GUI_INMOVESIZE) {
            return Sample::Busy;
        }
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.is_invalid() || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return Sample::Ignored;
        }
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return Sample::Ignored;
        }
        let mut class_buf = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut class_buf);
        let class = if len > 0 {
            String::from_utf16_lossy(&class_buf[..len as usize])
        } else {
            String::new()
        };
        if is_shell_class(&class) {
            return Sample::Ignored;
        }
        let hmonitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if !GetMonitorInfoW(hmonitor, (&raw mut info).cast()).as_bool() {
            return Sample::Ignored;
        }
        let end = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        Sample::Window {
            window: hwnd.0 as isize,
            monitor: WindowMonitor {
                device: String::from_utf16_lossy(&info.szDevice[..end]),
                origin_x: info.monitorInfo.rcMonitor.left,
                origin_y: info.monitorInfo.rcMonitor.top,
            },
            details: details_of(hwnd, class),
        }
    }
}

/// The owning process of `hwnd`, with its executable looked up only when the window or
/// process changed since the last sample (the sampler runs four times a second).
fn details_of(hwnd: windows::Win32::Foundation::HWND, class: String) -> WindowDetails {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    thread_local! {
        static LAST: std::cell::RefCell<Option<(isize, u32, String)>> =
            const { std::cell::RefCell::new(None) };
    }
    let mut pid = 0u32;
    // SAFETY: plain Win32 query with a valid out-pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
    let key = hwnd.0 as isize;
    let exe = LAST.with(|last| {
        let mut last = last.borrow_mut();
        match last.as_ref() {
            Some((h, p, exe)) if *h == key && *p == pid => exe.clone(),
            _ => {
                let exe = exe_file_name(pid);
                *last = Some((key, pid, exe.clone()));
                exe
            }
        }
    });
    WindowDetails { class, exe, pid }
}

/// File name (no directory) of the executable of process `pid`; empty when unreadable.
fn exe_file_name(pid: u32) -> String {
    crate::admin_agent::trust::process_image_path(pid)
        .and_then(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// Sample the foreground move/size loop for edge hold (Win32; interactive
/// session only). `None` when no loop is active.
pub fn sample_move() -> Option<MoveSample> {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetGUIThreadInfo, GetWindowRect, GUITHREADINFO, GUI_INMOVESIZE,
    };

    // SAFETY: plain Win32 queries with valid out-pointers sized by cbSize.
    unsafe {
        let mut gui = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(0, &raw mut gui).is_err()
            || !gui.flags.contains(GUI_INMOVESIZE)
            || gui.hwndMoveSize.is_invalid()
        {
            return None;
        }
        let mut rect = RECT::default();
        GetWindowRect(gui.hwndMoveSize, &raw mut rect).ok()?;
        let mut point = POINT::default();
        GetCursorPos(&raw mut point).ok()?;
        Some(MoveSample {
            window: gui.hwndMoveSize.0 as isize,
            size: (rect.right - rect.left, rect.bottom - rect.top),
            cursor: (point.x, point.y),
        })
    }
}

/// Put the cursor at `(x, y)` in desktop coordinates (Win32).
pub fn set_cursor(x: i32, y: i32) -> bool {
    // SAFETY: plain Win32 call without pointers.
    unsafe { windows::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y).is_ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(n: u8) -> WindowMonitor {
        WindowMonitor {
            device: format!(r"\\.\DISPLAY{n}"),
            origin_x: (i32::from(n) - 1) * 1920,
            origin_y: 0,
        }
    }

    fn win(window: isize, n: u8) -> Sample {
        Sample::Window {
            window,
            monitor: mon(n),
            details: WindowDetails::default(),
        }
    }

    fn win_with(window: isize, n: u8, class: &str, exe: &str, pid: u32) -> Sample {
        Sample::Window {
            window,
            monitor: mon(n),
            details: WindowDetails {
                class: class.into(),
                exe: exe.into(),
                pid,
            },
        }
    }

    fn tick(start: Instant, step: u32) -> Instant {
        start + SAMPLE_INTERVAL * step
    }

    #[test]
    fn switches_after_two_identical_samples() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        assert_eq!(f.observe(win(1, 2), tick(t, 0)), None);
        assert_eq!(f.observe(win(1, 2), tick(t, 1)), Some(mon(2)));
        f.switched(tick(t, 1));
        // The same pair never fires again.
        assert_eq!(f.observe(win(1, 2), tick(t, 2)), None);
        assert_eq!(f.observe(win(1, 2), tick(t, 20)), None);
    }

    #[test]
    fn confirmed_details_are_those_of_the_confirmed_window() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        assert_eq!(
            f.observe(
                win_with(5, 2, "CabinetWClass", "explorer.exe", 4242),
                tick(t, 0)
            ),
            None
        );
        assert_eq!(
            f.observe(
                win_with(5, 2, "CabinetWClass", "explorer.exe", 4242),
                tick(t, 1)
            ),
            Some(mon(2))
        );
        assert_eq!(
            f.confirmed_details(),
            &WindowDetails {
                class: "CabinetWClass".into(),
                exe: "explorer.exe".into(),
                pid: 4242,
            }
        );
    }

    #[test]
    fn a_flicker_does_not_confirm() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        assert_eq!(f.observe(win(1, 1), tick(t, 0)), None);
        assert_eq!(f.observe(win(2, 2), tick(t, 1)), None);
        assert_eq!(f.observe(win(1, 1), tick(t, 2)), None);
        assert_eq!(f.observe(win(1, 1), tick(t, 3)), Some(mon(1)));
    }

    #[test]
    fn busy_and_ignored_samples_reset_the_candidate() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        assert_eq!(f.observe(win(1, 2), tick(t, 0)), None);
        assert_eq!(f.observe(Sample::Busy, tick(t, 1)), None);
        assert_eq!(f.observe(win(1, 2), tick(t, 2)), None);
        assert_eq!(f.observe(Sample::Ignored, tick(t, 3)), None);
        assert_eq!(f.observe(win(1, 2), tick(t, 4)), None);
        assert_eq!(f.observe(win(1, 2), tick(t, 5)), Some(mon(2)));
    }

    #[test]
    fn decision_waits_for_the_drop() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        assert_eq!(f.observe(win(7, 1), tick(t, 0)), None);
        assert_eq!(f.observe(win(7, 1), tick(t, 1)), Some(mon(1)));
        f.switched(tick(t, 1));
        // Dragging across: every sample is busy, nothing switches.
        for step in 2..10 {
            assert_eq!(f.observe(Sample::Busy, tick(t, step)), None);
        }
        // Dropped mostly on monitor 2.
        assert_eq!(f.observe(win(7, 2), tick(t, 10)), None);
        assert_eq!(f.observe(win(7, 2), tick(t, 11)), Some(mon(2)));
    }

    #[test]
    fn manual_pick_is_not_undone_until_the_pair_changes() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        f.observe(win(1, 1), tick(t, 0));
        assert_eq!(f.observe(win(1, 1), tick(t, 1)), Some(mon(1)));
        // User picks monitor 2 manually; the focused window stays on 1.
        for step in 2..20 {
            assert_eq!(f.observe(win(1, 1), tick(t, step)), None);
        }
        // Focus moves to another window on monitor 1: a new pair.
        f.observe(win(3, 1), tick(t, 20));
        assert_eq!(f.observe(win(3, 1), tick(t, 21)), Some(mon(1)));
    }

    #[test]
    fn returning_to_the_handled_pair_after_the_desktop_is_no_change() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        f.observe(win(1, 1), tick(t, 0));
        assert!(f.observe(win(1, 1), tick(t, 1)).is_some());
        f.observe(Sample::Ignored, tick(t, 2));
        f.observe(Sample::Ignored, tick(t, 3));
        assert_eq!(f.observe(win(1, 1), tick(t, 4)), None);
        assert_eq!(f.observe(win(1, 1), tick(t, 5)), None);
    }

    #[test]
    fn at_most_one_automatic_switch_per_second() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        f.observe(win(1, 1), tick(t, 0));
        assert_eq!(f.observe(win(1, 1), tick(t, 1)), Some(mon(1)));
        f.switched(tick(t, 1));
        f.observe(win(2, 2), tick(t, 2));
        // Confirmed at 500 ms after the last switch: held back.
        assert_eq!(f.observe(win(2, 2), tick(t, 3)), None);
        assert_eq!(f.observe(win(2, 2), tick(t, 4)), None);
        // 1 s after the last switch the held pair goes through.
        assert_eq!(f.observe(win(2, 2), tick(t, 5)), Some(mon(2)));
    }

    #[test]
    fn rate_limit_counts_only_real_switches() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        f.observe(win(1, 1), tick(t, 0));
        assert!(f.observe(win(1, 1), tick(t, 1)).is_some());
        // No `switched`: the window was already on the captured monitor.
        f.observe(win(2, 2), tick(t, 2));
        assert_eq!(f.observe(win(2, 2), tick(t, 3)), Some(mon(2)));
    }

    #[test]
    fn shell_windows_are_ignored() {
        for class in [
            "Progman",
            "WorkerW",
            "Shell_TrayWnd",
            "Shell_SecondaryTrayWnd",
        ] {
            assert!(is_shell_class(class));
        }
        assert!(!is_shell_class("Notepad"));
        assert!(!is_shell_class("CabinetWClass"));
    }

    fn info(selector: &str, x: i32) -> MonitorInfo {
        MonitorInfo {
            selector: selector.into(),
            name: selector.into(),
            width: 1920,
            height: 1080,
            origin_x: x,
            origin_y: 0,
            selected: false,
        }
    }

    #[test]
    fn maps_monitor_by_device_name_then_origin() {
        let monitors = [info(r"\\.\DISPLAY1", 0), info(r"\\.\DISPLAY2", 1920)];
        assert_eq!(
            selector_for(&monitors, &mon(2)).as_deref(),
            Some(r"\\.\DISPLAY2")
        );
        let lower = WindowMonitor {
            device: r"\\.\display1".into(),
            origin_x: 5000,
            origin_y: 0,
        };
        assert_eq!(
            selector_for(&monitors, &lower).as_deref(),
            Some(r"\\.\DISPLAY1")
        );
        let renamed = WindowMonitor {
            device: r"\\.\DISPLAY9".into(),
            origin_x: 1920,
            origin_y: 0,
        };
        assert_eq!(
            selector_for(&monitors, &renamed).as_deref(),
            Some(r"\\.\DISPLAY2")
        );
        let unknown = WindowMonitor {
            device: r"\\.\DISPLAY9".into(),
            origin_x: -1,
            origin_y: 0,
        };
        assert_eq!(selector_for(&monitors, &unknown), None);
    }

    // Edge hold. Monitor 1 at the origin, monitor 2 to its right; monitor 3
    // below monitor 1 covers only its left half.
    const M1: Rect = Rect::new(0, 0, 1920, 1080);
    const M2: Rect = Rect::new(1920, 0, 2560, 1440);
    const M3: Rect = Rect::new(0, 1080, 960, 1080);
    const ALL: [Rect; 3] = [M1, M2, M3];

    fn moving(cursor: (i32, i32)) -> Option<MoveSample> {
        Some(MoveSample {
            window: 7,
            size: (800, 600),
            cursor,
        })
    }

    fn ms(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    /// Hold the cursor at `cursor` from `from` ms to `to` ms in 50 ms steps;
    /// returns the first switch and when it happened.
    fn hold(
        f: &mut FollowWindow,
        t: Instant,
        cursor: (i32, i32),
        current: Rect,
        from: u64,
        to: u64,
    ) -> Option<(u64, Rect)> {
        (from..=to).step_by(50).find_map(|at| {
            observe_edge(Some(f), moving(cursor), &ALL, current, ms(t, at)).map(|r| (at, r))
        })
    }

    #[test]
    fn edge_hold_shorter_than_250_ms_does_nothing() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        assert_eq!(hold(&mut f, t, (1919, 500), M1, 0, 200), None);
        // Leaving the edge before 250 ms restarts the hold.
        assert_eq!(
            observe_edge(Some(&mut f), moving((1000, 500)), &ALL, M1, ms(t, 250)),
            None
        );
        assert_eq!(hold(&mut f, t, (1919, 500), M1, 300, 500), None);
    }

    #[test]
    fn edge_hold_of_250_ms_with_a_neighbour_switches() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        assert_eq!(hold(&mut f, t, (1918, 500), M1, 0, 1000), Some((250, M2)));
        // Monitor 2's left edge leads back to monitor 1.
        let mut f = FollowWindow::default();
        assert_eq!(hold(&mut f, t, (1920, 100), M2, 0, 1000), Some((250, M1)));
        // Bottom edge of monitor 1 over monitor 3.
        let mut f = FollowWindow::default();
        assert_eq!(hold(&mut f, t, (300, 1079), M1, 0, 1000), Some((250, M3)));
    }

    #[test]
    fn edge_without_a_neighbour_does_nothing() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        assert_eq!(hold(&mut f, t, (0, 500), M1, 0, 2000), None);
        assert_eq!(hold(&mut f, t, (500, 0), M1, 0, 2000), None);
        // 3 px from the edge is outside the zone.
        assert_eq!(hold(&mut f, t, (1916, 500), M1, 0, 2000), None);
    }

    #[test]
    fn neighbour_not_behind_the_cursor_does_nothing() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        // Monitor 3 lies below the left half of monitor 1 only.
        assert_eq!(hold(&mut f, t, (1500, 1079), M1, 0, 2000), None);
        // Monitor 2 is taller than monitor 1: its right edge below y=1080 has
        // nothing behind it, and its left edge there has nothing either.
        assert_eq!(hold(&mut f, t, (1920, 1300), M2, 0, 2000), None);
    }

    #[test]
    fn a_resize_during_the_loop_does_nothing() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        assert_eq!(hold(&mut f, t, (1000, 500), M1, 0, 100), None);
        let resized = Some(MoveSample {
            window: 7,
            size: (900, 600),
            cursor: (1919, 500),
        });
        for at in (150..2000).step_by(50) {
            assert_eq!(
                observe_edge(Some(&mut f), resized, &ALL, M1, ms(t, at)),
                None
            );
        }
        // Back to the loop's starting size: still the same (resize) loop.
        assert_eq!(hold(&mut f, t, (1919, 500), M1, 2000, 3000), None);
        // A new loop moves again.
        assert_eq!(
            observe_edge(Some(&mut f), None, &ALL, M1, ms(t, 3050)),
            None
        );
        assert_eq!(
            hold(&mut f, t, (1919, 500), M1, 3100, 4000),
            Some((3350, M2))
        );
    }

    #[test]
    fn no_second_switch_until_the_cursor_left_the_edge_zone() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        assert_eq!(hold(&mut f, t, (1919, 500), M1, 0, 250), Some((250, M2)));
        f.switched(ms(t, 250));
        // Suppose the stream switched but the cursor is still at monitor 1's
        // edge (current stays M1 here): no repeat, however long.
        assert_eq!(hold(&mut f, t, (1919, 500), M1, 300, 5000), None);
        // The cursor leaves the zone; holding again switches again.
        assert_eq!(
            observe_edge(Some(&mut f), moving((1800, 500)), &ALL, M1, ms(t, 5050)),
            None
        );
        assert_eq!(
            hold(&mut f, t, (1919, 500), M1, 5100, 6000),
            Some((5350, M2))
        );
    }

    #[test]
    fn edge_hold_shares_the_one_switch_per_second_limit() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        // A focus switch just happened.
        f.switched(ms(t, 0));
        assert_eq!(hold(&mut f, t, (1919, 500), M1, 0, 2000), Some((1000, M2)));
    }

    #[test]
    fn edge_hold_needs_follow_and_a_move_loop() {
        let t = Instant::now();
        for at in (0..2000).step_by(50) {
            assert_eq!(
                observe_edge(None, moving((1919, 500)), &ALL, M1, ms(t, at)),
                None
            );
        }
        let mut f = FollowWindow::default();
        for at in (0..2000).step_by(50) {
            assert_eq!(observe_edge(Some(&mut f), None, &ALL, M1, ms(t, at)), None);
        }
    }

    #[test]
    fn the_drop_after_an_edge_switch_does_not_switch_back() {
        let t = Instant::now();
        let mut f = FollowWindow::default();
        f.observe(win(7, 1), tick(t, 0));
        assert_eq!(f.observe(win(7, 1), tick(t, 1)), Some(mon(1)));
        f.switched(tick(t, 1));
        let start = 2 * 250;
        assert_eq!(
            hold(&mut f, t, (1919, 500), M1, start + 1000, start + 2000),
            Some((start + 1250, M2))
        );
        f.switched(ms(t, start + 1250));
        // Dropped on monitor 2, the now-streamed monitor: the drop rule names
        // monitor 2 (the caller sees it is already streamed) and never 1.
        f.observe(Sample::Busy, ms(t, start + 1300));
        f.observe(win(7, 2), ms(t, start + 1350));
        let after = f.observe(win(7, 2), ms(t, start + 2350));
        assert_eq!(after, Some(mon(2)));
        assert_eq!(f.observe(win(7, 2), ms(t, start + 2600)), None);
    }

    #[test]
    fn the_cursor_is_carried_to_the_same_relative_position() {
        // Right edge of monitor 1 -> right edge of the larger monitor 2.
        assert_eq!(carry_cursor((1919, 540), M1, M2), (1920 + 2558, 720));
        // Left edge of monitor 2 -> left edge of monitor 1.
        assert_eq!(carry_cursor((1920, 0), M2, M1), (0, 0));
        // Clamped inside the target.
        assert_eq!(carry_cursor((5000, -5), M1, M3), (959, 1080));
        assert_eq!(carry_cursor((960, 1079), M1, M3), (480, 2159));
    }
}
