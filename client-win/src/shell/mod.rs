//! Win32 window, D3D11 device and flip-model swap chain, egui-directx11
//! integration and the event loop.
//!
//! Contract with the rest of the client:
//! - [`VideoLayer::draw`] is called every rendered frame, before egui, with
//!   the back buffer's render target. The media module implements it later;
//!   [`PlaceholderVideo`] stands in for now.
//! - [`WakeHandle::wake`] (any thread) asks for a frame now: "frame ready,
//!   present now". The loop waits on window messages, the wake event and
//!   (only while a frame is due) the swap chain's frame-latency waitable
//!   object with `MsgWaitForMultipleObjectsEx`. It never redraws while idle.
//! - [`ShellApp::frame`] builds the egui UI and fills [`FrameRequests`].

use std::path::PathBuf;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView};

mod d3d;
mod input;
mod placeholder;
mod window;

pub use placeholder::PlaceholderVideo;

/// A rectangle in physical pixels of the back buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Draws video (or a placeholder) into the back buffer before egui.
pub trait VideoLayer {
    fn draw(&mut self, ctx: &ID3D11DeviceContext, rtv: &ID3D11RenderTargetView, viewport: Rect);
}

/// Thread-safe "frame ready, present now" handle. Cloneable; wraps an
/// auto-reset Win32 event the loop waits on.
#[derive(Clone)]
pub struct WakeHandle(pub(crate) std::sync::Arc<window::WakeEvent>);

impl WakeHandle {
    pub fn wake(&self) {
        self.0.set();
    }
}

/// What the window looks like right now, for the UI.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowInfo {
    pub maximized: bool,
    pub focused: bool,
}

/// Requests the UI makes for this frame.
#[derive(Debug, Default)]
pub struct FrameRequests {
    /// Where video goes, in egui points; None hides the video layer (the
    /// canvas is cleared to the field colour instead).
    pub video: Option<egui::Rect>,
    /// Areas of the custom title bar that drag the window (HTCAPTION).
    pub drag: Vec<egui::Rect>,
    /// Interactive areas inside `drag` (buttons) that must stay clickable.
    pub no_drag: Vec<egui::Rect>,
    pub command: Option<WindowCommand>,
    /// Save this frame's back buffer as PNG (window contents only).
    pub capture: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowCommand {
    Minimize,
    ToggleMaximize,
    Close,
}

/// Input capture for a remote session (implemented in `crate::input`).
///
/// Contract:
/// - The WndProc calls [`InputCapture::on_message`] first, for every message
///   (Raw Input, keys, mouse, focus, activation, WM_SETCURSOR, size/move),
///   before any egui mapping. `Some(result)` consumes the message: the shell
///   returns that result and does nothing else with it.
/// - While [`InputCapture::captured`] is true, the shell withholds pointer and
///   keyboard events from egui. When it turns false, egui gets them again.
///   The shell releases its own mouse capture and clears stale egui pointer
///   state on each transition.
/// - [`InputCapture::on_frame`] is called once per rendered frame, after the
///   UI has laid out, with the video rect in client-area physical pixels
///   (None when no video is shown). The capture uses it for absolute mapping
///   and ClipCursor.
/// - The capture owns ClipCursor, cursor visibility and the cursor shape
///   while captured (it answers WM_SETCURSOR itself). The shell must not
///   change them then.
/// - The low-level keyboard hook lives on its own thread inside
///   `crate::input`, never on the UI/render thread, so it can't be stalled by
///   a slow frame.
pub trait InputCapture {
    fn on_message(&mut self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT>;
    fn captured(&self) -> bool;
    fn on_frame(&mut self, hwnd: HWND, video_px: Option<Rect>);
}

pub trait ShellApp {
    fn frame(&mut self, ui: &mut egui::Ui, info: WindowInfo, out: &mut FrameRequests);
}

pub struct ShellConfig {
    pub title: String,
    /// Initial client size in points (scaled by the monitor DPI).
    pub size: (f32, f32),
    pub min_size: (f32, f32),
}

/// The window plus its D3D11 device and swap chain.
pub struct Shell {
    inner: window::Window,
}

impl Shell {
    pub fn new(config: ShellConfig) -> anyhow::Result<Self> {
        Ok(Self { inner: window::Window::new(config)? })
    }

    pub fn wake_handle(&self) -> WakeHandle {
        self.inner.wake_handle()
    }

    /// The D3D11 device the window renders with, available before `run`.
    /// It was created with BGRA and video support, and multithread
    /// protection is on (`ID3D10Multithread::SetMultithreadProtected`), so
    /// a decoder on another thread (Media Foundation) can share it.
    pub fn device(&self) -> ID3D11Device {
        self.inner.device()
    }

    /// Installs the session's input capture; it applies from the next message.
    /// Call before `run`, or never for the home screens.
    pub fn set_input_capture(&mut self, capture: Box<dyn InputCapture>) {
        self.inner.set_input_capture(capture)
    }

    /// Runs until the window closes.
    pub fn run(self, app: &mut dyn ShellApp, video: &mut dyn VideoLayer) -> anyhow::Result<()> {
        self.inner.run(app, video)
    }
}
