//! Windows input for a remote session, behind the shell's InputCapture seam.
mod chords;
mod hook;
pub mod keyboard;
#[cfg(test)]
mod live_tests;
pub mod protocol;
mod state;

use crate::{
    net::InputLink,
    surface::{InputCapture, Rect},
};
use chords::{Chords, Escape};
use hook::{KeyEvent, KeyboardHook};
use state::CaptureState;
pub use state::{CaptureStatus, Mode};
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{ClientToScreen, ScreenToClient},
    UI::{
        Input::{
            KeyboardAndMouse::{
                GetAsyncKeyState, RegisterHotKey, ReleaseCapture, SetCapture, UnregisterHotKey,
                MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
            },
            *,
        },
        WindowsAndMessaging::*,
    },
};
const HOTKEY_RELEASE: i32 = 0x4250;
const HOTKEY_QUIT: i32 = 0x4251;

pub struct SessionInput {
    state: CaptureState,
    status: CaptureStatus,
    link: InputLink,
    stop: Arc<AtomicBool>,
    wake: Arc<dyn Fn() + Send + Sync>,
    viewport: Option<Rect>,
    picture: Option<protocol::Rect>,
    hook: Option<KeyboardHook>,
    clipped: bool,
    raw: bool,
    allowed: bool,
    keyboard_captured: bool,
    chords: Chords,
    sequence: protocol::Sequence,
    held: BTreeSet<(u16, bool)>,
    buttons: u8,
    motion: [i32; 4],
    absolute: Option<(u16, u16)>,
    error: Option<String>,
    sync_epoch: Option<u64>,
    hotkey_window: Option<usize>,
}

