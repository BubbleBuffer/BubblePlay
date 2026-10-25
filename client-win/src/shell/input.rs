//! Win32 messages to egui input events, key mapping, and the clipboard.

use std::mem::size_of;

use anyhow::{bail, Result};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::window::State;

/// Input state that outlives a single frame: queued events, modifiers,
/// pointer position and mouse-button bookkeeping.
#[derive(Default)]
pub(super) struct InputState {
    pub(super) events: Vec<egui::Event>,
    pub(super) modifiers: egui::Modifiers,
    /// Pointer position in points; None when the pointer is outside the window.
    pub(super) pointer: Option<egui::Pos2>,
    /// TrackMouseEvent(TME_LEAVE) is armed.
    tracking: bool,
    /// Buttons currently held; the window captures the mouse while > 0.
    buttons: u32,
    /// Which buttons and keys egui saw pressed and not released yet, so a
    /// capture switch can release them.
    held: Vec<egui::PointerButton>,
    keys_down: Vec<egui::Key>,
    high_surrogate: Option<u16>,
}

impl InputState {
    /// Builds the RawInput for one frame and drains the queued events.
    pub(super) fn raw_input(
        &mut self,
        screen: egui::Rect,
        ppp: f32,
        focused: bool,
        maximized: bool,
        time: f64,
    ) -> egui::RawInput {
        let info = egui::ViewportInfo {
            native_pixels_per_point: Some(ppp),
            inner_rect: Some(screen),
            outer_rect: Some(screen),
            maximized: Some(maximized),
            minimized: Some(false),
            focused: Some(focused),
            ..Default::default()
        };
        let mut raw = egui::RawInput {
            screen_rect: Some(screen),
            focused,
            time: Some(time),
            modifiers: self.modifiers,
            events: std::mem::take(&mut self.events),
            ..Default::default()
        };
        raw.viewports.insert(egui::ViewportId::ROOT, info);
        raw
    }

    /// Input capture started or ended: release every button and key egui
    /// still thinks is held (at the last pointer position), drop the
    /// modifiers and the pointer.
    pub(super) fn reset_for_capture_change(&mut self) {
        let pos = self.pointer.unwrap_or(egui::Pos2::ZERO);
        for button in std::mem::take(&mut self.held) {
            self.events.push(egui::Event::PointerButton {
                pos,
                button,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            });
        }
        for key in std::mem::take(&mut self.keys_down) {
            self.events.push(egui::Event::Key {
                key,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            });
        }
        self.buttons = 0;
        self.modifiers = egui::Modifiers::default();
        self.high_surrogate = None;
        self.pointer = None;
        self.events.push(egui::Event::PointerGone);
    }
}

/// Pointer and keyboard messages, which egui does not get while captured.
fn is_pointer_or_key(msg: u32) -> bool {
    matches!(
        msg,
        WM_MOUSEMOVE
            | WM_MOUSELEAVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_LBUTTONDBLCLK
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_RBUTTONDBLCLK
            | WM_MBUTTONDOWN
            | WM_MBUTTONUP
            | WM_MBUTTONDBLCLK
            | WM_XBUTTONDOWN
            | WM_XBUTTONUP
            | WM_XBUTTONDBLCLK
            | WM_MOUSEWHEEL
            | WM_MOUSEHWHEEL
            | WM_KEYDOWN
            | WM_KEYUP
            | WM_SYSKEYDOWN
            | WM_SYSKEYUP
            | WM_CHAR
            | WM_SYSCHAR
            | WM_DEADCHAR
            | WM_SYSDEADCHAR
    )
}

fn push_text(inp: &mut InputState, text: String) {
    if !text.chars().any(char::is_control) {
        inp.events.push(egui::Event::Text(text));
    }
}

