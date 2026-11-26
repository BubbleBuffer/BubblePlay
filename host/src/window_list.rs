//! App focus mode: the window list (`window-list-v1`, `docs/architecture/app-focus-mode.md`).
//!
//! The pure parts live here: which top-level windows a user can switch to, the rectangle a
//! client crops to (the app plus the visible windows it owns, such as menus), the wire messages
//! and the per-session sender state. Sampling windows and focusing one is
//! `window_list_capture.rs`; sending is the host main loop. Titles are sent to the viewer, who
//! sees the screen anyway, but are never logged.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};

use crate::capture::MonitorInfo;
use crate::follow_window::{is_ignored_class, is_ignored_exe, Rect};

/// Capability string, advertised by both sides in the ctl hello.
pub const CAPABILITY: &str = "window-list-v1";
/// Wire version carried in every message.
pub const WIRE_VERSION: u64 = 1;
pub const LIST_TYPE: &str = "window-list";
pub const FOCUS_TYPE: &str = "window-focus";
pub const FOCUS_FAILED_TYPE: &str = "window-focus-failed";
pub const ICON_TYPE: &str = "window-icon";
pub const START_TYPE: &str = "window-list-start";
pub const STOP_TYPE: &str = "window-list-stop";

pub const MAX_WINDOWS: usize = 64;
pub const MAX_TITLE_BYTES: usize = 256;
pub const MAX_PROCESS_BYTES: usize = 128;
pub const ICON_SIZE: u32 = 32;
pub const MAX_ICON_BYTES: usize = 8 * 1024;
/// Lists go out at most this often, and only when something changed.
pub const MIN_SEND_INTERVAL: Duration = Duration::from_millis(250);
/// The sampler's pace while focus mode is on.
pub const SAMPLE_INTERVAL: Duration = MIN_SEND_INTERVAL;
pub const MAX_LIST_MESSAGE_BYTES: usize = 64 * 1024;
/// Owned popups count when they touch the app's rectangle or come this close to it.
pub const POPUP_REACH_PX: i32 = 8;
/// Icons kept per session.
const ICON_CACHE_ENTRIES: usize = 128;

/// One top-level window as the sampler saw it. Samples are in z-order, topmost first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawWindow {
    /// The window handle's value; handles fit in 32 bits on 64-bit Windows.
    pub id: u32,
    /// `GW_OWNER`, 0 when unowned.
    pub owner: u32,
    pub thread: u32,
    pub pid: u32,
    pub class: String,
    /// File name of the process executable.
    pub exe: String,
    pub title: String,
    pub visible: bool,
    pub cloaked: bool,
    pub minimized: bool,
    pub tool_window: bool,
    pub app_window: bool,
    pub no_activate: bool,
    /// Visible frame in virtual-desktop pixels; for a minimized window, its restored position.
    pub rect: Rect,
    /// Hash of the window's icon, when one was read.
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Foreground,
    Normal,
    Minimized,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Foreground => "foreground",
            State::Normal => "normal",
            State::Minimized => "minimized",
        }
    }
}

/// One entry of the list sent to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: u32,
    pub title: String,
    pub process: String,
    pub icon: Option<String>,
    pub rect: Rect,
    pub monitor: Option<String>,
    pub state: State,
}

/// Why a `window-focus` failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusFailure {
    /// The window is gone, or was not in the list the client saw.
    Gone,
    /// The window belongs to an elevated process and the host could not raise it.
    Elevated,
    /// Windows did not let the host bring the window to the foreground.
    Refused,
}

impl FocusFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            FocusFailure::Gone => "gone",
            FocusFailure::Elevated => "elevated",
            FocusFailure::Refused => "refused",
        }
    }
}

/// A control from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientControl {
    /// The switcher opened: send lists from now on.
    Start,
    /// Focus mode ended: stop sending lists.
    Stop,
    Focus {
        id: u32,
    },
    Icon {
        hash: String,
    },
}

/// The windows a user can switch to: what Alt+Tab would show, minus the shell and the windows
/// follow active window ignores.
pub fn is_switchable(w: &RawWindow) -> bool {
    let app_like = w.app_window || (w.owner == 0 && !w.tool_window && !w.no_activate);
    w.visible
        && !w.cloaked
        && app_like
        && !w.title.trim().is_empty()
        && !is_ignored_class(&w.class)
        && !is_ignored_exe(&w.exe)
}

fn near(a: Rect, b: Rect, reach: i32) -> bool {
    a.left <= b.right + reach
        && b.left <= a.right + reach
        && a.top <= b.bottom + reach
        && b.top <= a.bottom + reach
}

