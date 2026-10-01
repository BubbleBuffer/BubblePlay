//! Text clipboard access for the interactive user session.
//!
//! Only the input proxy (interactive user token, default desktop) and the
//! unprivileged local host use this module. Every access re-checks that the
//! current input desktop is the default desktop, so clipboard text is never
//! read or written while Winlogon/UAC (the secure desktop) is active.
//!
//! Wire formats (see `fixtures/input-contract.json`):
//! - browser -> host: binary input packet type 12, chunked UTF-8 (see
//!   `crate::input`), reassembled by `InputSink`.
//! - host -> browser: JSON `{"type":"clipboard","protocol":1,"id","index","count","text"}`
//!   chunks of at most `HOST_CHUNK_BYTES` UTF-8 bytes, split on char boundaries.

use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

use crate::desktop_session::{DesktopKind, InputDesktop};

/// Maximum clipboard text (UTF-8 bytes) carried in either direction.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
/// Host -> browser chunk size (UTF-8 bytes). JSON escaping can grow a chunk
/// by at most 6x, which keeps every signaling message far below relay limits.
pub const HOST_CHUNK_BYTES: usize = 32 * 1024;
const CF_UNICODETEXT: u32 = 13;
const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(5);

/// Split `text` into chunks of at most `max_bytes` UTF-8 bytes without
/// breaking a code point. Always returns at least one chunk.
pub fn chunk_text(text: &str, max_bytes: usize) -> Vec<&str> {
    let max_bytes = max_bytes.max(4);
    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.len() > max_bytes {
        let mut end = max_bytes;
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&rest[..end]);
        rest = &rest[end..];
    }
    chunks.push(rest);
    chunks
}

/// Windows text controls expect CRLF; browsers on other platforms paste LF.
pub fn normalize_newlines_for_windows(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 32);
    let mut previous = '\0';
    for character in text.chars() {
        if character == '\n' && previous != '\r' {
            out.push('\r');
        }
        out.push(character);
        previous = character;
    }
    out
}

pub fn input_desktop_is_default() -> bool {
    InputDesktop::open().is_ok_and(|desktop| desktop.kind() == DesktopKind::Default)
}

pub fn sequence_number() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

struct OpenClipboardGuard;

impl OpenClipboardGuard {
    fn open() -> Result<Self> {
        let mut last_error = None;
        for attempt in 0..OPEN_ATTEMPTS {
            // A null owner avoids a window whose thread would have to pump
            // WM_DESTROYCLIPBOARD for other applications.
            match unsafe { OpenClipboard(None) } {
                Ok(()) => return Ok(Self),
                Err(error) => last_error = Some(error),
            }
            if attempt + 1 < OPEN_ATTEMPTS {
                thread::sleep(OPEN_RETRY_DELAY);
            }
        }
        Err(last_error.expect("at least one attempt")).context("OpenClipboard")
    }
}

impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

/// Replace the clipboard with `text` and return the resulting sequence number.
pub fn set_text(text: &str) -> Result<u32> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        bail!("clipboard text exceeds {MAX_CLIPBOARD_BYTES} bytes");
    }
    if !input_desktop_is_default() {
        bail!("clipboard is unavailable outside the default desktop");
    }
    let wide: Vec<u16> = normalize_newlines_for_windows(text)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let _guard = OpenClipboardGuard::open()?;
    unsafe { EmptyClipboard() }.context("EmptyClipboard")?;
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) }.context("GlobalAlloc")?;
    let target = unsafe { GlobalLock(memory) } as *mut u16;
    if target.is_null() {
        let _ = unsafe { GlobalFree(Some(memory)) };
        bail!("GlobalLock failed for clipboard text");
    }
    unsafe {
        std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
        let _ = GlobalUnlock(memory);
    }
    if let Err(error) = unsafe { SetClipboardData(CF_UNICODETEXT, Some(HANDLE(memory.0))) } {
        let _ = unsafe { GlobalFree(Some(memory)) };
        return Err(error).context("SetClipboardData");
    }
    Ok(sequence_number())
}