/// Handles one window message for the input. Returns the LRESULT when the
/// message is fully handled, None to let DefWindowProc run. Sets `frame_due`
/// when events were queued.
pub(super) fn message(
    st: &mut State,
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> Option<LRESULT> {
    if st.captured && is_pointer_or_key(msg) {
        // The capture did not consume it; it goes to DefWindowProc, not egui.
        return None;
    }
    let ppp = st.ppp();
    let before = st.input.events.len();
    let inp = &mut st.input;
    let handled = match msg {
        WM_MOUSEMOVE => {
            let p = pos(lp, ppp);
            if !inp.tracking {
                track_leave(hwnd);
                inp.tracking = true;
            }
            inp.pointer = Some(p);
            inp.events.push(egui::Event::PointerMoved(p));
            Some(LRESULT(0))
        }
        WM_MOUSELEAVE => {
            inp.tracking = false;
            inp.pointer = None;
            inp.events.push(egui::Event::PointerGone);
            Some(LRESULT(0))
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let down = matches!(
                msg,
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
            );
            let button = match msg {
                WM_LBUTTONDOWN | WM_LBUTTONUP => egui::PointerButton::Primary,
                WM_RBUTTONDOWN | WM_RBUTTONUP => egui::PointerButton::Secondary,
                WM_MBUTTONDOWN | WM_MBUTTONUP => egui::PointerButton::Middle,
                _ => {
                    if (wp.0 >> 16) & 0xffff == 2 {
                        egui::PointerButton::Extra2
                    } else {
                        egui::PointerButton::Extra1
                    }
                }
            };
            let p = pos(lp, ppp);
            inp.pointer = Some(p);
            if down {
                inp.held.push(button);
                inp.buttons += 1;
                unsafe {
                    SetCapture(hwnd);
                }
            } else {
                if let Some(i) = inp.held.iter().position(|b| *b == button) {
                    inp.held.remove(i);
                }
                inp.buttons = inp.buttons.saturating_sub(1);
                if inp.buttons == 0 {
                    let _ = unsafe { ReleaseCapture() };
                }
            }
            inp.events.push(egui::Event::PointerButton {
                pos: p,
                button,
                pressed: down,
                modifiers: inp.modifiers,
            });
            if matches!(msg, WM_XBUTTONDOWN | WM_XBUTTONUP) {
                Some(LRESULT(1))
            } else {
                Some(LRESULT(0))
            }
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let raw = ((wp.0 >> 16) & 0xffff) as u16 as i16;
            let notches = raw as f32 / 120.0;
            let delta = if msg == WM_MOUSEWHEEL {
                egui::vec2(0.0, notches)
            } else {
                egui::vec2(notches, 0.0)
            };
            inp.events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta,
                phase: egui::TouchPhase::Move,
                modifiers: inp.modifiers,
            });
            Some(LRESULT(0))
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let repeat = pressed && (lp.0 & (1 << 30)) != 0;
            inp.modifiers = current_modifiers();
            let mods = inp.modifiers;
            if let Some(key) = key_from_vk(wp.0 as u16) {
                if pressed {
                    if !inp.keys_down.contains(&key) {
                        inp.keys_down.push(key);
                    }
                } else {
                    inp.keys_down.retain(|k| *k != key);
                }
                inp.events.push(egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed,
                    repeat,
                    modifiers: mods,
                });
                if pressed && !repeat && mods.command {
                    match key {
                        egui::Key::C => inp.events.push(egui::Event::Copy),
                        egui::Key::X => inp.events.push(egui::Event::Cut),
                        egui::Key::V => {
                            if let Some(text) = read_clipboard(hwnd) {
                                inp.events.push(egui::Event::Paste(text));
                            }
                        }
                        _ => {}
                    }
                }
            }
            // Alt combinations (Alt+F4, Alt+Space) stay with DefWindowProc.
            if matches!(msg, WM_SYSKEYDOWN | WM_SYSKEYUP) {
                None
            } else {
                Some(LRESULT(0))
            }
        }
        WM_CHAR => {
            let unit = wp.0 as u16;
            match unit {
                0xD800..=0xDBFF => inp.high_surrogate = Some(unit),
                0xDC00..=0xDFFF => {
                    if let Some(hi) = inp.high_surrogate.take() {
                        if let Ok(s) = String::from_utf16(&[hi, unit]) {
                            push_text(inp, s);
                        }
                    }
                }
                _ => {
                    inp.high_surrogate = None;
                    if let Some(c) = char::from_u32(unit as u32) {
                        push_text(inp, c.to_string());
                    }
                }
            }
            Some(LRESULT(0))
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            let focused = msg == WM_SETFOCUS;
            st.focused = focused;
            inp.events.push(egui::Event::WindowFocused(focused));
            None
        }
        _ => None,
    };
    if st.input.events.len() != before {
        st.frame_due = true;
    }
    handled
}

/// Mouse position from a client-coordinate LPARAM, converted to points.
fn pos(lp: LPARAM, ppp: f32) -> egui::Pos2 {
    let x = (lp.0 & 0xffff) as i16 as f32;
    let y = ((lp.0 >> 16) & 0xffff) as i16 as f32;
    egui::pos2(x / ppp, y / ppp)
}

