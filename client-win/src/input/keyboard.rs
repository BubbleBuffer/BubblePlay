//! Win32 keyboard events to the physical keys the browser protocol carries.
//!
//! The low-level hook reports a virtual key (VK), a hardware scan code and an
//! E0 (extended) flag. The protocol sends the physical identity (set-1 scan
//! code plus E0) so the host's own layout maps the key (AltGr, AZERTY, dead
//! keys). The VK rides along as metadata: for a known key it is the VK that
//! `worker/public/input.js` `keyDescriptor` sends for the same
//! `KeyboardEvent.code`, never the local layout's VK. The host ignores that VK
//! on scan-code packets, and uses it only for VK-only packets (scan 0).
//!
//! Pause and NumLock have no make code SendInput accepts, so they travel
//! VK-only, as scan 0 does.
//!
//! Pure: no Win32 calls, so every mapping is unit-tested on any machine.

/// One physical key as the protocol carries it. `scan == 0` means VK-only and
/// the host injects `vk`; otherwise `scan` and `extended` identify the key and
/// `vk` is metadata the host ignores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalKey {
    pub scan: u16,
    pub extended: bool,
    pub vk: u16,
}

/// A `KeyboardEvent.code` with its set-1 make code, E0 flag and the VK that
/// `worker/public/input.js` sends for it (`keyDescriptor(code).vk`, 0 when the
/// browser sends none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanCode {
    pub code: &'static str,
    pub scan: u8,
    pub extended: bool,
    pub vk: u16,
}

