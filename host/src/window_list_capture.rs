//! App focus mode: the Win32 side of the window list (`docs/architecture/app-focus-mode.md`).
//!
//! Runs on the `window-list` worker thread (`window_list_service.rs`), which is per-monitor DPI
//! aware so rectangles are in physical pixels like the captured frame. Enumerates top-level
//! windows in z-order, reads their icons once, and focuses a window on request. Titles are read
//! without sending messages to other processes; icons are asked for with a short timeout that
//! gives up on hung apps.

use std::collections::{HashMap, HashSet};

use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EnumDisplayMonitors, GdiFlush,
    GetMonitorInfoW, MonitorFromWindow, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DrawIconEx, EnumWindows, GetClassLongPtrW, GetClassNameW,
    GetForegroundWindow, GetWindow, GetWindowLongPtrW, GetWindowPlacement, GetWindowRect,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
    SendMessageTimeoutW, SetForegroundWindow, ShowWindow, DI_NORMAL, GCLP_HICON, GCLP_HICONSM,
    GWL_EXSTYLE, GW_OWNER, HICON, ICON_BIG, ICON_SMALL, ICON_SMALL2, SMTO_ABORTIFHUNG, SMTO_BLOCK,
    SW_RESTORE, WINDOWPLACEMENT, WM_GETICON, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

use crate::capture::MonitorInfo;
use crate::follow_window::{exe_file_name, Rect};
use crate::window_list::{is_switchable, unblend, FocusFailure, IconCache, RawWindow, ICON_SIZE};
use crate::window_list_service::{Backend, Snapshot};

/// How long one app may take to answer an icon request.
const ICON_TIMEOUT_MS: u32 = 50;
/// Longest title read, in UTF-16 units; the list cuts titles further.
const TITLE_UNITS: usize = 512;

/// The Win32 [`Backend`].
#[derive(Default)]
pub struct Win32Backend {
    icons: IconCache,
    /// Icon hash per window, read once per window.
    window_icons: HashMap<u32, Option<String>>,
    dpi_aware: bool,
}

/// Window handles fit in 32 bits on 64-bit Windows and are sign-extended back.
fn hwnd_of(id: u32) -> HWND {
    HWND(id as i32 as isize as *mut core::ffi::c_void)
}

fn id_of(hwnd: HWND) -> u32 {
    hwnd.0 as usize as u32
}

unsafe extern "system" fn collect_window(hwnd: HWND, data: LPARAM) -> BOOL {
    // SAFETY: `data` is the Vec passed by `top_level_windows`, alive for the call.
    unsafe { (*(data.0 as *mut Vec<HWND>)).push(hwnd) };
    BOOL(1)
}

/// A failed Win32 cleanup or best-effort call leaves nothing to undo; note it for diagnosis.
fn note(ok: bool, call: &'static str) {
    if !ok {
        tracing::debug!(call, "window list: Win32 call failed");
    }
}

/// Top-level windows in z-order, topmost first.
fn top_level_windows() -> Vec<HWND> {
    let mut list: Vec<HWND> = Vec::new();
    // SAFETY: the callback only runs during this call and `list` outlives it.
    let listed = unsafe { EnumWindows(Some(collect_window), LPARAM((&raw mut list) as isize)) };
    note(listed.is_ok(), "EnumWindows");
    list
}

fn rect_of(r: RECT) -> Rect {
    Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

/// The visible frame, without the invisible resize borders; the window rectangle as a fallback.
fn frame_rect(hwnd: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    // SAFETY: `r` is a RECT-sized out buffer.
    let framed = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut r).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    };
    // SAFETY: as above.
    if framed.is_ok() || unsafe { GetWindowRect(hwnd, &mut r) }.is_ok() {
        Some(rect_of(r))
    } else {
        None
    }
}

/// Where a minimized window goes when restored.
fn restored_rect(hwnd: HWND) -> Option<Rect> {
    let mut placement = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: `placement` is sized by `length`.
    unsafe { GetWindowPlacement(hwnd, &mut placement) }.ok()?;
    Some(rect_of(placement.rcNormalPosition))
}

