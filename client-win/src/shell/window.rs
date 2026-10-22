//! Win32 window: class registration, borderless frame handling (WM_NCCALCSIZE,
//! WM_NCHITTEST, DPI), the message loop and the frame-pacing wait. Graphics
//! live in d3d.rs, message translation in input.rs.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObjectEx};
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::d3d::Gfx;
use super::input::{self, InputState};
use super::{
    FrameRequests, InputCapture, Rect, ShellApp, ShellConfig, VideoLayer, WakeHandle, WindowCommand, WindowInfo,
};

/// Width of the resize border, in device-independent pixels (scaled by DPI).
const FRAME_BORDER_DIP: f32 = 6.0;
/// Timer that repaints while the user drags or resizes the window: the modal
/// move/size loop blocks our own loop.
const TIMER_SIZE_MOVE: usize = 1;

/// Auto-reset Win32 event the loop waits on. `set` is callable from any thread.
pub struct WakeEvent(HANDLE);

// The handle is only used through Win32 calls, which are thread-safe.
unsafe impl Send for WakeEvent {}
unsafe impl Sync for WakeEvent {}

impl WakeEvent {
    fn new() -> Result<Self> {
        let handle = unsafe { CreateEventW(None, false, false, None)? };
        Ok(Self(handle))
    }

    pub fn set(&self) {
        let _ = unsafe { SetEvent(self.0) };
    }