/// Every entry of `scanCodes` in fixtures/input-contract.json, in fixture order,
/// with the browser's VK for each code. The tests check this table against the
/// fixture in both directions.
#[rustfmt::skip]
pub const SCAN_CODES: &[ScanCode] = &[
    ScanCode { code: "Escape", scan: 0x01, extended: false, vk: 0x1B },
    ScanCode { code: "Digit1", scan: 0x02, extended: false, vk: 0x31 },
    ScanCode { code: "Digit2", scan: 0x03, extended: false, vk: 0x32 },
    ScanCode { code: "Digit3", scan: 0x04, extended: false, vk: 0x33 },
    ScanCode { code: "Digit4", scan: 0x05, extended: false, vk: 0x34 },
    ScanCode { code: "Digit5", scan: 0x06, extended: false, vk: 0x35 },
    ScanCode { code: "Digit6", scan: 0x07, extended: false, vk: 0x36 },
    ScanCode { code: "Digit7", scan: 0x08, extended: false, vk: 0x37 },
    ScanCode { code: "Digit8", scan: 0x09, extended: false, vk: 0x38 },
    ScanCode { code: "Digit9", scan: 0x0A, extended: false, vk: 0x39 },
    ScanCode { code: "Digit0", scan: 0x0B, extended: false, vk: 0x30 },
    ScanCode { code: "Minus", scan: 0x0C, extended: false, vk: 0xBD },
    ScanCode { code: "Equal", scan: 0x0D, extended: false, vk: 0xBB },
    ScanCode { code: "Backspace", scan: 0x0E, extended: false, vk: 0x08 },
    ScanCode { code: "Tab", scan: 0x0F, extended: false, vk: 0x09 },
    ScanCode { code: "KeyQ", scan: 0x10, extended: false, vk: 0x51 },
    ScanCode { code: "KeyW", scan: 0x11, extended: false, vk: 0x57 },
    ScanCode { code: "KeyE", scan: 0x12, extended: false, vk: 0x45 },
    ScanCode { code: "KeyR", scan: 0x13, extended: false, vk: 0x52 },
    ScanCode { code: "KeyT", scan: 0x14, extended: false, vk: 0x54 },
    ScanCode { code: "KeyY", scan: 0x15, extended: false, vk: 0x59 },
    ScanCode { code: "KeyU", scan: 0x16, extended: false, vk: 0x55 },
    ScanCode { code: "KeyI", scan: 0x17, extended: false, vk: 0x49 },
    ScanCode { code: "KeyO", scan: 0x18, extended: false, vk: 0x4F },
    ScanCode { code: "KeyP", scan: 0x19, extended: false, vk: 0x50 },
    ScanCode { code: "BracketLeft", scan: 0x1A, extended: false, vk: 0xDB },
    ScanCode { code: "BracketRight", scan: 0x1B, extended: false, vk: 0xDD },
    ScanCode { code: "Enter", scan: 0x1C, extended: false, vk: 0x0D },
    ScanCode { code: "ControlLeft", scan: 0x1D, extended: false, vk: 0xA2 },
    ScanCode { code: "KeyA", scan: 0x1E, extended: false, vk: 0x41 },
    ScanCode { code: "KeyS", scan: 0x1F, extended: false, vk: 0x53 },
    ScanCode { code: "KeyD", scan: 0x20, extended: false, vk: 0x44 },
    ScanCode { code: "KeyF", scan: 0x21, extended: false, vk: 0x46 },
    ScanCode { code: "KeyG", scan: 0x22, extended: false, vk: 0x47 },
    ScanCode { code: "KeyH", scan: 0x23, extended: false, vk: 0x48 },
    ScanCode { code: "KeyJ", scan: 0x24, extended: false, vk: 0x4A },
    ScanCode { code: "KeyK", scan: 0x25, extended: false, vk: 0x4B },
    ScanCode { code: "KeyL", scan: 0x26, extended: false, vk: 0x4C },
    ScanCode { code: "Semicolon", scan: 0x27, extended: false, vk: 0xBA },
    ScanCode { code: "Quote", scan: 0x28, extended: false, vk: 0xDE },
    ScanCode { code: "Backquote", scan: 0x29, extended: false, vk: 0xC0 },
    ScanCode { code: "ShiftLeft", scan: 0x2A, extended: false, vk: 0xA0 },
    ScanCode { code: "Backslash", scan: 0x2B, extended: false, vk: 0xDC },
    ScanCode { code: "KeyZ", scan: 0x2C, extended: false, vk: 0x5A },
    ScanCode { code: "KeyX", scan: 0x2D, extended: false, vk: 0x58 },
    ScanCode { code: "KeyC", scan: 0x2E, extended: false, vk: 0x43 },
    ScanCode { code: "KeyV", scan: 0x2F, extended: false, vk: 0x56 },
    ScanCode { code: "KeyB", scan: 0x30, extended: false, vk: 0x42 },
    ScanCode { code: "KeyN", scan: 0x31, extended: false, vk: 0x4E },
    ScanCode { code: "KeyM", scan: 0x32, extended: false, vk: 0x4D },
    ScanCode { code: "Comma", scan: 0x33, extended: false, vk: 0xBC },
    ScanCode { code: "Period", scan: 0x34, extended: false, vk: 0xBE },
    ScanCode { code: "Slash", scan: 0x35, extended: false, vk: 0xBF },
    ScanCode { code: "ShiftRight", scan: 0x36, extended: false, vk: 0xA1 },
    ScanCode { code: "NumpadMultiply", scan: 0x37, extended: false, vk: 0x6A },
    ScanCode { code: "AltLeft", scan: 0x38, extended: false, vk: 0xA4 },
    ScanCode { code: "Space", scan: 0x39, extended: false, vk: 0x20 },
    ScanCode { code: "CapsLock", scan: 0x3A, extended: false, vk: 0x14 },
    ScanCode { code: "F1", scan: 0x3B, extended: false, vk: 0x70 },
    ScanCode { code: "F2", scan: 0x3C, extended: false, vk: 0x71 },
    ScanCode { code: "F3", scan: 0x3D, extended: false, vk: 0x72 },
    ScanCode { code: "F4", scan: 0x3E, extended: false, vk: 0x73 },
    ScanCode { code: "F5", scan: 0x3F, extended: false, vk: 0x74 },
    ScanCode { code: "F6", scan: 0x40, extended: false, vk: 0x75 },
    ScanCode { code: "F7", scan: 0x41, extended: false, vk: 0x76 },
    ScanCode { code: "F8", scan: 0x42, extended: false, vk: 0x77 },
    ScanCode { code: "F9", scan: 0x43, extended: false, vk: 0x78 },
    ScanCode { code: "F10", scan: 0x44, extended: false, vk: 0x79 },
    ScanCode { code: "ScrollLock", scan: 0x46, extended: false, vk: 0x91 },
    ScanCode { code: "Numpad7", scan: 0x47, extended: false, vk: 0x67 },
    ScanCode { code: "Numpad8", scan: 0x48, extended: false, vk: 0x68 },
    ScanCode { code: "Numpad9", scan: 0x49, extended: false, vk: 0x69 },
    ScanCode { code: "NumpadSubtract", scan: 0x4A, extended: false, vk: 0x6D },
    ScanCode { code: "Numpad4", scan: 0x4B, extended: false, vk: 0x64 },
    ScanCode { code: "Numpad5", scan: 0x4C, extended: false, vk: 0x65 },
    ScanCode { code: "Numpad6", scan: 0x4D, extended: false, vk: 0x66 },
    ScanCode { code: "NumpadAdd", scan: 0x4E, extended: false, vk: 0x6B },
    ScanCode { code: "Numpad1", scan: 0x4F, extended: false, vk: 0x61 },
    ScanCode { code: "Numpad2", scan: 0x50, extended: false, vk: 0x62 },
    ScanCode { code: "Numpad3", scan: 0x51, extended: false, vk: 0x63 },
    ScanCode { code: "Numpad0", scan: 0x52, extended: false, vk: 0x60 },
    ScanCode { code: "NumpadDecimal", scan: 0x53, extended: false, vk: 0x6E },
    ScanCode { code: "IntlBackslash", scan: 0x56, extended: false, vk: 0xE2 },
    ScanCode { code: "F11", scan: 0x57, extended: false, vk: 0x7A },
    ScanCode { code: "F12", scan: 0x58, extended: false, vk: 0x7B },
    ScanCode { code: "NumpadEqual", scan: 0x59, extended: false, vk: 0x00 },
    ScanCode { code: "F13", scan: 0x64, extended: false, vk: 0x7C },
    ScanCode { code: "F14", scan: 0x65, extended: false, vk: 0x7D },
    ScanCode { code: "F15", scan: 0x66, extended: false, vk: 0x7E },
    ScanCode { code: "F16", scan: 0x67, extended: false, vk: 0x7F },
    ScanCode { code: "F17", scan: 0x68, extended: false, vk: 0x80 },
    ScanCode { code: "F18", scan: 0x69, extended: false, vk: 0x81 },
    ScanCode { code: "F19", scan: 0x6A, extended: false, vk: 0x82 },
    ScanCode { code: "F20", scan: 0x6B, extended: false, vk: 0x83 },
    ScanCode { code: "F21", scan: 0x6C, extended: false, vk: 0x84 },
    ScanCode { code: "F22", scan: 0x6D, extended: false, vk: 0x85 },
    ScanCode { code: "F23", scan: 0x6E, extended: false, vk: 0x86 },
    ScanCode { code: "KanaMode", scan: 0x70, extended: false, vk: 0x00 },
    ScanCode { code: "Lang2", scan: 0x71, extended: false, vk: 0x00 },
    ScanCode { code: "Lang1", scan: 0x72, extended: false, vk: 0x00 },
    ScanCode { code: "IntlRo", scan: 0x73, extended: false, vk: 0x00 },
    ScanCode { code: "F24", scan: 0x76, extended: false, vk: 0x87 },
    ScanCode { code: "Convert", scan: 0x79, extended: false, vk: 0x00 },
    ScanCode { code: "NonConvert", scan: 0x7B, extended: false, vk: 0x00 },
    ScanCode { code: "IntlYen", scan: 0x7D, extended: false, vk: 0x00 },
    ScanCode { code: "NumpadComma", scan: 0x7E, extended: false, vk: 0x00 },
    ScanCode { code: "MediaTrackPrevious", scan: 0x10, extended: true, vk: 0x00 },
    ScanCode { code: "MediaTrackNext", scan: 0x19, extended: true, vk: 0x00 },
    ScanCode { code: "NumpadEnter", scan: 0x1C, extended: true, vk: 0x0D },
    ScanCode { code: "ControlRight", scan: 0x1D, extended: true, vk: 0xA3 },
    ScanCode { code: "AudioVolumeMute", scan: 0x20, extended: true, vk: 0x00 },
    ScanCode { code: "LaunchApp2", scan: 0x21, extended: true, vk: 0x00 },
    ScanCode { code: "MediaPlayPause", scan: 0x22, extended: true, vk: 0x00 },
    ScanCode { code: "MediaStop", scan: 0x24, extended: true, vk: 0x00 },
    ScanCode { code: "AudioVolumeDown", scan: 0x2E, extended: true, vk: 0x00 },
    ScanCode { code: "AudioVolumeUp", scan: 0x30, extended: true, vk: 0x00 },
    ScanCode { code: "BrowserHome", scan: 0x32, extended: true, vk: 0x00 },
    ScanCode { code: "NumpadDivide", scan: 0x35, extended: true, vk: 0x6F },
    ScanCode { code: "PrintScreen", scan: 0x37, extended: true, vk: 0x2C },
    ScanCode { code: "AltRight", scan: 0x38, extended: true, vk: 0xA5 },
    ScanCode { code: "Home", scan: 0x47, extended: true, vk: 0x24 },
    ScanCode { code: "ArrowUp", scan: 0x48, extended: true, vk: 0x26 },
    ScanCode { code: "PageUp", scan: 0x49, extended: true, vk: 0x21 },
    ScanCode { code: "ArrowLeft", scan: 0x4B, extended: true, vk: 0x25 },
    ScanCode { code: "ArrowRight", scan: 0x4D, extended: true, vk: 0x27 },
    ScanCode { code: "End", scan: 0x4F, extended: true, vk: 0x23 },
    ScanCode { code: "ArrowDown", scan: 0x50, extended: true, vk: 0x28 },
    ScanCode { code: "PageDown", scan: 0x51, extended: true, vk: 0x22 },
    ScanCode { code: "Insert", scan: 0x52, extended: true, vk: 0x2D },
    ScanCode { code: "Delete", scan: 0x53, extended: true, vk: 0x2E },
    ScanCode { code: "MetaLeft", scan: 0x5B, extended: true, vk: 0x5B },
    ScanCode { code: "MetaRight", scan: 0x5C, extended: true, vk: 0x5C },
    ScanCode { code: "ContextMenu", scan: 0x5D, extended: true, vk: 0x5D },
    ScanCode { code: "BrowserSearch", scan: 0x65, extended: true, vk: 0x00 },
    ScanCode { code: "BrowserFavorites", scan: 0x66, extended: true, vk: 0x00 },
    ScanCode { code: "BrowserRefresh", scan: 0x67, extended: true, vk: 0x00 },
    ScanCode { code: "BrowserStop", scan: 0x68, extended: true, vk: 0x00 },
    ScanCode { code: "BrowserForward", scan: 0x69, extended: true, vk: 0x00 },
    ScanCode { code: "BrowserBack", scan: 0x6A, extended: true, vk: 0x00 },
    ScanCode { code: "LaunchApp1", scan: 0x6B, extended: true, vk: 0x00 },
    ScanCode { code: "LaunchMail", scan: 0x6C, extended: true, vk: 0x00 },
    ScanCode { code: "MediaSelect", scan: 0x6D, extended: true, vk: 0x00 },
];