const CF_HDROP: u32 = 15;

/// Build the CF_HDROP payload: a DROPFILES header (20 bytes, wide names)
/// followed by NUL-terminated UTF-16 paths and a final NUL.
pub fn hdrop_bytes(paths: &[std::path::PathBuf]) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    const HEADER: u32 = 20;
    let mut out = Vec::new();
    out.extend_from_slice(&HEADER.to_le_bytes()); // pFiles
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    out.extend_from_slice(&0i32.to_le_bytes()); // fNC
    out.extend_from_slice(&1i32.to_le_bytes()); // fWide
    for path in paths {
        for unit in path.as_os_str().encode_wide() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// Put `paths` on the clipboard as a file list (what Explorer's Ctrl+C
/// puts there) and return the resulting sequence number.
pub fn set_files(paths: &[std::path::PathBuf]) -> Result<u32> {
    if paths.is_empty() {
        bail!("no files to place on the clipboard");
    }
    if !input_desktop_is_default() {
        bail!("clipboard is unavailable outside the default desktop");
    }
    let bytes = hdrop_bytes(paths);
    let _guard = OpenClipboardGuard::open()?;
    unsafe { EmptyClipboard() }.context("EmptyClipboard")?;
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }.context("GlobalAlloc")?;
    let target = unsafe { GlobalLock(memory) } as *mut u8;
    if target.is_null() {
        let _ = unsafe { GlobalFree(Some(memory)) };
        bail!("GlobalLock failed for clipboard file list");
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
        let _ = GlobalUnlock(memory);
    }
    if let Err(error) = unsafe { SetClipboardData(CF_HDROP, Some(HANDLE(memory.0))) } {
        let _ = unsafe { GlobalFree(Some(memory)) };
        return Err(error).context("SetClipboardData(CF_HDROP)");
    }
    Ok(sequence_number())
}

/// The clipboard's file list (CF_HDROP), or `None` when it holds no files.
pub fn read_files() -> Result<Option<Vec<std::path::PathBuf>>> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
    if !input_desktop_is_default() {
        bail!("clipboard is unavailable outside the default desktop");
    }
    let _guard = OpenClipboardGuard::open()?;
    let Ok(handle) = (unsafe { GetClipboardData(CF_HDROP) }) else {
        return Ok(None);
    };
    let drop = HDROP(handle.0);
    let count = unsafe { DragQueryFileW(drop, u32::MAX, None) };
    let mut paths = Vec::new();
    for index in 0..count {
        let length = unsafe { DragQueryFileW(drop, index, None) } as usize;
        let mut buffer = vec![0u16; length + 1];
        let written = unsafe { DragQueryFileW(drop, index, Some(&mut buffer)) } as usize;
        buffer.truncate(written);
        paths.push(std::path::PathBuf::from(std::ffi::OsString::from_wide(
            &buffer,
        )));
    }
    Ok(Some(paths))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardText {
    Text(String),
    NoText,
    TooLarge,
}

pub fn read_text() -> Result<ClipboardText> {
    if !input_desktop_is_default() {
        bail!("clipboard is unavailable outside the default desktop");
    }
    let _guard = OpenClipboardGuard::open()?;
    let Ok(handle) = (unsafe { GetClipboardData(CF_UNICODETEXT) }) else {
        return Ok(ClipboardText::NoText);
    };
    let memory = HGLOBAL(handle.0);
    let units = unsafe { GlobalSize(memory) } / 2;
    let source = unsafe { GlobalLock(memory) } as *const u16;
    if source.is_null() {
        return Ok(ClipboardText::NoText);
    }
    let wide = unsafe { std::slice::from_raw_parts(source, units) };
    let length = wide.iter().position(|unit| *unit == 0).unwrap_or(units);
    // Every UTF-16 unit is at least one UTF-8 byte: reject before converting.
    let result = if length > MAX_CLIPBOARD_BYTES {
        ClipboardText::TooLarge
    } else {
        let text = String::from_utf16_lossy(&wide[..length]);
        if text.len() > MAX_CLIPBOARD_BYTES {
            ClipboardText::TooLarge
        } else if text.is_empty() {
            ClipboardText::NoText
        } else {
            ClipboardText::Text(text)
        }
    };
    let _ = unsafe { GlobalUnlock(memory) };
    Ok(result)
}