    fn handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for WakeEvent {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Pointers to the app and video layer, valid only while `Window::run` is on
/// the stack. They let WM_TIMER repaint during the modal move/size loop.
#[derive(Clone, Copy)]
struct Ptrs {
    /// Points at a local `&mut dyn ShellApp` in `run`.
    app: *mut c_void,
    /// Points at a local `&mut dyn VideoLayer` in `run`.
    video: *mut c_void,
}

/// Everything the window procedure and the loop share. Owned by `Window`
/// through a raw pointer stored in GWLP_USERDATA.
pub(super) struct State {
    pub(super) hwnd: HWND,
    pub(super) wake: Arc<WakeEvent>,
    pub(super) gfx: Option<Gfx>,
    pub(super) input: InputState,
    /// The session's input capture (see [`InputCapture`]). It is taken out
    /// of the state while one of its methods runs, so a message sent back
    /// into the window procedure from inside it does not reach it again.
    capture: Option<Box<dyn InputCapture>>,
    /// `capture.captured()` as of the last check; while true, pointer and
    /// keyboard input bypass egui and the capture owns the cursor.
    pub(super) captured: bool,
    /// Drag and no-drag regions of the last rendered frame, in points.
    pub(super) hit_drag: Vec<egui::Rect>,
    pub(super) hit_no_drag: Vec<egui::Rect>,
    pub(super) cursor: egui::CursorIcon,
    /// A frame should be rendered as soon as the swap chain allows.
    pub(super) frame_due: bool,
    /// The frame-latency waitable object was consumed and not used yet.
    pub(super) latency_ready: bool,
    /// When egui asked for a repaint (None: none requested).
    pub(super) next_repaint: Option<Instant>,
    pub(super) min_points: (f32, f32),
    pub(super) dpi: u32,
    pub(super) maximized: bool,
    pub(super) minimized: bool,
    pub(super) focused: bool,
    pub(super) closed: bool,
    rendering: bool,
    pub(super) error: Option<anyhow::Error>,
    pub(super) start: Instant,
    ptrs: Option<Ptrs>,
}

impl State {
    fn new(wake: Arc<WakeEvent>, min_points: (f32, f32), dpi: u32) -> Self {
        Self {
            hwnd: HWND::default(),
            wake,
            gfx: None,
            input: InputState::default(),
            capture: None,
            captured: false,
            hit_drag: Vec::new(),
            hit_no_drag: Vec::new(),
            cursor: egui::CursorIcon::Default,
            frame_due: true,
            latency_ready: false,
            next_repaint: None,
            min_points,
            dpi,
            maximized: false,
            minimized: false,
            focused: true,
            closed: false,
            rendering: false,
            error: None,
            start: Instant::now(),
            ptrs: None,
        }
    }

    pub(super) fn ppp(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    /// Re-reads `captured()`. On a change (either way) the window drops its
    /// own mouse capture and egui's stale pointer, button, key and modifier
    /// state is cleared, so nothing stays "held" across the switch.
    fn sync_capture(&mut self) {
        let now = self.capture.as_ref().is_some_and(|c| c.captured());
        if now == self.captured {
            return;
        }
        self.captured = now;
        self.input.reset_for_capture_change();
        self.frame_due = true;
        // Last: ReleaseCapture sends WM_CAPTURECHANGED back into the
        // window procedure.
        if !self.hwnd.is_invalid() && unsafe { GetCapture() } == self.hwnd {
            let _ = unsafe { ReleaseCapture() };
        }
    }

    /// Gives `on_frame` this frame's video rect in client-area physical
    /// pixels and returns that rect for the video layer.
    fn frame_hook(&mut self, video: Option<egui::Rect>, ppo: f32) -> Option<Rect> {
        let px = to_px(video, ppo);
        if let Some(mut cap) = self.capture.take() {
            cap.on_frame(self.hwnd, px);
            if self.capture.is_none() {
                self.capture = Some(cap);
            }
        }
        self.sync_capture();
        px
    }

    /// Builds the UI for one frame, draws it and presents. Returns a window
    /// command the UI asked for.
    pub(super) fn render(
        &mut self,
        app: &mut dyn ShellApp,
        video: &mut dyn VideoLayer,
    ) -> Result<Option<WindowCommand>> {
        self.frame_due = false;
        let ppp = self.ppp();
        let Some(gfx) = self.gfx.as_mut() else { return Ok(None) };
        let (w, h) = gfx.size;
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w as f32 / ppp, h as f32 / ppp));
        let raw = self.input.raw_input(
            screen,
            ppp,
            self.focused,
            self.maximized,
            self.start.elapsed().as_secs_f64(),
        );
        let info = WindowInfo { maximized: self.maximized };
        let mut req = FrameRequests::default();
        let full = gfx.egui_ctx.run_ui(raw, |ui| app.frame(ui, info, &mut req));
        let repaint = full
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(Duration::MAX, |v| v.repaint_delay);
        let ppo = full.pixels_per_point;
        let (render_out, platform, _) = egui_directx11::split_output(full);

        // The back buffer is the client area, so these are client pixels.
        let video_px = self.frame_hook(req.video, ppo);
        self.hit_drag = std::mem::take(&mut req.drag);
        self.hit_no_drag = std::mem::take(&mut req.no_drag);
        self.cursor = platform.cursor_icon;
        let capture = req.capture.take();
        let command = req.command;
        let hwnd = self.hwnd;
        for cmd in platform.commands {
            if let egui::OutputCommand::CopyText(text) = cmd {
                if let Err(e) = input::set_clipboard(hwnd, &text) {
                    eprintln!("clipboard copy failed: {e:#}");
                }
            }
        }

        let Some(gfx) = self.gfx.as_mut() else { return Ok(None) };
        gfx.draw(render_out, video_px, video, capture.as_deref())?;
        self.next_repaint = if repaint == Duration::MAX { None } else { Instant::now().checked_add(repaint) };
        // While captured, the cursor belongs to the capture.
        if self.input.pointer.is_some() && !self.captured {
            apply_cursor(self.cursor);
        }
        Ok(command)
    }
}

/// An egui rect in points to client-area physical pixels.
fn to_px(video: Option<egui::Rect>, ppo: f32) -> Option<Rect> {
    video.map(|r| Rect { x: r.min.x * ppo, y: r.min.y * ppo, w: r.width() * ppo, h: r.height() * ppo })
}

/// Offers a message to the input capture first. `Some` means the capture
/// consumed it and the window procedure returns that result.
fn capture_message(st: &mut State, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    let mut cap = st.capture.take()?;
    let result = cap.on_message(hwnd, msg, wp, lp);
    if st.capture.is_none() {
        st.capture = Some(cap);
    }
    st.sync_capture();
    result
}

/// Renders through the pointers `run` installed. Returns Ok(None) when no
/// app is attached or a render is already running.
fn render_now(st: &mut State) -> Result<Option<WindowCommand>> {
    let Some(p) = st.ptrs else { return Ok(None) };
    if st.rendering {
        return Ok(None);
    }
    st.rendering = true;
    let app = unsafe { &mut *(p.app as *mut &mut dyn ShellApp) };
    let video = unsafe { &mut *(p.video as *mut &mut dyn VideoLayer) };
    let result = st.render(&mut **app, &mut **video);
    st.rendering = false;
    result
}

/// The window: owns the state box and the Win32 window handle.
pub struct Window {
    hwnd: HWND,
    wake: Arc<WakeEvent>,
    ptr: *mut State,
}

impl Window {
    pub fn new(config: ShellConfig) -> Result<Self> {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let dpi = unsafe { GetDpiForSystem() }.max(96);
        let scale = dpi as f32 / 96.0;
        let wake = Arc::new(WakeEvent::new()?);
        let ptr = Box::into_raw(Box::new(State::new(wake.clone(), config.min_size, dpi)));
        // From here on, Drop frees the box, even if creation fails.
        let mut this = Self { hwnd: HWND::default(), wake, ptr };

        let w = (config.size.0 * scale).round() as i32;
        let h = (config.size.1 * scale).round() as i32;
        this.hwnd = unsafe { create_window(&config.title, w, h, ptr)? };
        unsafe {
            let st = &mut *ptr;
            st.dpi = GetDpiForWindow(this.hwnd).max(96);
            st.maximized = IsZoomed(this.hwnd).as_bool();
            // Keep the DWM shadow on the frame-less client area.
            let margins = MARGINS { cxLeftWidth: 1, cxRightWidth: 1, cyTopHeight: 1, cyBottomHeight: 1 };
            let _ = DwmExtendFrameIntoClientArea(this.hwnd, &margins);
            let mut rc = RECT::default();
            GetClientRect(this.hwnd, &mut rc)?;
            let gfx = Gfx::new(this.hwnd, (rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32)?;
            st.gfx = Some(gfx);
            let _ = ShowWindow(this.hwnd, SW_SHOW);
        }
        Ok(this)
    }

    pub fn wake_handle(&self) -> WakeHandle {
        WakeHandle(self.wake.clone())
    }

    /// Installs the input capture; it sees every message from the next one.
    pub fn set_input_capture(&mut self, capture: Box<dyn InputCapture>) {
        let st = unsafe { &mut *self.ptr };
        st.capture = Some(capture);
        st.sync_capture();
    }

    /// The D3D11 device the window renders with.
    pub fn device(&self) -> ID3D11Device {
        unsafe { &*self.ptr }.gfx.as_ref().expect("gfx created in new").device().clone()
    }

    /// Runs the event loop until the window closes.
    pub fn run(self, app: &mut dyn ShellApp, video: &mut dyn VideoLayer) -> Result<()> {
        let mut app_ref: &mut dyn ShellApp = app;
        let mut video_ref: &mut dyn VideoLayer = video;
        let ptrs = Ptrs {
            app: &mut app_ref as *mut &mut dyn ShellApp as *mut c_void,
            video: &mut video_ref as *mut &mut dyn VideoLayer as *mut c_void,
        };
        unsafe { (*self.ptr).ptrs = Some(ptrs) };
        let result = unsafe { event_loop(self.ptr) };
        unsafe { (*self.ptr).ptrs = None };
        result
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            if !(*self.ptr).closed && !self.hwnd.is_invalid() {
                let _ = DestroyWindow(self.hwnd);
            }
            drop(Box::from_raw(self.ptr));
        }
    }
}

unsafe fn create_window(title: &str, w: i32, h: i32, state: *mut State) -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class = w!("BubblePlayShell");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: class,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        ..Default::default()
    };
    RegisterClassW(&wc);
    let title_w: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(title_w.as_ptr()),
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        w,
        h,
        None,
        None,
        Some(instance.into()),
        Some(state as *const c_void),
    )?;
    Ok(hwnd)
}

