//! Browser input -> Windows SendInput.
//!
//! Wire format (binary, little-endian, one unordered/unreliable DataChannel):
//!
//! | type | name     | payload                                   |
//! |------|----------|-------------------------------------------|
//! | 0    | key down | u16 vk (Windows VK code)                  |
//! | 1    | key up   | u16 vk                                    |
//! | 2    | mouse rel| i16 dx, i16 dy (PointerLock unadjusted)   |
//! | 3    | mouse abs| u16 x, u16 y (0..65535 over primary mon.) |
//! | 4    | buttons  | u8 bitmask (1=left 2=right 4=middle)      |
//! | 5    | wheel    | i16 delta (WHEEL_DELTA units)             |
//!
//! Anti-cheat note: SendInput is the v0 injection method. Games with kernel
//! anti-cheat may ignore synthetic input or flag the process; accepted for v0.

use tracing::{debug, warn};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    MOUSEINPUT, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, VIRTUAL_KEY,
};

const TYPE_KEY_DOWN: u8 = 0;
const TYPE_KEY_UP: u8 = 1;
const TYPE_MOUSE_REL: u8 = 2;
const TYPE_MOUSE_ABS: u8 = 3;
const TYPE_BUTTONS: u8 = 4;
const TYPE_WHEEL: u8 = 5;

pub struct InputSink {
    prev_buttons: u8,
}

impl InputSink {
    pub fn new() -> Self {
        Self { prev_buttons: 0 }
    }

    /// Handle one data-channel packet. Malformed packets are logged and dropped.
    pub fn handle(&mut self, data: &[u8]) {
        let Some(&tag) = data.first() else { return };
        match tag {
            TYPE_KEY_DOWN => {
                if data.len() >= 3 {
                    let vk = u16::from_le_bytes([data[1], data[2]]);
                    self.key(vk, false);
                }
            }
            TYPE_KEY_UP => {
                if data.len() >= 3 {
                    let vk = u16::from_le_bytes([data[1], data[2]]);
                    self.key(vk, true);
                }
            }
            TYPE_MOUSE_REL => {
                if data.len() >= 5 {
                    let dx = i16::from_le_bytes([data[1], data[2]]);
                    let dy = i16::from_le_bytes([data[3], data[4]]);
                    self.mouse_rel(dx, dy);
                }
            }
            TYPE_MOUSE_ABS => {
                if data.len() >= 5 {
                    let x = u16::from_le_bytes([data[1], data[2]]);
                    let y = u16::from_le_bytes([data[3], data[4]]);
                    self.mouse_abs(x, y);
                }
            }
            TYPE_BUTTONS => {
                if data.len() >= 2 {
                    self.buttons(data[1]);
                }
            }
            TYPE_WHEEL => {
                if data.len() >= 3 {
                    let delta = i16::from_le_bytes([data[1], data[2]]);
                    self.wheel(delta);
                }
            }
            other => debug!(other, "unknown input packet type"),
        }
    }

    fn key(&self, vk: u16, up: bool) {
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
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        if sent != 1 {
            warn!(vk, up, "SendInput(key) failed");
        }
    }

    fn mouse_rel(&self, dx: i16, dy: i16) {
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
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    }

    fn mouse_abs(&self, x: u16, y: u16) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: x as i32,
                    dy: y as i32,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    }

    /// Bitmask diff -> press/release events for each button.
    fn buttons(&mut self, mask: u8) {
        let changed = mask ^ self.prev_buttons;
        self.prev_buttons = mask;
        if changed == 0 {
            return;
        }
        let mut inputs = Vec::with_capacity(3);
        for (bit, down_flag, up_flag) in [
            (1u8, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            (2u8, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            (4u8, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        ] {
            if changed & bit != 0 {
                let flags = if mask & bit != 0 { down_flag } else { up_flag };
                inputs.push(INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: 0,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }
        if !inputs.is_empty() {
            unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        }
    }

    fn wheel(&self, delta: i16) {
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
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    }
}