/// Detects clipboard changes by sequence number (cheap; no clipboard open).
#[derive(Debug, Default)]
pub struct ClipboardWatcher {
    last_sequence: Option<u32>,
}

impl ClipboardWatcher {
    /// Record a change this process made so it is not echoed back.
    pub fn note_own_change(&mut self, sequence: u32) {
        self.last_sequence = Some(sequence);
    }

    /// Decide whether the observed sequence is a new change to report.
    /// `reset` (a new browser session) only establishes the baseline.
    pub fn observe(&mut self, sequence: u32, reset: bool) -> bool {
        let previous = self.last_sequence.replace(sequence);
        // Sequence 0 means the window station denies clipboard access.
        !reset && sequence != 0 && previous.is_some_and(|previous| previous != sequence)
    }

    /// Return new clipboard text since the last poll, if any.
    pub fn poll(&mut self, reset: bool) -> Option<String> {
        if !self.observe(sequence_number(), reset) {
            return None;
        }
        match read_text() {
            Ok(ClipboardText::Text(text)) => Some(text),
            Ok(ClipboardText::TooLarge) => {
                tracing::warn!("host clipboard text exceeds 1 MiB; not synchronized");
                None
            }
            Ok(ClipboardText::NoText) => None,
            Err(error) => {
                tracing::debug!(?error, "host clipboard read failed");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_respect_byte_limit_and_code_points() {
        let text = "aé€😀".repeat(1000);
        let chunks = chunk_text(&text, 7);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.len() <= 7 && !chunk.is_empty()));
        assert_eq!(chunks.concat(), text);
        assert_eq!(chunk_text("", 32), vec![""]);
        assert_eq!(chunk_text("abc", 32), vec!["abc"]);
        let full = "x".repeat(MAX_CLIPBOARD_BYTES);
        assert_eq!(chunk_text(&full, HOST_CHUNK_BYTES).len(), 32);
    }

    #[test]
    fn newlines_are_normalized_without_doubling_crlf() {
        assert_eq!(
            normalize_newlines_for_windows("a\nb\r\nc\n"),
            "a\r\nb\r\nc\r\n"
        );
        assert_eq!(normalize_newlines_for_windows("plain"), "plain");
    }

    #[test]
    fn hdrop_payload_is_a_wide_double_nul_list() {
        let bytes = hdrop_bytes(&[
            std::path::PathBuf::from(r"C:\a.txt"),
            std::path::PathBuf::from("b"),
        ]);
        assert_eq!(&bytes[0..4], &20u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &1i32.to_le_bytes());
        let units: Vec<u16> = bytes[20..]
            .chunks(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let expected: Vec<u16> = "C:\\a.txt\0b\0\0".encode_utf16().collect();
        assert_eq!(units, expected);
    }

    #[test]
    fn watcher_reports_only_foreign_changes_after_a_baseline() {
        let mut watcher = ClipboardWatcher::default();
        assert!(
            !watcher.observe(5, false),
            "first observation is a baseline"
        );
        assert!(!watcher.observe(5, false));
        assert!(watcher.observe(6, false));
        watcher.note_own_change(9);
        assert!(!watcher.observe(9, false), "own writes are not echoed");
        assert!(!watcher.observe(10, true), "reset re-baselines");
        assert!(!watcher.observe(0, false), "no clipboard access");
    }
}
