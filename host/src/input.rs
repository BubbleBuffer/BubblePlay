//! Browser input -> Windows SendInput.
//!
//! Wire format (binary, little-endian). Control is reliable; motion is lossy.
//!
//! | type | name     | payload                                   |
//! |------|----------|-------------------------------------------|
//! | 0    | key down | u16 vk (Windows VK code)                  |
//! | 1    | key up   | u16 vk                                    |
//! | 2    | mouse rel| i16 dx, i16 dy (PointerLock unadjusted)   |
//! | 3    | mouse abs| u16 x, u16 y (0..65535 over primary mon.) |
//! | 4    | buttons  | u8 bitmask (1=left 2=right 4=middle 8=X1 16=X2) |
//! | 5    | wheel    | i16 delta (WHEEL_DELTA units)             |
//! | 6    | release  | no payload                                |
//! | 7    | motion v2| u16 seq, i16 dx, dy, wheel_x, wheel_y   |
//! | 8    | absolute v2 | u16 seq, u16 x, u16 y                 |
//!
//! Anti-cheat note: SendInput is the v0 injection method. Games with kernel
//! anti-cheat may ignore synthetic input or flag the process; accepted for v0.

use std::collections::BTreeSet;

use tracing::{debug, warn};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    VIRTUAL_KEY,
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
}

#[derive(Default)]
struct InputState {
    pressed_keys: BTreeSet<u16>,
    buttons: u8,
    last_motion_sequence: Option<u16>,
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
}

#[cfg(test)]
mod tests {
    use super::{decode, newer_sequence, Command, InputState};

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
        assert!(newer_sequence(Some(5), 6));
        assert!(!newer_sequence(Some(6), 5));
    }
}

impl InputSink {
    pub fn new() -> Self {
        Self {
            state: InputState::default(),
        }
    }

    /// Handle one data-channel packet. Malformed packets are logged and dropped.
    pub fn handle(&mut self, data: &[u8]) {
        let Some(command) = decode(data) else {
            debug!(?data, "malformed or unknown input packet");
            return;
        };
        match command {
            Command::Key(vk, up) => {
                self.state.key(vk, up);
                self.key(vk, up);
            }
            Command::Rel(dx, dy) => self.mouse_rel(dx, dy),
            Command::Abs(x, y) => self.mouse_abs(x, y),
            Command::Buttons(mask) => self.buttons(mask),
            Command::Wheel(delta) => self.wheel(delta),
            Command::ReleaseAll => self.release_all(),
            Command::Motion(seq, dx, dy, wx, wy) => {
                if newer_sequence(self.state.last_motion_sequence, seq) {
                    self.state.last_motion_sequence = Some(seq);
                    self.mouse_rel(dx, dy);
                    self.wheel_horizontal(wx);
                    self.wheel(wy);
                }
            }
            Command::Absolute(seq, x, y) => {
                if newer_sequence(self.state.last_motion_sequence, seq) {
                    self.state.last_motion_sequence = Some(seq);
                    self.mouse_abs(x, y);
                }
            }
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
            unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        }
    }

    fn wheel(&self, delta: i16) {
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
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    }

    fn wheel_horizontal(&self, delta: i16) {
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
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
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

    fn send_button_releases(&self, buttons: u8) {
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
            unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        }
    }
}

impl Drop for InputSink {
    fn drop(&mut self) {
        self.release_all();
    }
}