const VK_PAUSE: u16 = 0x13;
const VK_NUMLOCK: u16 = 0x90;
const VK_RCONTROL: u16 = 0xA3;
const VK_RMENU: u16 = 0xA5;

/// Maps a hook event (`vkCode`, `scanCode`, E0 flag) to the key sent on the wire.
///
/// - A known physical key (a scan and E0 pair in `SCAN_CODES`) keeps its scan
///   and E0 flag and gets the browser's VK for its code, whatever the local
///   layout reported.
/// - An unknown raw scan keeps its scan and E0 flag and the native VK.
/// - Pause and NumLock, and events without a scan code, are VK-only with the
///   native VK.
///
/// Right Ctrl and right Alt VKs name their side, so E0 is implied even when the
/// hook dropped the flag. Returns `None` only when the event has neither a
/// usable scan code nor a usable VK.
pub fn physical_key(vk: u16, scan: u16, extended: bool) -> Option<PhysicalKey> {
    // Pause and NumLock have no make code SendInput accepts (the hook reports
    // scan 0x45 for both), so the VK decides and they go VK-only.
    if matches!(vk, VK_PAUSE | VK_NUMLOCK) {
        return vk_only(vk);
    }
    // Scan 0 (no hardware code) and out-of-range codes fall back to the VK.
    if scan == 0 || scan > 0xFF {
        return vk_only(vk);
    }
    let scan = scan as u8;
    let extended = extended || matches!(vk, VK_RCONTROL | VK_RMENU) || extended_only(scan);
    Some(match code_at(scan, extended) {
        Some(key) => PhysicalKey {
            scan: u16::from(scan),
            extended,
            vk: key.vk,
        },
        None => PhysicalKey {
            scan: u16::from(scan),
            extended,
            vk,
        },
    })
}

