//! Window-system types the core media and input modules share with the egui shell.
//!
//! They live here, not in `shell`, so the core builds without the `egui-client`
//! feature. `shell` re-exports them.

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D11::{ID3D11DeviceContext, ID3D11RenderTargetView};

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
    fn on_message(
        &mut self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT>;
    fn captured(&self) -> bool;
    fn on_frame(&mut self, hwnd: HWND, video_px: Option<Rect>);
}