/// Pumps every queued message. Returns true when WM_QUIT was seen.
unsafe fn pump_messages() -> bool {
    let mut msg = MSG::default();
    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
        if msg.message == WM_QUIT {
            return true;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    false
}

/// Waits on `handles` plus window messages. Returns the index of the handle
/// that fired; `handles.len()` or more means a message or a timeout.
unsafe fn wait_index(handles: &[HANDLE], timeout_ms: u32) -> Result<usize> {
    let r = MsgWaitForMultipleObjectsEx(Some(handles), timeout_ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
    if r == WAIT_FAILED {
        bail!("MsgWaitForMultipleObjectsEx failed");
    }
    Ok(r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize)
}

fn ms_until(t: Instant, now: Instant) -> u32 {
    let d = t.saturating_duration_since(now);
    d.as_micros().div_ceil(1000).clamp(1, 3_600_000) as u32
}

/// The loop. Draws only when a frame is due (input, wake, resize, or egui's
/// repaint delay elapsed). Otherwise it blocks in MsgWaitForMultipleObjectsEx
/// with a timeout equal to egui's next repaint, or INFINITE when egui asked
/// for none. The frame-latency waitable joins the wait only while a frame is due.
unsafe fn event_loop(st: *mut State) -> Result<()> {
    loop {
        if pump_messages() {
            break;
        }
        let s = &mut *st;
        if s.closed {
            break;
        }
        let now = Instant::now();
        if s.next_repaint.is_some_and(|t| now >= t) {
            s.next_repaint = None;
            s.frame_due = true;
        }

        if s.frame_due && !s.minimized {
            if !s.latency_ready {
                if let Some(h) = s.gfx.as_ref().map(|g| g.latency_handle()) {
                    s.latency_ready = WaitForSingleObjectEx(h, 0, false) == WAIT_OBJECT_0;
                }
            }
            if s.latency_ready {
                s.latency_ready = false;
                match render_now(s) {
                    Ok(Some(cmd)) => execute_command(s.hwnd, cmd),
                    Ok(None) => {}
                    Err(e) => {
                        s.error = Some(e);
                        let _ = DestroyWindow(s.hwnd);
                    }
                }
                continue;
            }
            // Frame due, swap chain busy: wait for it, a wake or a message.
            let latency = s.gfx.as_ref().map(|g| g.latency_handle()).unwrap_or_default();
            let handles = [s.wake.handle(), latency];
            match wait_index(&handles, 100)? {
                0 => s.frame_due = true,
                1 => s.latency_ready = true,
                _ => {}
            }
        } else {
            let timeout = match (s.minimized, s.next_repaint) {
                (false, Some(t)) => ms_until(t, now),
                _ => u32::MAX,
            };
            let handles = [s.wake.handle()];
            if wait_index(&handles, timeout)? == 0 {
                s.frame_due = true;
            }
        }
    }
    let s = &mut *st;
    match s.error.take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

unsafe fn execute_command(hwnd: HWND, cmd: WindowCommand) {
    match cmd {
        WindowCommand::Minimize => {
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
        }
        WindowCommand::ToggleMaximize => {
            let show = if IsZoomed(hwnd).as_bool() { SW_RESTORE } else { SW_MAXIMIZE };
            let _ = ShowWindow(hwnd, show);
        }
        WindowCommand::Close => {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// Repaints from WM_TIMER during the modal move/size loop. Waits up to 50 ms
/// for the swap chain so the loop never stalls for long.
fn modal_redraw(st: &mut State) {
    if st.minimized {
        return;
    }
    let Some(latency) = st.gfx.as_ref().map(|g| g.latency_handle()) else { return };
    if unsafe { WaitForSingleObjectEx(latency, 50, false) } != WAIT_OBJECT_0 {
        return;
    }
    match render_now(st) {
        Ok(Some(cmd)) => unsafe { execute_command(st.hwnd, cmd) },
        Ok(None) => {}
        Err(e) => {
            st.error = Some(e);
            unsafe {
                let _ = DestroyWindow(st.hwnd);
            }
        }
    }
}

fn cursor_id(icon: egui::CursorIcon) -> PCWSTR {
    use egui::CursorIcon as C;
    match icon {
        C::PointingHand | C::Grab | C::Grabbing => IDC_HAND,
        C::Text | C::VerticalText => IDC_IBEAM,
        C::ResizeHorizontal | C::ResizeEast | C::ResizeWest => IDC_SIZEWE,
        C::ResizeVertical | C::ResizeNorth | C::ResizeSouth => IDC_SIZENS,
        C::ResizeNeSw | C::ResizeNorthEast | C::ResizeSouthWest => IDC_SIZENESW,
        C::ResizeNwSe | C::ResizeNorthWest | C::ResizeSouthEast => IDC_SIZENWSE,
        C::NotAllowed | C::NoDrop => IDC_NO,
        C::Progress => IDC_APPSTARTING,
        C::Wait => IDC_WAIT,
        C::Crosshair => IDC_CROSS,
        C::Help => IDC_HELP,
        C::Move | C::AllScroll => IDC_SIZEALL,
        _ => IDC_ARROW,
    }
}

fn apply_cursor(icon: egui::CursorIcon) {
    unsafe {
        let cursor = LoadCursorW(None, cursor_id(icon)).ok();
        SetCursor(cursor);
    }
}

/// Hit testing for the borderless frame: resize borders, the custom title bar
/// (HTCAPTION, so Windows drags it and double-click maximizes), else client.
unsafe fn hit_test(st: &State, hwnd: HWND, lp: LPARAM) -> LRESULT {
    let x = (lp.0 & 0xffff) as i16 as i32;
    let y = ((lp.0 >> 16) & 0xffff) as i16 as i32;
    let mut r = RECT::default();
    let _ = GetWindowRect(hwnd, &mut r);
    let (px, py) = (x - r.left, y - r.top);
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let ppp = st.ppp();
    if !st.maximized {
        let b = (FRAME_BORDER_DIP * ppp).round() as i32;
        let left = px < b;
        let right = px >= w - b;
        let top = py < b;
        let bottom = py >= h - b;
        let code = if top && left {
            Some(HTTOPLEFT)
        } else if top && right {
            Some(HTTOPRIGHT)
        } else if bottom && left {
            Some(HTBOTTOMLEFT)
        } else if bottom && right {
            Some(HTBOTTOMRIGHT)
        } else if left {
            Some(HTLEFT)
        } else if right {
            Some(HTRIGHT)
        } else if top {
            Some(HTTOP)
        } else if bottom {
            Some(HTBOTTOM)
        } else {
            None
        };
        if let Some(c) = code {
            return LRESULT(c as isize);
        }
    }
    let p = egui::pos2(px as f32 / ppp, py as f32 / ppp);
    let in_drag = st.hit_drag.iter().any(|r| r.contains(p)) && !st.hit_no_drag.iter().any(|r| r.contains(p));
    LRESULT(if in_drag { HTCAPTION as isize } else { HTCLIENT as isize })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = lp.0 as usize as *const CREATESTRUCTW;
        let ptr = (*cs).lpCreateParams as *mut State;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
        (*ptr).hwnd = hwnd;
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as usize as *mut State;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let st = &mut *ptr;

    if let Some(result) = capture_message(st, hwnd, msg, wp, lp) {
        return result;
    }
    if let Some(result) = input::message(st, hwnd, msg, wp, lp) {
        return result;
    }

    match msg {
        WM_NCCALCSIZE => {
            // Frame-less client: the whole window is client area. A maximized
            // window overhangs the monitor by its frame, so clip to the work area.
            if wp.0 != 0 && IsZoomed(hwnd).as_bool() {
                let params = lp.0 as usize as *mut NCCALCSIZE_PARAMS;
                let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
                if GetMonitorInfoW(mon, &mut mi).as_bool() {
                    (*params).rgrc[0] = mi.rcWork;
                }
            }
            return LRESULT(0);
        }
        WM_NCHITTEST => return hit_test(st, hwnd, lp),
        WM_GETMINMAXINFO => {
            let mmi = lp.0 as usize as *mut MINMAXINFO;
            let s = st.ppp();
            (*mmi).ptMinTrackSize = POINT {
                x: (st.min_points.0 * s).ceil() as i32,
                y: (st.min_points.1 * s).ceil() as i32,
            };
            return LRESULT(0);
        }
        WM_SIZE => {
            let w = (lp.0 & 0xffff) as u32;
            let h = ((lp.0 >> 16) & 0xffff) as u32;
            st.minimized = wp.0 as u32 == SIZE_MINIMIZED;
            st.maximized = wp.0 as u32 == SIZE_MAXIMIZED;
            if !st.minimized && w > 0 && h > 0 {
                if let Some(Err(e)) = st.gfx.as_mut().map(|g| g.resize(w, h)) {
                    st.error = Some(e);
                    let _ = DestroyWindow(hwnd);
                }
                st.frame_due = true;
            }
            return LRESULT(0);
        }
        WM_ENTERSIZEMOVE => {
            SetTimer(Some(hwnd), TIMER_SIZE_MOVE, 16, None);
            return LRESULT(0);
        }
        WM_EXITSIZEMOVE => {
            let _ = KillTimer(Some(hwnd), TIMER_SIZE_MOVE);
            return LRESULT(0);
        }
        WM_TIMER => {
            if wp.0 == TIMER_SIZE_MOVE && !st.closed {
                modal_redraw(st);
            }
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            st.dpi = ((wp.0 >> 16) & 0xffff) as u32;
            let r = lp.0 as usize as *const RECT;
            let _ = SetWindowPos(
                hwnd,
                None,
                (*r).left,
                (*r).top,
                (*r).right - (*r).left,
                (*r).bottom - (*r).top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            st.frame_due = true;
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_SETCURSOR => {
            if (lp.0 & 0xffff) as u32 == HTCLIENT {
                // While captured the capture answers WM_SETCURSOR; if it let
                // one through, leave the cursor alone (DefWindowProc would
                // set the class arrow).
                if !st.captured {
                    apply_cursor(st.cursor);
                }
                return LRESULT(1);
            }
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        }
        WM_DESTROY => {
            st.closed = true;
            PostQuitMessage(0);
            return LRESULT(0);
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

#[cfg(test)]
mod tests {
    //! The InputCapture seam without a window: messages go through the same
    //! two steps the window procedure takes (capture first, then egui), and
    //! frames through `frame_hook`. The null HWND makes the Win32 side calls
    //! (TrackMouseEvent, mouse capture) no-ops.
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::*;

    #[derive(Default)]
    struct Log {
        messages: Vec<u32>,
        frames: Vec<Option<Rect>>,
    }

    struct Fake {
        log: Rc<RefCell<Log>>,
        captured: Rc<Cell<bool>>,
        /// The one message this capture consumes (0: none).
        consume: u32,
    }

    impl InputCapture for Fake {
        fn on_message(&mut self, _hwnd: HWND, msg: u32, _wp: WPARAM, _lp: LPARAM) -> Option<LRESULT> {
            self.log.borrow_mut().messages.push(msg);
            (msg == self.consume).then_some(LRESULT(7))
        }
        fn captured(&self) -> bool {
            self.captured.get()
        }
        fn on_frame(&mut self, _hwnd: HWND, video_px: Option<Rect>) {
            self.log.borrow_mut().frames.push(video_px);
        }
    }

    fn state_with(consume: u32) -> (State, Rc<RefCell<Log>>, Rc<Cell<bool>>) {
        let mut st = State::new(Arc::new(WakeEvent::new().unwrap()), (760.0, 520.0), 96);
        let log = Rc::new(RefCell::new(Log::default()));
        let captured = Rc::new(Cell::new(false));
        st.capture = Some(Box::new(Fake { log: log.clone(), captured: captured.clone(), consume }));
        (st, log, captured)
    }

    /// What the window procedure does before its own message handling.
    fn deliver(st: &mut State, msg: u32, wp: usize, lp: isize) -> Option<LRESULT> {
        let (wp, lp) = (WPARAM(wp), LPARAM(lp));
        capture_message(st, HWND::default(), msg, wp, lp).or_else(|| input::message(st, HWND::default(), msg, wp, lp))
    }

    fn at(x: isize, y: isize) -> isize {
        (y << 16) | x
    }

    fn moves(st: &State) -> usize {
        st.input.events.iter().filter(|e| matches!(e, egui::Event::PointerMoved(_))).count()
    }

    #[test]
    fn consumed_messages_skip_egui() {
        let (mut st, log, _) = state_with(WM_KEYDOWN);
        assert_eq!(deliver(&mut st, WM_KEYDOWN, 0x41, 0), Some(LRESULT(7)));
        assert!(st.input.events.is_empty(), "a consumed key reached egui: {:?}", st.input.events);
        // Not consumed: the capture still sees it first, then egui gets it.
        deliver(&mut st, WM_MOUSEMOVE, 0, at(10, 10));
        assert_eq!(log.borrow().messages, vec![WM_KEYDOWN, WM_MOUSEMOVE]);
        assert_eq!(moves(&st), 1);
    }

    #[test]
    fn egui_is_starved_while_captured() {
        let (mut st, _, captured) = state_with(0);
        deliver(&mut st, WM_MOUSEMOVE, 0, at(10, 10));
        deliver(&mut st, WM_KEYDOWN, 0x41, 0);
        assert_eq!(moves(&st), 1);
        st.input.events.clear();

        captured.set(true);
        assert_eq!(deliver(&mut st, WM_MOUSEMOVE, 0, at(20, 20)), None);
        assert_eq!(deliver(&mut st, WM_CHAR, 'x' as usize, 0), None);
        assert_eq!(deliver(&mut st, WM_KEYDOWN, 0x42, 0), None);
        assert!(st.captured);
        // The switch released the held key and dropped the pointer; nothing
        // after it reached egui.
        let ev = &st.input.events;
        assert!(
            matches!(ev.as_slice(), [egui::Event::Key { key: egui::Key::A, pressed: false, .. }, egui::Event::PointerGone]),
            "events while captured: {ev:?}"
        );
        assert_eq!(st.input.pointer, None);
        assert_eq!(st.input.modifiers, egui::Modifiers::default());
        st.input.events.clear();

        captured.set(false);
        deliver(&mut st, WM_MOUSEMOVE, 0, at(30, 30));
        assert!(!st.captured);
        assert_eq!(moves(&st), 1, "egui does not get the pointer back: {:?}", st.input.events);
    }

    #[test]
    fn on_frame_gets_client_pixels() {
        let (mut st, log, _) = state_with(0);
        let video = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(300.0, 200.0));
        let px1 = st.frame_hook(Some(video), 1.0);
        let px15 = st.frame_hook(Some(video), 1.5);
        st.frame_hook(None, 1.5);
        let r1 = Rect { x: 10.0, y: 20.0, w: 300.0, h: 200.0 };
        let r15 = Rect { x: 15.0, y: 30.0, w: 450.0, h: 300.0 };
        assert_eq!(px1, Some(r1));
        assert_eq!(px15, Some(r15));
        assert_eq!(log.borrow().frames, vec![Some(r1), Some(r15), None]);
    }
}