fn vk_only(vk: u16) -> Option<PhysicalKey> {
    (1..=0xFF).contains(&vk).then_some(PhysicalKey {
        scan: 0,
        extended: false,
        vk,
    })
}

fn code_at(scan: u8, extended: bool) -> Option<&'static ScanCode> {
    SCAN_CODES
        .iter()
        .find(|key| key.scan == scan && key.extended == extended)
}

/// A scan code that exists only as an E0 key in the table (Win, media and
/// browser keys) is extended even when the hook dropped the flag. Codes that
/// have both forms keep the flag the hook reported.
fn extended_only(scan: u8) -> bool {
    SCAN_CODES
        .iter()
        .any(|key| key.scan == scan && key.extended)
        && !SCAN_CODES
            .iter()
            .any(|key| key.scan == scan && !key.extended)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const FIXTURE: &str = include_str!("../../../fixtures/input-contract.json");

    /// Codes for which `keyDescriptor` in worker/public/input.js sends VK 0.
    const BROWSER_SENDS_NO_VK: &[&str] = &[
        "NumpadEqual",
        "KanaMode",
        "Lang2",
        "Lang1",
        "IntlRo",
        "Convert",
        "NonConvert",
        "IntlYen",
        "NumpadComma",
        "MediaTrackPrevious",
        "MediaTrackNext",
        "AudioVolumeMute",
        "LaunchApp2",
        "MediaPlayPause",
        "MediaStop",
        "AudioVolumeDown",
        "AudioVolumeUp",
        "BrowserHome",
        "BrowserSearch",
        "BrowserFavorites",
        "BrowserRefresh",
        "BrowserStop",
        "BrowserForward",
        "BrowserBack",
        "LaunchApp1",
        "LaunchMail",
        "MediaSelect",
    ];

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).expect("fixture parses")
    }

    fn key(code: &str) -> ScanCode {
        *SCAN_CODES
            .iter()
            .find(|key| key.code == code)
            .unwrap_or_else(|| panic!("{code} missing from table"))
    }

    /// Maps a table key the way the hook would report it, with a layout VK that is
    /// not the browser's, so the test also proves the layout VK is replaced.
    fn map_from_layout(entry: ScanCode, layout_vk: u16) -> Option<PhysicalKey> {
        physical_key(layout_vk, u16::from(entry.scan), entry.extended)
    }

    #[test]
    fn table_matches_every_fixture_scan_code_exactly() {
        let fixture = fixture();
        let codes = fixture["scanCodes"].as_object().expect("scanCodes object");
        assert_eq!(
            SCAN_CODES.len(),
            codes.len(),
            "table and fixture sizes differ"
        );
        for entry in SCAN_CODES {
            let fixed = codes
                .get(entry.code)
                .unwrap_or_else(|| panic!("{} not in fixture", entry.code));
            assert_eq!(
                u64::from(entry.scan),
                fixed["scan"].as_u64().unwrap(),
                "{} scan",
                entry.code
            );
            assert_eq!(
                entry.extended,
                fixed["extended"].as_bool().unwrap(),
                "{} extended",
                entry.code
            );
        }
        for name in codes.keys() {
            assert!(
                SCAN_CODES.iter().any(|entry| entry.code == name.as_str()),
                "{name} missing from table"
            );
        }
    }

    #[test]
    fn table_vk_is_zero_exactly_where_input_js_sends_none() {
        let zero: Vec<&str> = SCAN_CODES
            .iter()
            .filter(|entry| entry.vk == 0)
            .map(|entry| entry.code)
            .collect();
        assert_eq!(zero, BROWSER_SENDS_NO_VK);
    }

    #[test]
    fn table_vk_follows_the_browser_code_to_vk_rules() {
        for entry in SCAN_CODES {
            let code = entry.code;
            let expected = if let Some(letter) = code.strip_prefix("Key").filter(|s| s.len() == 1) {
                Some(0x41 + u16::from(letter.as_bytes()[0] - b'A'))
            } else if let Some(digit) = code.strip_prefix("Digit") {
                Some(0x30 + u16::from(digit.as_bytes()[0] - b'0'))
            } else if let Some(digit) = code.strip_prefix("Numpad").filter(|s| s.len() == 1) {
                Some(0x60 + u16::from(digit.as_bytes()[0] - b'0'))
            } else if let Some(n) = code.strip_prefix('F').and_then(|n| n.parse::<u16>().ok()) {
                Some(0x6F + n)
            } else {
                None
            };
            if let Some(expected) = expected {
                assert_eq!(entry.vk, expected, "{code}");
            }
        }
    }

    #[test]
    fn table_vk_spot_checks_match_input_js() {
        let spot = [
            ("KeyQ", 0x51),
            ("Enter", 0x0D),
            ("NumpadEnter", 0x0D),
            ("ShiftLeft", 0xA0),
            ("ShiftRight", 0xA1),
            ("ControlLeft", 0xA2),
            ("ControlRight", 0xA3),
            ("AltLeft", 0xA4),
            ("AltRight", 0xA5),
            ("MetaLeft", 0x5B),
            ("MetaRight", 0x5C),
            ("ContextMenu", 0x5D),
            ("PrintScreen", 0x2C),
            ("NumpadDivide", 0x6F),
            ("NumpadMultiply", 0x6A),
            ("NumpadDecimal", 0x6E),
            ("NumpadSubtract", 0x6D),
            ("NumpadAdd", 0x6B),
            ("Insert", 0x2D),
            ("Delete", 0x2E),
            ("Home", 0x24),
            ("End", 0x23),
            ("PageUp", 0x21),
            ("ArrowLeft", 0x25),
            ("ArrowUp", 0x26),
            ("ArrowDown", 0x28),
            ("Semicolon", 0xBA),
            ("Backquote", 0xC0),
            ("IntlBackslash", 0xE2),
            ("ScrollLock", 0x91),
            ("Space", 0x20),
            ("CapsLock", 0x14),
            ("Escape", 0x1B),
            ("Backspace", 0x08),
            ("Tab", 0x09),
            ("F13", 0x7C),
            ("F24", 0x87),
        ];
        // `key` panics on a missing code, so a dropped row fails here instead of passing silently.
        for (code, vk) in spot {
            assert_eq!(key(code).vk, vk, "{code}");
        }
    }

    #[test]
    fn every_table_key_maps_to_its_own_scan_flag_and_browser_vk() {
        for entry in SCAN_CODES {
            for layout_vk in [0, 0x41, 0xFF] {
                assert_eq!(
                    map_from_layout(*entry, layout_vk),
                    Some(PhysicalKey {
                        scan: u16::from(entry.scan),
                        extended: entry.extended,
                        vk: entry.vk,
                    }),
                    "{} with layout VK {layout_vk:#04x}",
                    entry.code,
                );
            }
        }
    }

    #[test]
    fn must_be_extended_keys_are_e0_in_the_table() {
        let fixture = fixture();
        for name in fixture["mustBeExtended"].as_array().unwrap() {
            let name = name.as_str().unwrap();
            assert!(key(name).extended, "{name} must be E0");
        }
    }

    #[test]
    fn vk_only_keys_from_fixture_map_to_vk_fallback() {
        let fixture = fixture();
        for (name, vk) in fixture["vkOnly"].as_object().unwrap() {
            let vk = vk.as_u64().unwrap() as u16;
            // Hook scan for Pause and NumLock is 0x45; the native VK wins over it.
            assert_eq!(
                physical_key(vk, 0x45, false),
                Some(PhysicalKey {
                    scan: 0,
                    extended: false,
                    vk
                }),
                "{name}"
            );
            assert_eq!(
                physical_key(vk, 0, false),
                Some(PhysicalKey {
                    scan: 0,
                    extended: false,
                    vk
                }),
                "{name} no scan"
            );
        }
    }

    #[test]
    fn locale_layout_vk_is_replaced_by_the_browser_code_vk() {
        // AZERTY: the key at the QWERTY-Q position reports layout VK 'A'; the browser sends code KeyQ.
        assert_eq!(
            physical_key(0x41, 0x10, false),
            Some(PhysicalKey {
                scan: 0x10,
                extended: false,
                vk: 0x51
            }),
        );
        // Dvorak: the key at the QWERTY-S position reports layout VK 'O'; the browser sends KeyS.
        assert_eq!(
            physical_key(0x4F, 0x1F, false),
            Some(PhysicalKey {
                scan: 0x1F,
                extended: false,
                vk: 0x53
            }),
        );
        // A digit VK from a layout does not leak onto a letter position.
        assert_eq!(
            physical_key(0x31, 0x1E, false),
            Some(PhysicalKey {
                scan: 0x1E,
                extended: false,
                vk: 0x41
            }),
        );
        // A key that the browser sends no VK for keeps VK 0 even if the layout reports one.
        assert_eq!(
            physical_key(0x3A, 0x7E, false),
            Some(PhysicalKey {
                scan: 0x7E,
                extended: false,
                vk: 0
            }),
        );
    }

    #[test]
    fn numpad_enter_and_enter_differ_only_by_e0() {
        assert_eq!(
            physical_key(0x0D, 0x1C, false),
            Some(PhysicalKey {
                scan: 0x1C,
                extended: false,
                vk: 0x0D
            })
        );
        assert_eq!(
            physical_key(0x0D, 0x1C, true),
            Some(PhysicalKey {
                scan: 0x1C,
                extended: true,
                vk: 0x0D
            })
        );
    }

    #[test]
    fn numpad_divide_and_slash_share_a_scan_code_and_differ_by_e0() {
        assert_eq!(
            physical_key(0xBF, 0x35, false),
            Some(PhysicalKey {
                scan: 0x35,
                extended: false,
                vk: 0xBF
            })
        );
        assert_eq!(
            physical_key(0x6F, 0x35, true),
            Some(PhysicalKey {
                scan: 0x35,
                extended: true,
                vk: 0x6F
            })
        );
    }

    #[test]
    fn left_and_right_modifiers_keep_their_side_with_browser_vks() {
        assert_eq!(
            physical_key(0x10, 0x2A, false),
            Some(PhysicalKey {
                scan: 0x2A,
                extended: false,
                vk: 0xA0
            }),
            "left shift",
        );
        assert_eq!(
            physical_key(0x10, 0x36, false),
            Some(PhysicalKey {
                scan: 0x36,
                extended: false,
                vk: 0xA1
            }),
            "right shift",
        );
        assert_eq!(
            physical_key(0x11, 0x1D, false),
            Some(PhysicalKey {
                scan: 0x1D,
                extended: false,
                vk: 0xA2
            }),
            "left control",
        );
        assert_eq!(
            physical_key(0x11, 0x1D, true),
            Some(PhysicalKey {
                scan: 0x1D,
                extended: true,
                vk: 0xA3
            }),
            "right control from a generic VK plus E0",
        );
        assert_eq!(
            physical_key(0x12, 0x38, false),
            Some(PhysicalKey {
                scan: 0x38,
                extended: false,
                vk: 0xA4
            }),
            "left alt",
        );
        assert_eq!(
            physical_key(0x12, 0x38, true),
            Some(PhysicalKey {
                scan: 0x38,
                extended: true,
                vk: 0xA5
            }),
            "right alt from a generic VK plus E0",
        );
    }

    #[test]
    fn right_control_and_right_alt_vks_force_e0_when_the_flag_is_missing() {
        assert_eq!(
            physical_key(0xA3, 0x1D, false),
            Some(PhysicalKey {
                scan: 0x1D,
                extended: true,
                vk: 0xA3
            }),
        );
        assert_eq!(
            physical_key(0xA5, 0x38, false),
            Some(PhysicalKey {
                scan: 0x38,
                extended: true,
                vk: 0xA5
            }),
        );
        // Left VKs never imply E0.
        assert_eq!(
            physical_key(0xA2, 0x1D, false),
            Some(PhysicalKey {
                scan: 0x1D,
                extended: false,
                vk: 0xA2
            }),
        );
        assert_eq!(
            physical_key(0xA4, 0x38, false),
            Some(PhysicalKey {
                scan: 0x38,
                extended: false,
                vk: 0xA4
            }),
        );
    }

    #[test]
    fn right_control_vk_forces_e0_even_on_an_unknown_raw_scan() {
        // Scan 0x54 is not a table key, so it keeps its scan and the native VK.
        assert_eq!(
            physical_key(0xA3, 0x54, false),
            Some(PhysicalKey {
                scan: 0x54,
                extended: true,
                vk: 0xA3
            }),
        );
    }

    #[test]
    fn win_keys_are_extended_even_when_the_hook_drops_the_flag() {
        assert_eq!(
            physical_key(0x5B, 0x5B, true),
            Some(PhysicalKey {
                scan: 0x5B,
                extended: true,
                vk: 0x5B
            })
        );
        assert_eq!(
            physical_key(0x5B, 0x5B, false),
            Some(PhysicalKey {
                scan: 0x5B,
                extended: true,
                vk: 0x5B
            })
        );
        assert_eq!(
            physical_key(0x5C, 0x5C, false),
            Some(PhysicalKey {
                scan: 0x5C,
                extended: true,
                vk: 0x5C
            })
        );
    }

    #[test]
    fn unknown_raw_scans_are_preserved_with_the_native_vk() {
        assert_eq!(
            physical_key(0x2C, 0x54, false),
            Some(PhysicalKey {
                scan: 0x54,
                extended: false,
                vk: 0x2C
            }),
        );
        // 0x45 is only special when the VK says Pause or NumLock.
        assert_eq!(
            physical_key(0x41, 0x45, false),
            Some(PhysicalKey {
                scan: 0x45,
                extended: false,
                vk: 0x41
            }),
        );
    }

    #[test]
    fn same_scan_with_and_without_e0_keeps_the_hook_flag_when_both_exist() {
        assert_eq!(
            physical_key(0x2D, 0x52, false),
            Some(PhysicalKey {
                scan: 0x52,
                extended: false,
                vk: 0x60
            }),
            "Numpad0 sends VK_NUMPAD0",
        );
        assert_eq!(
            physical_key(0x2D, 0x52, true),
            Some(PhysicalKey {
                scan: 0x52,
                extended: true,
                vk: 0x2D
            }),
            "Insert",
        );
        assert_eq!(
            physical_key(0x2C, 0x37, true),
            Some(PhysicalKey {
                scan: 0x37,
                extended: true,
                vk: 0x2C
            }),
            "PrintScreen",
        );
        assert_eq!(
            physical_key(0x6A, 0x37, false),
            Some(PhysicalKey {
                scan: 0x37,
                extended: false,
                vk: 0x6A
            }),
            "NumpadMultiply",
        );
    }

    #[test]
    fn pause_and_numlock_keep_their_native_vk() {
        assert_eq!(
            physical_key(0x13, 0x45, false),
            Some(PhysicalKey {
                scan: 0,
                extended: false,
                vk: 0x13
            })
        );
        assert_eq!(
            physical_key(0x90, 0x45, false),
            Some(PhysicalKey {
                scan: 0,
                extended: false,
                vk: 0x90
            })
        );
        assert_eq!(
            physical_key(0x90, 0x45, true),
            Some(PhysicalKey {
                scan: 0,
                extended: false,
                vk: 0x90
            })
        );
    }

    #[test]
    fn scan_zero_or_oversized_uses_native_vk_and_unusable_events_are_rejected() {
        assert_eq!(
            physical_key(0x5D, 0, false),
            Some(PhysicalKey {
                scan: 0,
                extended: false,
                vk: 0x5D
            })
        );
        assert_eq!(
            physical_key(0x41, 0x0100, false),
            Some(PhysicalKey {
                scan: 0,
                extended: false,
                vk: 0x41
            })
        );
        assert_eq!(physical_key(0, 0, false), None);
        assert_eq!(physical_key(0x100, 0, false), None);
        assert_eq!(physical_key(0x100, 0x0100, false), None);
    }
}