fn class_name(hwnd: HWND) -> String {
    let mut buffer = [0u16; 128];
    // SAFETY: the buffer is valid for the call.
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

/// The caption. For another process's window this reads the stored text and sends no message.
fn title(hwnd: HWND) -> String {
    let mut buffer = [0u16; TITLE_UNITS];
    // SAFETY: the buffer is valid for the call.
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

fn describe(hwnd: HWND, exes: &mut HashMap<u32, String>) -> Option<RawWindow> {
    // SAFETY: plain queries on a handle that may have gone stale, which they tolerate.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return None;
        }
        let mut pid = 0u32;
        let thread = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let owner = GetWindow(hwnd, GW_OWNER).map_or(0, id_of);
        let mut cloaked = 0u32;
        let cloaked_read = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok();
        let minimized = IsIconic(hwnd).as_bool();
        let rect = if minimized {
            restored_rect(hwnd)
        } else {
            frame_rect(hwnd)
        }?;
        Some(RawWindow {
            id: id_of(hwnd),
            owner,
            thread,
            pid,
            class: class_name(hwnd),
            exe: exes
                .entry(pid)
                .or_insert_with(|| exe_file_name(pid))
                .clone(),
            title: title(hwnd),
            visible: true,
            cloaked: cloaked_read && cloaked != 0,
            minimized,
            tool_window: ex & WS_EX_TOOLWINDOW.0 != 0,
            app_window: ex & WS_EX_APPWINDOW.0 != 0,
            no_activate: ex & WS_EX_NOACTIVATE.0 != 0,
            rect,
            icon: None,
        })
    }
}

fn device_of(info: &MONITORINFOEXW) -> String {
    let end = info
        .szDevice
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(info.szDevice.len());
    String::from_utf16_lossy(&info.szDevice[..end])
}

fn monitor_info(monitor: HMONITOR) -> Option<MONITORINFOEXW> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `info` is sized by cbSize.
    unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast()) }
        .as_bool()
        .then_some(info)
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the Vec passed by `monitors`, alive for the call.
    let list = unsafe { &mut *(data.0 as *mut Vec<MonitorInfo>) };
    if let Some(info) = monitor_info(monitor) {
        let r = info.monitorInfo.rcMonitor;
        let device = device_of(&info);
        list.push(MonitorInfo {
            selector: device.clone(),
            name: device,
            width: (r.right - r.left).max(0) as u32,
            height: (r.bottom - r.top).max(0) as u32,
            origin_x: r.left,
            origin_y: r.top,
            selected: false,
        });
    }
    BOOL(1)
}

/// Attached monitors by device name, which is what capture uses as the selector.
fn monitors() -> Vec<MonitorInfo> {
    let mut list: Vec<MonitorInfo> = Vec::new();
    // SAFETY: the callback only runs during this call and `list` outlives it.
    let listed = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM((&raw mut list) as isize),
        )
    };
    note(listed.as_bool(), "EnumDisplayMonitors");
    list
}

/// The window's icon handle; the handle belongs to the window or its class, not to us.
fn icon_handle(hwnd: HWND) -> Option<HICON> {
    for kind in [ICON_BIG, ICON_SMALL2, ICON_SMALL] {
        let mut result = 0usize;
        // SAFETY: `result` is a valid out pointer; the call gives up on hung apps.
        let answered = unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_GETICON,
                WPARAM(kind as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                ICON_TIMEOUT_MS,
                Some(&mut result),
            )
        };
        if answered.0 != 0 && result != 0 {
            return Some(HICON(result as *mut core::ffi::c_void));
        }
    }
    [GCLP_HICON, GCLP_HICONSM].into_iter().find_map(|index| {
        // SAFETY: reads the window class's icon handle.
        let handle = unsafe { GetClassLongPtrW(hwnd, index) };
        (handle != 0).then_some(HICON(handle as *mut core::ffi::c_void))
    })
}