fn track_leave(hwnd: HWND) {
    let mut tme = TRACKMOUSEEVENT {
        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    let _ = unsafe { TrackMouseEvent(&mut tme) };
}

fn current_modifiers() -> egui::Modifiers {
    let down = |vk: VIRTUAL_KEY| unsafe { GetKeyState(vk.0 as i32) < 0 };
    let ctrl = down(VK_CONTROL);
    egui::Modifiers {
        alt: down(VK_MENU),
        ctrl,
        shift: down(VK_SHIFT),
        mac_cmd: false,
        command: ctrl,
    }
}

const LETTERS: [egui::Key; 26] = [
    egui::Key::A,
    egui::Key::B,
    egui::Key::C,
    egui::Key::D,
    egui::Key::E,
    egui::Key::F,
    egui::Key::G,
    egui::Key::H,
    egui::Key::I,
    egui::Key::J,
    egui::Key::K,
    egui::Key::L,
    egui::Key::M,
    egui::Key::N,
    egui::Key::O,
    egui::Key::P,
    egui::Key::Q,
    egui::Key::R,
    egui::Key::S,
    egui::Key::T,
    egui::Key::U,
    egui::Key::V,
    egui::Key::W,
    egui::Key::X,
    egui::Key::Y,
    egui::Key::Z,
];

const DIGITS: [egui::Key; 10] = [
    egui::Key::Num0,
    egui::Key::Num1,
    egui::Key::Num2,
    egui::Key::Num3,
    egui::Key::Num4,
    egui::Key::Num5,
    egui::Key::Num6,
    egui::Key::Num7,
    egui::Key::Num8,
    egui::Key::Num9,
];

const FUNCTION_KEYS: [egui::Key; 12] = [
    egui::Key::F1,
    egui::Key::F2,
    egui::Key::F3,
    egui::Key::F4,
    egui::Key::F5,
    egui::Key::F6,
    egui::Key::F7,
    egui::Key::F8,
    egui::Key::F9,
    egui::Key::F10,
    egui::Key::F11,
    egui::Key::F12,
];

/// Virtual-key code to egui key, for the keys the UI uses.
fn key_from_vk(vk: u16) -> Option<egui::Key> {
    use egui::Key;
    let key = match vk {
        0x30..=0x39 => DIGITS[(vk - 0x30) as usize],
        0x41..=0x5A => LETTERS[(vk - 0x41) as usize],
        0x70..=0x7B => FUNCTION_KEYS[(vk - 0x70) as usize],
        0x25 => Key::ArrowLeft,
        0x26 => Key::ArrowUp,
        0x27 => Key::ArrowRight,
        0x28 => Key::ArrowDown,
        0x1B => Key::Escape,
        0x09 => Key::Tab,
        0x08 => Key::Backspace,
        0x0D => Key::Enter,
        0x20 => Key::Space,
        0x2D => Key::Insert,
        0x2E => Key::Delete,
        0x24 => Key::Home,
        0x23 => Key::End,
        0x21 => Key::PageUp,
        0x22 => Key::PageDown,
        0xBD => Key::Minus,
        0xBB => Key::Equals,
        0xBC => Key::Comma,
        0xBE => Key::Period,
        0xBA => Key::Semicolon,
        0xBF => Key::Slash,
        0xDC => Key::Backslash,
        0xDB => Key::OpenBracket,
        0xDD => Key::CloseBracket,
        0xDE => Key::Quote,
        0xC0 => Key::Backtick,
        _ => return None,
    };
    Some(key)
}

/// Puts `text` on the clipboard as CF_UNICODETEXT.
pub(super) fn set_clipboard(hwnd: HWND, text: &str) -> Result<()> {
    unsafe {
        OpenClipboard(Some(hwnd))?;
        let result = fill_clipboard(text);
        let _ = CloseClipboard();
        result
    }
}

unsafe fn fill_clipboard(text: &str) -> Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    EmptyClipboard()?;
    let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)?;
    let dst = GlobalLock(mem) as *mut u16;
    if dst.is_null() {
        bail!("GlobalLock failed");
    }
    std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
    let _ = GlobalUnlock(mem);
    // On success the system owns the memory.
    SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0)))?;
    Ok(())
}

/// Reads CF_UNICODETEXT from the clipboard, if any.
fn read_clipboard(hwnd: HWND) -> Option<String> {
    unsafe {
        OpenClipboard(Some(hwnd)).ok()?;
        let text = read_clipboard_text();
        let _ = CloseClipboard();
        text
    }
}

unsafe fn read_clipboard_text() -> Option<String> {
    let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
    let mem = HGLOBAL(handle.0);
    let ptr = GlobalLock(mem) as *const u16;
    if ptr.is_null() {
        return None;
    }
    let mut len = 0;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
    let _ = GlobalUnlock(mem);
    Some(text)
}