fn union(a: Rect, b: Rect) -> Rect {
    Rect {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

fn owned_by(window: &RawWindow, app: u32, all: &HashMap<u32, &RawWindow>) -> bool {
    let mut owner = window.owner;
    for _ in 0..8 {
        if owner == 0 {
            return false;
        }
        if owner == app {
            return true;
        }
        owner = all.get(&owner).map_or(0, |w| w.owner);
    }
    false
}

/// The app's rectangle widened by the visible windows it owns that sit above it and touch it
/// (directly or through another such window): menus, submenus, dialogs, tooltips. Menus belong
/// to the app's UI thread rather than its owner chain, so the same thread counts as owning,
/// except for the app's other switchable windows (browsers run several on one thread).
pub fn crop_rect(index: usize, windows: &[RawWindow]) -> Rect {
    let app = &windows[index];
    let by_id: HashMap<u32, &RawWindow> = windows.iter().map(|w| (w.id, w)).collect();
    let above: Vec<&RawWindow> = windows[..index]
        .iter()
        .filter(|w| {
            w.visible
                && !w.cloaked
                && !w.minimized
                && w.pid == app.pid
                && (owned_by(w, app.id, &by_id) || (w.thread == app.thread && !is_switchable(w)))
        })
        .collect();
    let mut rect = app.rect;
    let mut taken = vec![false; above.len()];
    loop {
        let mut grew = false;
        for (i, w) in above.iter().enumerate() {
            if !taken[i] && near(w.rect, rect, POPUP_REACH_PX) {
                taken[i] = true;
                rect = union(rect, w.rect);
                grew = true;
            }
        }
        if !grew {
            return rect;
        }
    }
}

fn overlap_area(a: Rect, b: Rect) -> i64 {
    let w = i64::from(a.right.min(b.right) - a.left.max(b.left)).max(0);
    let h = i64::from(a.bottom.min(b.bottom) - a.top.max(b.top)).max(0);
    w * h
}

/// The monitor holding most of `rect`, if any part of it is on a monitor.
pub fn monitor_for(rect: Rect, monitors: &[MonitorInfo]) -> Option<String> {
    monitors
        .iter()
        .map(|m| (overlap_area(rect, Rect::of(m)), m))
        .filter(|(area, _)| *area > 0)
        .max_by_key(|(area, _)| *area)
        .map(|(_, m)| m.selector.clone())
}

/// `text` cut to at most `max` bytes on a character boundary.
pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The list for the client, in z-order, at most [`MAX_WINDOWS`] entries.
pub fn build(windows: &[RawWindow], foreground: u32, monitors: &[MonitorInfo]) -> Vec<Entry> {
    windows
        .iter()
        .enumerate()
        .filter(|(_, w)| is_switchable(w))
        .take(MAX_WINDOWS)
        .map(|(index, w)| {
            let rect = if w.minimized {
                w.rect
            } else {
                crop_rect(index, windows)
            };
            Entry {
                id: w.id,
                title: truncate(w.title.trim(), MAX_TITLE_BYTES),
                process: truncate(&w.exe, MAX_PROCESS_BYTES),
                icon: w.icon.clone(),
                rect,
                monitor: monitor_for(rect, monitors),
                state: if w.minimized {
                    State::Minimized
                } else if w.id == foreground {
                    State::Foreground
                } else {
                    State::Normal
                },
            }
        })
        .collect()
}

fn entry_json(e: &Entry) -> Value {
    let mut v = json!({
        "id": e.id,
        "title": e.title,
        "process": e.process,
        "rect": [e.rect.left, e.rect.top, e.rect.right - e.rect.left, e.rect.bottom - e.rect.top],
        "state": e.state.as_str(),
    });
    if let Some(icon) = &e.icon {
        v["icon"] = json!(icon);
    }
    if let Some(monitor) = &e.monitor {
        v["monitor"] = json!(monitor);
    }
    v
}

/// The `window-list` message, with trailing entries dropped until it fits
/// [`MAX_LIST_MESSAGE_BYTES`].
pub fn list_message(seq: u64, entries: &[Entry]) -> String {
    let mut windows: Vec<Value> = entries.iter().map(entry_json).collect();
    loop {
        let text = json!({"type": LIST_TYPE, "v": WIRE_VERSION, "seq": seq, "windows": windows})
            .to_string();
        if text.len() <= MAX_LIST_MESSAGE_BYTES || windows.is_empty() {
            return text;
        }
        windows.pop();
    }
}

pub fn focus_failed_message(id: u32, reason: FocusFailure) -> String {
    json!({"type": FOCUS_FAILED_TYPE, "v": WIRE_VERSION, "id": id, "reason": reason.as_str()})
        .to_string()
}

pub fn icon_message(hash: &str, png: &[u8]) -> String {
    let png = base64::engine::general_purpose::STANDARD.encode(png);
    json!({"type": ICON_TYPE, "v": WIRE_VERSION, "icon": hash, "png": png}).to_string()
}

/// An icon drawn twice, on black and on white (both BGRA, the same size), as RGBA with its
/// transparency: what changes between the two backgrounds shows through. Works for icons with an
/// alpha channel and for old masked icons alike.
pub fn unblend(black: &[u8], white: &[u8]) -> Vec<u8> {
    black
        .chunks_exact(4)
        .zip(white.chunks_exact(4))
        .flat_map(|(b, w)| {
            let seen = (0..3).map(|c| w[c].saturating_sub(b[c])).max().unwrap_or(0);
            let alpha = 255 - seen;
            if alpha == 0 {
                return [0, 0, 0, 0];
            }
            let channel = |c: usize| (u32::from(b[c]) * 255 / u32::from(alpha)).min(255) as u8;
            [channel(2), channel(1), channel(0), alpha]
        })
        .collect()
}

/// FNV-1a 64 of the PNG bytes, as 16 lowercase hex digits.
pub fn icon_hash(png: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in png {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 16 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn parse_control(kind: &str, value: &Value) -> Option<ClientControl> {
    if value
        .get("v")
        .is_some_and(|v| v.as_u64() != Some(WIRE_VERSION))
    {
        return None;
    }
    match kind {
        FOCUS_TYPE => {
            let id = u32::try_from(value.get("id")?.as_u64()?).ok()?;
            Some(ClientControl::Focus { id })
        }
        ICON_TYPE => {
            let hash = value.get("icon")?.as_str()?;
            valid_hash(hash).then(|| ClientControl::Icon {
                hash: hash.to_owned(),
            })
        }
        START_TYPE => Some(ClientControl::Start),
        _ => Some(ClientControl::Stop),
    }
}

/// A window-list control from the client, or `None` for any other message. Malformed controls
/// are dropped here with a debug line, so the caller only routes.
pub fn parse_client(text: &str) -> Option<ClientControl> {
    if !text.contains("window-") {
        return None;
    }
    let value: Value = serde_json::from_str(text).ok()?;
    let kind = value.get("type")?.as_str()?;
    if ![START_TYPE, STOP_TYPE, FOCUS_TYPE, ICON_TYPE].contains(&kind) {
        return None;
    }
    let parsed = parse_control(kind, &value);
    if parsed.is_none() {
        tracing::debug!(kind, "dropping a malformed window-list control");
    }
    parsed
}

/// Per-session sender: sends a list only in focus mode, only when it changed, at most every
/// [`MIN_SEND_INTERVAL`], and remembers which ids the client was shown.
#[derive(Debug, Default)]
pub struct Sender {
    active: bool,
    seq: u64,
    last: Option<Vec<Entry>>,
    last_sent: Option<Instant>,
    listed: HashSet<u32>,
}

impl Sender {
    /// Focus mode on: the next list goes out even if unchanged.
    pub fn start(&mut self) {
        self.active = true;
        self.last = None;
    }

    pub fn stop(&mut self) {
        self.active = false;
        self.last = None;
        self.listed.clear();
    }

    pub fn active(&self) -> bool {
        self.active
    }

    /// Whether `id` was in a list the client was sent.
    pub fn listed(&self, id: u32) -> bool {
        self.listed.contains(&id)
    }

    /// The message to send now, if any.
    pub fn next(&mut self, now: Instant, entries: Vec<Entry>) -> Option<String> {
        if !self.active || self.last.as_ref() == Some(&entries) {
            return None;
        }
        if self
            .last_sent
            .is_some_and(|at| now.duration_since(at) < MIN_SEND_INTERVAL)
        {
            return None;
        }
        self.seq += 1;
        self.last_sent = Some(now);
        self.listed.extend(entries.iter().map(|e| e.id));
        let text = list_message(self.seq, &entries);
        self.last = Some(entries);
        Some(text)
    }
}

/// Icons read this session, by hash.
#[derive(Debug, Default)]
pub struct IconCache {
    icons: HashMap<String, Vec<u8>>,
}

impl IconCache {
    /// Store a PNG and return its hash; `None` when it is too large or the cache is full.
    pub fn insert(&mut self, png: Vec<u8>) -> Option<String> {
        if png.is_empty() || png.len() > MAX_ICON_BYTES {
            return None;
        }
        let hash = icon_hash(&png);
        if !self.icons.contains_key(&hash) && self.icons.len() >= ICON_CACHE_ENTRIES {
            return None;
        }
        self.icons.insert(hash.clone(), png);
        Some(hash)
    }

    pub fn get(&self, hash: &str) -> Option<&[u8]> {
        self.icons.get(hash).map(Vec::as_slice)
    }
}

#[cfg(test)]
mod tests;