/// The icon at [`ICON_SIZE`] as RGBA, drawn on black and on white to recover transparency.
fn render_icon(icon: HICON) -> Option<Vec<u8>> {
    let size = ICON_SIZE as i32;
    let bytes = (ICON_SIZE * ICON_SIZE * 4) as usize;
    // SAFETY: every GDI object made here is released below; `bits` is the DIB's storage of
    // `bytes` bytes while the bitmap lives, and is only read after GdiFlush.
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: bytes as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut::<core::ffi::c_void>();
        let rgba = match CreateDIBSection(
            Some(dc),
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            None::<HANDLE>,
            0,
        ) {
            Ok(bitmap) => {
                let previous = SelectObject(dc, bitmap.into());
                let draw = |background: u8| -> Option<Vec<u8>> {
                    if bits.is_null() {
                        return None;
                    }
                    std::ptr::write_bytes(bits.cast::<u8>(), background, bytes);
                    DrawIconEx(dc, 0, 0, icon, size, size, 0, None, DI_NORMAL).ok()?;
                    if !GdiFlush().as_bool() {
                        return None;
                    }
                    Some(std::slice::from_raw_parts(bits.cast::<u8>(), bytes).to_vec())
                };
                let black = draw(0);
                let white = draw(0xFF);
                SelectObject(dc, previous);
                note(DeleteObject(bitmap.into()).as_bool(), "DeleteObject");
                black
                    .zip(white)
                    .map(|(black, white)| unblend(&black, &white))
            }
            Err(_) => None,
        };
        note(DeleteDC(dc).as_bool(), "DeleteDC");
        rgba
    }
}

fn encode_png(rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, ICON_SIZE, ICON_SIZE);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().ok()?.write_image_data(rgba).ok()?;
    Some(out)
}

/// Whether the process runs elevated. A token this host may not open counts as elevated.
fn elevated(pid: u32) -> bool {
    // SAFETY: both handles are closed below; the out buffers are sized for the calls.
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return true;
        };
        let mut token = HANDLE::default();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token);
        note(CloseHandle(process).is_ok(), "CloseHandle");
        if opened.is_err() {
            return true;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut bytes = 0u32;
        let read = GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut bytes,
        );
        note(CloseHandle(token).is_ok(), "CloseHandle");
        read.is_ok() && elevation.TokenIsElevated != 0
    }
}

impl Win32Backend {
    fn icon_for(&mut self, id: u32) -> Option<String> {
        if let Some(hash) = self.window_icons.get(&id) {
            return hash.clone();
        }
        let hash = icon_handle(hwnd_of(id))
            .and_then(render_icon)
            .and_then(|rgba| encode_png(&rgba))
            .and_then(|png| self.icons.insert(png));
        self.window_icons.insert(id, hash.clone());
        hash
    }
}

impl Backend for Win32Backend {
    fn sample(&mut self) -> Snapshot {
        if !self.dpi_aware {
            // SAFETY: changes the DPI awareness of this worker thread only.
            let previous =
                unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
            note(!previous.0.is_null(), "SetThreadDpiAwarenessContext");
            self.dpi_aware = true;
        }
        let mut exes = HashMap::new();
        let mut windows: Vec<RawWindow> = top_level_windows()
            .into_iter()
            .filter_map(|hwnd| describe(hwnd, &mut exes))
            .collect();
        let live: HashSet<u32> = windows.iter().map(|w| w.id).collect();
        self.window_icons.retain(|id, _| live.contains(id));
        for window in windows.iter_mut().filter(|w| is_switchable(w)) {
            window.icon = self.icon_for(window.id);
        }
        Snapshot {
            windows,
            // SAFETY: a plain query.
            foreground: id_of(unsafe { GetForegroundWindow() }),
            monitors: monitors(),
        }
    }

    fn focus(&mut self, id: u32) -> Result<Option<String>, FocusFailure> {
        let hwnd = hwnd_of(id);
        // SAFETY: plain window calls on a handle that may have gone stale, which they tolerate.
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(FocusFailure::Gone);
            }
            if IsIconic(hwnd).as_bool() {
                // Returns whether the window was visible before, not success.
                let _was_visible = ShowWindow(hwnd, SW_RESTORE);
            }
            // Both are best effort; the foreground check below decides.
            note(BringWindowToTop(hwnd).is_ok(), "BringWindowToTop");
            note(SetForegroundWindow(hwnd).as_bool(), "SetForegroundWindow");
            if GetForegroundWindow() == hwnd {
                let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                return Ok(monitor_info(monitor).map(|info| device_of(&info)));
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            Err(if elevated(pid) {
                FocusFailure::Elevated
            } else {
                FocusFailure::Refused
            })
        }
    }

    fn icon(&mut self, hash: &str) -> Option<Vec<u8>> {
        self.icons.get(hash).map(<[u8]>::to_vec)
    }
}
