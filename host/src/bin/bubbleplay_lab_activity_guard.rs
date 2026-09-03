//! User-session physical-input guard for long Stream Lab campaigns.
//!
//! Low-level Windows hooks expose the injected flags that `GetLastInputInfo`
//! does not.  The hook callbacks only update atomics; a separate reporter
//! writes bounded NDJSON so a slow parent cannot stall the desktop input path.

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT,
    MSG, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL,
};

const LLKHF_LOWER_IL_INJECTED: u32 = 0x02;
const LLKHF_INJECTED: u32 = 0x10;
const LLMHF_INJECTED: u32 = 0x01;
const LLMHF_LOWER_IL_INJECTED: u32 = 0x02;

static PHYSICAL_ACTIVITY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn physical_keyboard_event(flags: u32) -> bool {
    flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) == 0
}

fn physical_mouse_event(flags: u32) -> bool {
    flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) == 0
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if physical_keyboard_event(event.flags.0) {
            PHYSICAL_ACTIVITY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let event = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if physical_mouse_event(event.flags) {
            PHYSICAL_ACTIVITY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

struct Hooks {
    keyboard: HHOOK,
    mouse: HHOOK,
}

impl Drop for Hooks {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.keyboard);
            let _ = UnhookWindowsHookEx(self.mouse);
        }
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn spawn_reporter() {
    thread::spawn(|| {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        let _ = writeln!(
            output,
            "{{\"event\":\"ready\",\"pid\":{}}}",
            std::process::id()
        );
        let _ = output.flush();
        let mut observed = 0;
        loop {
            let sequence = PHYSICAL_ACTIVITY_SEQUENCE.load(Ordering::Relaxed);
            if sequence != observed {
                observed = sequence;
                if writeln!(
                    output,
                    "{{\"event\":\"physical-input\",\"sequence\":{sequence},\"atMs\":{}}}",
                    unix_time_ms()
                )
                .and_then(|_| output.flush())
                .is_err()
                {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
}

fn main() -> windows::core::Result<()> {
    let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0)? };
    let mouse = match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) } {
        Ok(mouse) => mouse,
        Err(error) => {
            unsafe {
                let _ = UnhookWindowsHookEx(keyboard);
            }
            return Err(error);
        }
    };
    let _hooks = Hooks { keyboard, mouse };
    spawn_reporter();
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{physical_keyboard_event, physical_mouse_event};

    #[test]
    fn injected_input_is_not_reported_as_user_activity() {
        assert!(physical_keyboard_event(0));
        assert!(!physical_keyboard_event(0x10));
        assert!(!physical_keyboard_event(0x02));
        assert!(physical_mouse_event(0));
        assert!(!physical_mouse_event(0x01));
        assert!(!physical_mouse_event(0x02));
    }
}