impl SessionInput {
    pub fn new(
        mode: Mode,
        link: InputLink,
        stop: Arc<AtomicBool>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let state = CaptureState::new(mode);
        let status = state.status();
        Self {
            state,
            status,
            link,
            stop,
            wake,
            viewport: None,
            picture: None,
            hook: None,
            clipped: false,
            raw: false,
            allowed: false,
            keyboard_captured: false,
            chords: Chords::default(),
            sequence: protocol::Sequence::new(),
            held: BTreeSet::new(),
            buttons: 0,
            motion: [0; 4],
            absolute: None,
            error: None,
            sync_epoch: None,
            hotkey_window: None,
        }
    }
    pub fn status(&self) -> CaptureStatus {
        self.status.clone()
    }
    fn publish(&self) {
        self.status.publish(self.state.snapshot(
            self.hook.as_ref().is_some_and(|h| h.active()),
            self.clipped,
            self.error.clone(),
        ));
    }
    fn clear_pending(&mut self) {
        self.held.clear();
        self.buttons = 0;
        self.motion = [0; 4];
        self.absolute = None;
    }
    fn hotkeys(&mut self, hwnd: HWND) {
        if !self.state.active || self.hotkey_window.is_some() || hwnd.is_invalid() {
            return;
        }
        let modifiers = MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT;
        let setup = unsafe {
            RegisterHotKey(Some(hwnd), HOTKEY_RELEASE, modifiers, 0x5a)
                .and_then(|_| RegisterHotKey(Some(hwnd), HOTKEY_QUIT, modifiers, 0x51))
        };
        if let Err(error) = setup {
            unsafe {
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_RELEASE);
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_QUIT);
            }
            self.error = Some(format!("escape hotkey registration failed: {error}"));
            self.state.active = false;
            self.stop.store(true, Ordering::Release);
            self.release();
        } else {
            self.hotkey_window = Some(hwnd.0 as usize);
        }
    }
    fn release(&mut self) {
        self.link.release(self.state.epoch);
        if self.state.mode == Mode::Work && self.buttons != 0 {
            unsafe {
                let _ = ReleaseCapture();
            }
        }
        self.clear_pending();
        self.hook.take();
        if self.clipped {
            unsafe {
                let _ = ClipCursor(None);
                SetCursor(LoadCursorW(None, IDC_ARROW).ok());
            }
            self.clipped = false;
        }
        if self.raw {
            let devices = [RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 2,
                dwFlags: RIDEV_REMOVE,
                hwndTarget: HWND::default(),
            }];
            unsafe {
                let _ =
                    RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32);
            }
            self.raw = false;
        }
        self.allowed = false;
        self.keyboard_captured = false;
    }
    fn refresh(&mut self, hwnd: HWND) {
        let net = self.link.snapshot();
        let foreground = !hwnd.is_invalid() && unsafe { GetForegroundWindow() } == hwnd;
        if self.state.epoch != net.epoch {
            self.release();
            self.state.epoch = net.epoch;
            self.sequence.enter_epoch(&net.epoch.to_string());
            self.chords.reset();
            self.state.reason = "new-epoch";
        }
        let was_focused = self.state.focused;
        self.state.focused = foreground;
        self.state.active = !self.stop.load(Ordering::Acquire);
        self.state.ready = net.ready;
        self.state.rect = self.picture.is_some();
        if was_focused && !foreground {
            self.state.reason = "focus-loss";
            self.chords.reset();
        }
        if !was_focused && foreground {
            // Held modifiers on re-entry must still permit the escape chord,
            // without inventing a remote press for a key already held locally.
            for vk in 0xa0..=0xa5 {
                self.chords
                    .seed(vk, unsafe { GetAsyncKeyState(vk as i32) } < 0);
            }
        }
        if (self.allowed && !self.state.eligible())
            || (self.keyboard_captured && !self.state.captured())
        {
            self.release();
        }
        if self.state.mode == Mode::Game && self.state.captured() && self.hook.is_none() {
            let setup = (|| -> anyhow::Result<()> {
                self.hook = Some(KeyboardHook::install(
                    hwnd,
                    self.wake.clone(),
                    self.stop.clone(),
                    self.link.ready_signal(),
                )?);
                let devices = [RAWINPUTDEVICE {
                    usUsagePage: 1,
                    usUsage: 2,
                    dwFlags: RAWINPUTDEVICE_FLAGS(0),
                    hwndTarget: hwnd,
                }];
                unsafe {
                    RegisterRawInputDevices(
                        &devices,
                        std::mem::size_of::<RAWINPUTDEVICE>() as u32,
                    )?;
                }
                self.raw = true;
                self.clip(hwnd)?;
                Ok(())
            })();
            if let Err(error) = setup {
                self.error = Some(format!("capture: {error:#}"));
                self.state.requested = false;
                self.state.reason = "capture-error";
                self.release();
            }
        }
        if self.hook.as_ref().is_some_and(|h| !h.active()) {
            self.error = self
                .hook
                .as_ref()
                .filter(|h| h.overflowed())
                .map(|_| "keyboard hook queue overflow; capture released".into());
            self.state.requested = false;
            self.state.reason = "hook-inactive";
            self.release();
        }
        self.allowed = self.state.eligible();
        self.keyboard_captured = self.state.captured();
        self.publish();
        if self.state.mode == Mode::Work
            && self.state.eligible()
            && self.sync_epoch != Some(self.state.epoch)
            && self.sequenced(|seq| protocol::cursor_sync(seq).to_vec())
        {
            self.sync_epoch = Some(self.state.epoch);
        }
    }
    fn clip(&mut self, hwnd: HWND) -> anyhow::Result<()> {
        let Some(picture) = self.picture else {
            return Ok(());
        };
        if unsafe { GetForegroundWindow() } != hwnd {
            anyhow::bail!("refused cursor clip: window is not foreground");
        }
        let mut point = POINT {
            x: picture.x as i32,
            y: picture.y as i32,
        };
        unsafe {
            if !ClientToScreen(hwnd, &mut point).as_bool() {
                anyhow::bail!("ClientToScreen failed");
            }
            let rect = RECT {
                left: point.x,
                top: point.y,
                right: point.x + picture.w as i32,
                bottom: point.y + picture.h as i32,
            };
            ClipCursor(Some(&rect))?;
            SetCursor(None);
        }
        self.clipped = true;
        Ok(())
    }
    fn send(&mut self, bytes: Vec<u8>) -> bool {
        self.link.send(self.state.epoch, bytes)
    }
    fn sequenced(&mut self, build: impl FnOnce(u16) -> Vec<u8>) -> bool {
        let seq = self.sequence.alloc();
        if self.send(build(seq)) {
            true
        } else {
            self.sequence.release(seq);
            false
        }
    }
    fn key(&mut self, event: KeyEvent, hwnd: HWND) -> bool {
        let decision = self.chords.key(event);
        if let Some(escape) = decision.escape {
            self.escape(escape, hwnd);
            return true;
        }
        if self.state.captured() {
            for key in decision.forward {
                self.forward_key(key);
            }
        }
        decision.consumed || self.state.captured()
    }
    fn escape(&mut self, escape: Escape, hwnd: HWND) {
        self.release();
        match escape {
            Escape::Toggle => {
                self.state.toggle();
                if self.state.requested {
                    self.state.video_focused = true;
                }
            }
            Escape::Quit => {
                self.state.reason = "quit-chord";
                self.state.active = false;
                self.stop.store(true, Ordering::Release);
            }
        }
        self.refresh(hwnd);
        (self.wake)();
    }
    fn forward_key(&mut self, event: KeyEvent) {
        let Some(key) = keyboard::physical_key(event.vk, event.scan, event.extended) else {
            return;
        };
        let snapshot = self.link.snapshot();
        if !snapshot.scancode && key.vk == 0 {
            return;
        } // browser has no legacy VK for this physical key
        let id = if key.scan != 0 {
            (key.scan, key.extended)
        } else {
            (key.vk | 0x100, false)
        };
        if event.up {
            if !self.held.remove(&id) {
                return;
            }
        } else if !self.held.insert(id) {
            return;
        }
        let packet = if snapshot.scancode {
            protocol::scan_key(key, event.up).to_vec()
        } else {
            let vk = match key.vk {
                0xa0 | 0xa1 => 0x10,
                0xa2 | 0xa3 => 0x11,
                0xa4 | 0xa5 => 0x12,
                vk => vk,
            };
            protocol::key(vk, event.up).to_vec()
        };
        if !self.send(packet) {
            self.release();
            self.state.requested = false;
            self.state.reason = "input-refused";
            self.publish();
        }
    }
    fn flush_modifiers(&mut self) {
        if self.state.captured() {
            for key in self.chords.flush() {
                self.forward_key(key);
            }
        }
    }
    fn drain_hook(&mut self, hwnd: HWND) {
        // Drain first so a release chord can remove the hook without discarding
        // already queued chord key-ups. That makes a second chord reliable even
        // when SendInput delivered its entire first chord during a slow frame.
        let mut keys = Vec::new();
        if let Some(hook) = &mut self.hook {
            while let Some(key) = hook.pop() {
                keys.push(key);
            }
        }
        for key in keys {
            self.key(key, hwnd);
        }
    }
    fn point(&self, x: f64, y: f64, drag: bool) -> Option<(u16, u16)> {
        let viewport = self.viewport?;
        let picture = self.picture?;
        if !drag && !inside(viewport, x, y) {
            return None;
        }
        protocol::absolute_point(
            picture,
            x.clamp(picture.x, picture.x + picture.w),
            y.clamp(picture.y, picture.y + picture.h),
        )
    }
    fn flush_motion(&mut self) {
        if !self.state.eligible() {
            self.motion = [0; 4];
            self.absolute = None;
            return;
        }
        let mut budget = 32;
        if let Some((x, y)) = self.absolute {
            if !self.sequenced(|seq| protocol::absolute(seq, x, y).to_vec()) {
                return;
            }
            self.absolute = None;
            budget -= 1;
        }
        while budget > 0 && self.motion.iter().any(|n| *n != 0) {
            let chunk = self
                .motion
                .map(|n| n.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
            if !self.sequenced(|seq| {
                protocol::motion(seq, chunk[0], chunk[1], chunk[2], chunk[3]).to_vec()
            }) {
                break;
            }
            for (pending, part) in self.motion.iter_mut().zip(chunk) {
                *pending -= part as i32;
            }
            budget -= 1;
        }
        if self.absolute.is_some() || self.motion.iter().any(|n| *n != 0) {
            (self.wake)();
        }
    }
    fn raw_motion(&mut self, lp: LPARAM) {
        if !self.state.captured() || self.state.mode != Mode::Game {
            return;
        }
        unsafe {
            let mut size = 0;
            if GetRawInputData(
                HRAWINPUT(lp.0 as *mut _),
                RID_INPUT,
                None,
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            ) == u32::MAX
                || size > 65536
            {
                return;
            }
            let mut storage = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
            let count = GetRawInputData(
                HRAWINPUT(lp.0 as *mut _),
                RID_INPUT,
                Some(storage.as_mut_ptr().cast()),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            );
            if count == u32::MAX || (count as usize) < std::mem::size_of::<RAWINPUT>() {
                return;
            }
            let input = &*(storage.as_ptr() as *const RAWINPUT);
            if input.header.dwType != RIM_TYPEMOUSE.0 {
                return;
            }
            let mouse = input.data.mouse;
            if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 {
                return;
            }
            self.motion[0] = self.motion[0].saturating_add(mouse.lLastX);
            self.motion[1] = self.motion[1].saturating_add(mouse.lLastY);
        }
    }
    fn cursor(&self) {
        unsafe {
            if self.state.mode == Mode::Game {
                SetCursor(None);
                return;
            }
            let id = match self.link.snapshot().cursor_kind {
                1 => IDC_IBEAM,
                2 => IDC_HAND,
                3 => IDC_CROSS,
                4 => IDC_SIZEALL,
                5 => IDC_SIZENS,
                6 => IDC_SIZEWE,
                7 => IDC_SIZENESW,
                8 => IDC_SIZENWSE,
                9 => IDC_WAIT,
                10 => IDC_APPSTARTING,
                11 => IDC_NO,
                12 => IDC_HELP,
                13 => IDC_UPARROW,
                _ => IDC_ARROW,
            };
            SetCursor(LoadCursorW(None, id).ok());
        }
    }
}
impl InputCapture for SessionInput {
    fn captured(&self) -> bool {
        self.state.captured()
    }
    fn on_frame(&mut self, hwnd: HWND, video_px: Option<Rect>) {
        self.hotkeys(hwnd);
        self.viewport = video_px;
        let net = self.link.snapshot();
        self.picture = video_px.and_then(|rect| video_content(rect, net.width, net.height));
        self.refresh(hwnd);
        self.drain_hook(hwnd);
        if self.state.captured() && self.state.mode == Mode::Game {
            if let Err(error) = self.clip(hwnd) {
                self.error = Some(format!("clip: {error:#}"));
                self.state.requested = false;
                self.release();
            }
        }
        if self.state.eligible() && self.state.mode == Mode::Work && self.buttons != 0 {
            unsafe {
                SetCapture(hwnd);
            }
        }
        self.flush_motion();
        self.publish();
        if self.state.captured() && self.state.mode == Mode::Work {
            self.cursor();
        }
    }
    fn on_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        self.refresh(hwnd);
        self.drain_hook(hwnd);
        match msg {
            WM_HOTKEY
                if self.state.active && matches!(wp.0 as i32, HOTKEY_RELEASE | HOTKEY_QUIT) =>
            {
                let quit = wp.0 as i32 == HOTKEY_QUIT;
                self.chords.reserve(if quit { 0x51 } else { 0x5a });
                self.escape(if quit { Escape::Quit } else { Escape::Toggle }, hwnd);
                Some(LRESULT(0))
            }
            WM_KILLFOCUS | WM_ACTIVATEAPP if msg == WM_KILLFOCUS || wp.0 == 0 => {
                self.state.focused = false;
                self.state.reason = "focus-loss";
                self.chords.reset();
                self.release();
                self.publish();
                None
            }
            WM_CLOSE | WM_DESTROY => {
                self.state.active = false;
                self.stop.store(true, Ordering::Release);
                self.release();
                self.publish();
                None
            }
            WM_INPUT => {
                self.raw_motion(lp);
                (self.wake)();
                None /* DefWindowProc releases foreground Raw Input storage. */
            }
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
                if self.hook.is_some() {
                    return Some(LRESULT(0));
                }
                let vk = wp.0 as u16;
                // Work mode leaves local system navigation to Windows.
                if self.state.mode == Mode::Work
                    && (matches!(vk, 0x5b | 0x5c)
                        || (vk == 0x09 && unsafe { GetAsyncKeyState(0x12) } < 0))
                {
                    return None;
                }
                let scan = ((lp.0 >> 16) & 255) as u16;
                let extended = lp.0 & (1 << 24) != 0;
                // Only modifiers need side resolution here. Escape detection
                // uses the native logical letter, while packet VK metadata is
                // derived later from the physical key like the browser.
                let vk = if matches!(vk, 0x10..=0x12) {
                    keyboard::physical_key(vk, scan, extended).map_or(vk, |key| key.vk)
                } else {
                    vk
                };
                let event = KeyEvent {
                    vk,
                    scan,
                    extended,
                    up: matches!(msg, WM_KEYUP | WM_SYSKEYUP),
                };
                let consumed = self.key(event, hwnd);
                if consumed && !matches!(msg, WM_SYSKEYDOWN | WM_SYSKEYUP) {
                    Some(LRESULT(0))
                } else {
                    None
                }
            }
            WM_CHAR | WM_SYSCHAR | WM_DEADCHAR | WM_SYSDEADCHAR if self.state.captured() => {
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE if self.state.mode == Mode::Work && self.state.eligible() => {
                let (x, y) = client_point(lp);
                if let Some(point) = self.point(x, y, self.buttons != 0) {
                    self.absolute = Some(point);
                    (self.wake)();
                    Some(LRESULT(0))
                } else {
                    None
                }
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
            | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
                if !self.state.eligible() {
                    return None;
                }
                let (x, y) = client_point(lp);
                let point = self.point(x, y, self.buttons != 0);
                if self.state.mode == Mode::Work && point.is_none() {
                    if self.state.video_focused {
                        self.state.video_focused = false;
                        self.release();
                        self.publish();
                    }
                    return None;
                }
                self.state.video_focused = true;
                self.flush_modifiers();
                let bit = match msg {
                    WM_LBUTTONDOWN | WM_LBUTTONUP => 1,
                    WM_RBUTTONDOWN | WM_RBUTTONUP => 2,
                    WM_MBUTTONDOWN | WM_MBUTTONUP => 4,
                    _ => {
                        if wp.0 >> 16 == 1 {
                            8
                        } else {
                            16
                        }
                    }
                };
                let down = matches!(
                    msg,
                    WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
                );
                if down {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
                if self.state.mode == Mode::Work {
                    self.absolute = None;
                    self.motion[0] = 0;
                    self.motion[1] = 0;
                    let (x, y) = point.unwrap();
                    let mask = self.buttons;
                    self.sequenced(|seq| protocol::buttons_at(seq, x, y, mask).to_vec());
                } else {
                    self.send(protocol::buttons(self.buttons).to_vec());
                }
                if self.buttons == 0 {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                }
                self.publish();
                Some(LRESULT(if matches!(msg, WM_XBUTTONDOWN | WM_XBUTTONUP) {
                    1
                } else {
                    0
                }))
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL if self.state.eligible() => {
                // The browser reserves Ctrl+wheel for its local viewport zoom.
                // Zoom belongs to the UI track; never turn it into remote scroll.
                if unsafe { GetAsyncKeyState(0x11) } < 0 {
                    return Some(LRESULT(0));
                }
                let mut point = POINT {
                    x: (lp.0 & 65535) as i16 as i32,
                    y: ((lp.0 >> 16) & 65535) as i16 as i32,
                };
                unsafe {
                    let _ = ScreenToClient(hwnd, &mut point);
                }
                if self.state.mode == Mode::Work
                    && self
                        .point(point.x as f64, point.y as f64, self.buttons != 0)
                        .is_none()
                {
                    return None;
                }
                self.flush_modifiers();
                let delta = ((wp.0 >> 16) & 65535) as i16 as i32;
                let axis = if msg == WM_MOUSEHWHEEL { 2 } else { 3 };
                self.motion[axis] = self.motion[axis].saturating_add(delta);
                (self.wake)();
                Some(LRESULT(0))
            }
            WM_SETCURSOR if self.state.captured() && (lp.0 & 65535) as u32 == HTCLIENT => {
                self.cursor();
                Some(LRESULT(1))
            }
            _ => None,
        }
    }
}
impl Drop for SessionInput {
    fn drop(&mut self) {
        self.state.active = false;
        self.state.reason = "session-end";
        self.release();
        if let Some(hwnd) = self.hotkey_window.take() {
            unsafe {
                let hwnd = HWND(hwnd as *mut _);
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_RELEASE);
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_QUIT);
            }
        }
        self.publish();
    }
}
fn client_point(lp: LPARAM) -> (f64, f64) {
    (
        (lp.0 & 65535) as i16 as f64,
        ((lp.0 >> 16) & 65535) as i16 as f64,
    )
}
fn inside(rect: Rect, x: f64, y: f64) -> bool {
    x >= rect.x as f64
        && y >= rect.y as f64
        && x <= (rect.x + rect.w) as f64
        && y <= (rect.y + rect.h) as f64
}
/// Matches the media renderer's integer blit rect, including pixel rounding.
fn video_content(rect: Rect, width: u32, height: u32) -> Option<protocol::Rect> {
    if width == 0
        || height == 0
        || ![rect.x, rect.y, rect.w, rect.h]
            .iter()
            .all(|v| v.is_finite())
        || rect.w <= 0.0
        || rect.h <= 0.0
    {
        return None;
    }
    let scale = (rect.w / width as f32).min(rect.h / height as f32);
    let (w, h) = (
        (width as f32 * scale).round(),
        (height as f32 * scale).round(),
    );
    Some(protocol::Rect {
        x: (rect.x + ((rect.w - w) / 2.0).floor()) as i32 as f64,
        y: (rect.y + ((rect.h - h) / 2.0).floor()) as i32 as f64,
        w: w as f64,
        h: h as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn integer_letterbox_matches_video_blit_and_maps_bars_at_edges() {
        let r = video_content(
            Rect {
                x: 10.0,
                y: 20.0,
                w: 801.0,
                h: 601.0,
            },
            1920,
            1080,
        )
        .unwrap();
        assert_eq!(
            r,
            protocol::Rect {
                x: 10.0,
                y: 95.0,
                w: 801.0,
                h: 451.0
            }
        );
        assert_eq!(protocol::absolute_point(r, r.x, r.y), Some((0, 0)));
        assert_eq!(
            protocol::absolute_point(r, r.x + r.w, r.y + r.h),
            Some((65535, 65535))
        );
        assert!(video_content(Rect::default(), 1920, 1080).is_none());
    }
}
