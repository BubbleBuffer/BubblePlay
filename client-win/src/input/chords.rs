//! Reserve Ctrl+Alt+Shift+Z/Q, including repeats and releases. Modifier
//! presses wait until a non-chord key, pointer action, or modifier release
//! proves they are ordinary remote input; the escape chord never leaks its
//! terminal key or a fresh modifier press into the remote session.
use super::hook::KeyEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Escape {
    Toggle,
    Quit,
}
pub struct Chords {
    down: [bool; 256],
    consumed: [bool; 256],
    pending: Vec<KeyEvent>,
    hotkey_pending: Option<u16>,
}
impl Default for Chords {
    fn default() -> Self {
        Self {
            down: [false; 256],
            consumed: [false; 256],
            pending: Vec::new(),
            hotkey_pending: None,
        }
    }
}
pub struct Decision {
    pub forward: Vec<KeyEvent>,
    pub escape: Option<Escape>,
    pub consumed: bool,
}
fn modifier(vk: u16) -> bool {
    matches!(vk, 0x10..=0x12 | 0xa0..=0xa5)
}
impl Chords {
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn seed(&mut self, vk: u16, down: bool) {
        if vk < 256 {
            self.down[vk as usize] = down;
        }
    }
    pub fn flush(&mut self) -> Vec<KeyEvent> {
        std::mem::take(&mut self.pending)
    }
    pub fn reserve(&mut self, terminal: u16) {
        self.pending.clear();
        for key in 0..256 {
            if self.down[key] && modifier(key as u16) {
                self.consumed[key] = true;
            }
        }
        self.hotkey_pending = Some(terminal);
    }
    fn has(&self, generic: u16, left: u16, right: u16) -> bool {
        [generic, left, right]
            .iter()
            .any(|vk| self.down[*vk as usize])
    }
    pub fn key(&mut self, event: KeyEvent) -> Decision {
        if event.vk >= 256 {
            return Decision {
                forward: vec![event],
                escape: None,
                consumed: false,
            };
        }
        let vk = event.vk as usize;
        let repeated = self.down[vk] && !event.up;
        self.down[vk] = !event.up;
        if let Some(terminal) = self.hotkey_pending {
            let any_mod = self
                .down
                .iter()
                .enumerate()
                .any(|(vk, down)| *down && modifier(vk as u16));
            if modifier(event.vk) {
                if event.up {
                    self.consumed[vk] = false;
                    if !any_mod {
                        self.hotkey_pending = None;
                    }
                }
                return Decision {
                    forward: Vec::new(),
                    escape: None,
                    consumed: true,
                };
            }
            if event.vk == terminal && event.up {
                if !any_mod {
                    self.hotkey_pending = None;
                }
                return Decision {
                    forward: Vec::new(),
                    escape: None,
                    consumed: true,
                };
            }
            if !any_mod {
                self.hotkey_pending = None;
            }
        }
        if self.consumed[vk] {
            if event.up {
                self.consumed[vk] = false;
            }
            return Decision {
                forward: Vec::new(),
                escape: None,
                consumed: true,
            };
        }
        let armed =
            self.has(0x11, 0xa2, 0xa3) && self.has(0x12, 0xa4, 0xa5) && self.has(0x10, 0xa0, 0xa1);
        if !event.up && !repeated && armed && matches!(event.vk, 0x5a | 0x51) {
            self.pending.clear();
            for key in 0..256 {
                if self.down[key] && modifier(key as u16) {
                    self.consumed[key] = true;
                }
            }
            self.consumed[vk] = true;
            return Decision {
                forward: Vec::new(),
                escape: Some(if event.vk == 0x5a {
                    Escape::Toggle
                } else {
                    Escape::Quit
                }),
                consumed: true,
            };
        }
        if repeated {
            return Decision {
                forward: Vec::new(),
                escape: None,
                consumed: false,
            };
        }
        if modifier(event.vk) && !event.up {
            self.pending.push(event);
            return Decision {
                forward: Vec::new(),
                escape: None,
                consumed: false,
            };
        }
        let mut forward = self.flush();
        forward.push(event);
        Decision {
            forward,
            escape: None,
            consumed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(vk: u16, up: bool) -> KeyEvent {
        KeyEvent {
            vk,
            up,
            ..Default::default()
        }
    }
    #[test]
    fn both_escape_chords_consume_all_fresh_parts_and_repeat() {
        for (terminal, escape) in [(0x5a, Escape::Toggle), (0x51, Escape::Quit)] {
            let mut detector = Chords::default();
            for vk in [0xa3, 0xa5, 0xa1] {
                assert!(detector.key(key(vk, false)).forward.is_empty());
            }
            let result = detector.key(key(terminal, false));
            assert_eq!(result.escape, Some(escape));
            assert!(result.forward.is_empty());
            assert_eq!(detector.key(key(terminal, false)).escape, None);
            for vk in [terminal, 0xa1, 0xa5, 0xa3] {
                assert!(detector.key(key(vk, true)).forward.is_empty());
            }
            assert_eq!(
                detector.key(key(0x41, false)).forward,
                vec![key(0x41, false)]
            );
        }
    }
    #[test]
    fn ordinary_shortcuts_and_lone_modifiers_forward_in_order() {
        let mut detector = Chords::default();
        assert!(detector.key(key(0x11, false)).forward.is_empty());
        assert_eq!(
            detector.key(key(0x43, false)).forward,
            vec![key(0x11, false), key(0x43, false)]
        );
        assert_eq!(detector.key(key(0x43, true)).forward, vec![key(0x43, true)]);
        assert_eq!(detector.key(key(0x11, true)).forward, vec![key(0x11, true)]);
        assert!(detector.key(key(0x12, false)).forward.is_empty());
        assert_eq!(
            detector.key(key(0x12, true)).forward,
            vec![key(0x12, false), key(0x12, true)]
        );
    }
    #[test]
    fn incomplete_or_wrong_chord_is_not_reserved() {
        for missing in [0x10, 0x11, 0x12] {
            let mut detector = Chords::default();
            for vk in [0x10, 0x11, 0x12] {
                if vk != missing {
                    detector.key(key(vk, false));
                }
            }
            assert_eq!(detector.key(key(0x5a, false)).escape, None);
        }
    }
    #[test]
    fn second_press_toggles_again_without_releasing_modifiers() {
        let mut detector = Chords::default();
        for vk in [0x10, 0x11, 0x12] {
            detector.key(key(vk, false));
        }
        assert_eq!(detector.key(key(0x5a, false)).escape, Some(Escape::Toggle));
        detector.key(key(0x5a, true));
        assert_eq!(detector.key(key(0x5a, false)).escape, Some(Escape::Toggle));
    }
    #[test]
    fn registered_hotkey_consumes_modifiers_even_when_notification_precedes_key_messages() {
        let mut detector = Chords::default();
        detector.reserve(0x5a);
        for vk in [0xa2, 0xa4, 0xa0] {
            assert!(detector.key(key(vk, false)).forward.is_empty());
        }
        for vk in [0x5a, 0xa0, 0xa4, 0xa2] {
            assert!(detector.key(key(vk, true)).forward.is_empty());
        }
        assert_eq!(
            detector.key(key(0x5a, false)).forward,
            vec![key(0x5a, false)],
            "an ordinary later Z must work"
        );
    }
}
