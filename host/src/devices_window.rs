//! The native Win32 "BubblePlay - Devices" window of the host process.
//!
//! It implements the UI side of [`crate::devices_ui_api`]: [`DevicesEvent`] in,
//! [`DevicesCommand`] out, and nothing else. The window never touches the trusted list or the
//! relay. Layout, colours and wording follow `docs/design/devices-window-mockup.html`.
//!
//! The file has two halves. The first is plain Rust (model, text, QR matrix, control mapping) and
//! is unit-tested. The second is the Win32 shell: one window class on its own thread, pages made
//! by showing and hiding child controls, as `tray/key_ui.rs` does. Pairing codes, QR URIs and
//! comparison numbers live only in the model while they are on screen and are never logged.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, CreateSolidBrush, DeleteObject, FillRect, GetDC, GetSysColor,
    GetSysColorBrush, GetTextExtentPoint32W, InvalidateRect, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, CLEARTYPE_QUALITY, COLOR_WINDOW, COLOR_WINDOWTEXT, DEFAULT_CHARSET, HBRUSH, HDC,
    HFONT, HGDIOBJ, LOGFONTW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::Controls::{SetScrollInfo, ShowScrollBar, DRAWITEMSTRUCT, EM_SETRECT};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForWindow, SetThreadDpiAwarenessContext,
    SystemParametersInfoForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::devices_ui_api::{DeviceRow, DevicesCommand, DevicesEvent, DevicesUi};

// ---------------------------------------------------------------------------------------------
// Plain Rust half: model, text, QR, control mapping.
// ---------------------------------------------------------------------------------------------

/// What the window shows about this PC. The host fills it in once at start.
#[derive(Clone)]
pub struct DevicesWindowInfo {
    pub pc_name: String,
    pub pc_id: String,
    /// `https://play.example.com`, as the relay origin; shown without the scheme.
    pub relay_origin: String,
    /// True while the host holds its relay connection. Polled once per second.
    pub relay_online: Arc<AtomicBool>,
}

/// The one page the window shows at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    List,
    Waiting,
    Expired,
    Stopped,
    Failed,
    Request,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    Neutral,
    Ok,
    Error,
}

/// Every control of the main window, by its child ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum Ctl {
    Band = 101,
    ThisPc,
    PcName,
    PcId,
    RelayFixed,
    RelayWord,
    PairButton,
    PairedHead,
    Status,
    ListFrame,
    LvHead,
    ColName,
    ColId,
    ColSeen,
    Empty,
    Title,
    Qr,
    QrCaption,
    CodeLabel,
    Code,
    PairPcIdLabel,
    PairPcId,
    Instruction,
    Countdown,
    Message,
    MessageBody,
    DeviceLabel,
    DeviceName,
    DeviceId,
    NumberLabel,
    Number,
    NumberBody,
    Cancel,
    Close,
    NewCode,
    Allow,
    Deny,
}

const ALL_CTLS: [Ctl; 37] = [
    Ctl::Band,
    Ctl::ThisPc,
    Ctl::PcName,
    Ctl::PcId,
    Ctl::RelayFixed,
    Ctl::RelayWord,
    Ctl::PairButton,
    Ctl::PairedHead,
    Ctl::Status,
    Ctl::ListFrame,
    Ctl::LvHead,
    Ctl::ColName,
    Ctl::ColId,
    Ctl::ColSeen,
    Ctl::Empty,
    Ctl::Title,
    Ctl::Qr,
    Ctl::QrCaption,
    Ctl::CodeLabel,
    Ctl::Code,
    Ctl::PairPcIdLabel,
    Ctl::PairPcId,
    Ctl::Instruction,
    Ctl::Countdown,
    Ctl::Message,
    Ctl::MessageBody,
    Ctl::DeviceLabel,
    Ctl::DeviceName,
    Ctl::DeviceId,
    Ctl::NumberLabel,
    Ctl::Number,
    Ctl::NumberBody,
    Ctl::Cancel,
    Ctl::Close,
    Ctl::NewCode,
    Ctl::Allow,
    Ctl::Deny,
];

/// Controls that only paint a background for the controls over them, bottom first. They sit
/// at the bottom of the sibling z-order, in this order, with the list container just above.
const BACKGROUND_CTLS: [Ctl; 3] = [Ctl::Band, Ctl::ListFrame, Ctl::LvHead];

impl Ctl {
    pub fn id(self) -> i32 {
        self as i32
    }

    pub fn from_id(id: i32) -> Option<Self> {
        ALL_CTLS.iter().copied().find(|ctl| ctl.id() == id)
    }
}

/// Foreground colour roles, from the mockup's tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fg {
    Ink,
    Taupe,
    Cream,
    Milk,
    Apricot,
    Pastel,
    Error,
}

/// Background colour roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bg {
    Milk,
    Duvet,
    Raised,
    Soft,
    Cloth,
    Moss,
}

/// Font roles. The window builds one font per role and DPI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FontKind {
    Message,
    MessageBold,
    Semi20,
    Semi16,
    Semi13,
    Semi12,
    Mono15,
    Mono16,
    Mono12,
    Mono36,
    Mono72,
}

const FONT_KINDS: [FontKind; 11] = [
    FontKind::Message,
    FontKind::MessageBold,
    FontKind::Semi20,
    FontKind::Semi16,
    FontKind::Semi13,
    FontKind::Semi12,
    FontKind::Mono15,
    FontKind::Mono16,
    FontKind::Mono12,
    FontKind::Mono36,
    FontKind::Mono72,
];

const RGB_MILK: (u8, u8, u8) = (0xF3, 0xEF, 0xE7);
const RGB_RAISED: (u8, u8, u8) = (0xFF, 0xFA, 0xF1);
const RGB_CLOTH: (u8, u8, u8) = (0xDC, 0xD4, 0xC8);
const RGB_INK: (u8, u8, u8) = (0x28, 0x2A, 0x2F);
const RGB_TAUPE: (u8, u8, u8) = (0x62, 0x5C, 0x55);
const RGB_SOFT: (u8, u8, u8) = (0xB8, 0xAD, 0xA1);
const RGB_DUVET: (u8, u8, u8) = (0x2F, 0x31, 0x38);
const RGB_CREAM: (u8, u8, u8) = (0xC6, 0xBA, 0xAC);
const RGB_APRICOT: (u8, u8, u8) = (0xF0, 0xA0, 0x64);
const RGB_PASTEL: (u8, u8, u8) = (0xE5, 0xB1, 0xAA);
const RGB_MOSS: (u8, u8, u8) = (0xCA, 0xD8, 0xCD);
const RGB_ERROR: (u8, u8, u8) = (0xC4, 0x2B, 0x1C);
const RGB_BLACK: (u8, u8, u8) = (0x10, 0x12, 0x14);
const RGB_WHITE: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);

fn fg_rgb(fg: Fg) -> (u8, u8, u8) {
    match fg {
        Fg::Ink => RGB_INK,
        Fg::Taupe => RGB_TAUPE,
        Fg::Cream => RGB_CREAM,
        Fg::Milk => RGB_MILK,
        Fg::Apricot => RGB_APRICOT,
        Fg::Pastel => RGB_PASTEL,
        Fg::Error => RGB_ERROR,
    }
}

fn bg_rgb(bg: Bg) -> (u8, u8, u8) {
    match bg {
        Bg::Milk => RGB_MILK,
        Bg::Duvet => RGB_DUVET,
        Bg::Raised => RGB_RAISED,
        Bg::Soft => RGB_SOFT,
        Bg::Cloth => RGB_CLOTH,
        Bg::Moss => RGB_MOSS,
    }
}

/// Text shown for a device that sent no name.
pub const UNNAMED: &str = "Unnamed device";

pub fn display_name(name: &str) -> &str {
    if name.trim().is_empty() {
        UNNAMED
    } else {
        name
    }
}

/// `4:32`, rounded up so the last second reads `0:01`, not `0:00`.
pub fn format_countdown(remaining: Duration) -> String {
    let secs = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Unix seconds of an RFC 3339 timestamp such as `2026-10-27T22:14:03Z`. Fractions are ignored.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || !text.is_ascii() {
        return None;
    }
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let separators = bytes[4] == b'-'
        && bytes[7] == b'-'
        && matches!(bytes[10], b'T' | b't' | b' ')
        && bytes[13] == b':'
        && bytes[16] == b':';
    if !separators {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut index = 19;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
    }
    let offset = match bytes.get(index) {
        Some(b'Z' | b'z') if index + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) if bytes.len() == index + 6 && bytes[index + 3] == b':' => {
            let seconds = number(index + 1..index + 3)? * 3600 + number(index + 4..index + 6)? * 60;
            if *sign == b'-' {
                -seconds
            } else {
                seconds
            }
        }
        _ => return None,
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// The "Last seen" cell: `now · connected`, or how long ago the stored time was.
pub fn last_seen_text(row: &DeviceRow, now_unix: i64) -> String {
    if row.connected {
        return "now \u{b7} connected".to_owned();
    }
    let Some(text) = row.last_seen.as_deref() else {
        return "Never".to_owned();
    };
    let Some(seen) = parse_rfc3339(text) else {
        return "Unknown".to_owned();
    };
    let age = now_unix.saturating_sub(seen);
    match age {
        i64::MIN..=59 => "just now".to_owned(),
        60..=3599 => format!("{} min ago", age / 60),
        3600..=86_399 => format!("{} h ago", age / 3600),
        86_400..=172_799 => "yesterday".to_owned(),
        _ => format!("{} days ago", age / 86_400),
    }
}

/// The question the Revoke confirmation asks.
pub fn revoke_confirm_text(name: &str, id: &str, connected: bool) -> String {
    let tail = if connected {
        "It is disconnected now and can come back only by pairing again at this PC."
    } else {
        "It can come back only by pairing again at this PC."
    };
    format!("Revoke {} ({id})? {tail}", display_name(name))
}

/// `play.example.com` from `https://play.example.com`.
pub fn relay_host(origin: &str) -> &str {
    origin.split_once("://").map_or(origin, |(_, rest)| rest)
}

/// A QR code as a square module matrix.
#[derive(Clone, PartialEq, Eq)]
pub struct QrMatrix {
    pub version: i16,
    pub size: usize,
    dark: Vec<bool>,
}

impl QrMatrix {
    pub fn is_dark(&self, x: usize, y: usize) -> bool {
        x < self.size && y < self.size && self.dark[y * self.size + x]
    }
}

/// Encodes `uri` at error-correction level M. `None` if it does not fit any QR version.
pub fn qr_matrix(uri: &str) -> Option<QrMatrix> {
    use qrcode::{Color, EcLevel, QrCode, Version};
    let code = QrCode::with_error_correction_level(uri.as_bytes(), EcLevel::M).ok()?;
    let version = match code.version() {
        Version::Normal(version) => version,
        Version::Micro(version) => -version,
    };
    let size = code.width();
    let dark = code
        .to_colors()
        .into_iter()
        .map(|color| color == Color::Dark)
        .collect();
    Some(QrMatrix {
        version,
        size,
        dark,
    })
}

/// Pixels per module and the top-left offset that centre a matrix with a 4-module quiet zone in
/// a square box of `box_px`. `None` if even one pixel per module does not fit.
pub fn qr_layout(size: usize, box_px: i32) -> Option<(i32, i32)> {
    let total = i32::try_from(size).ok()?.checked_add(8)?;
    let scale = box_px / total;
    if scale < 1 {
        return None;
    }
    Some((scale, (box_px - scale * size as i32) / 2))
}

/// What the pairing page shows while a code is open.
struct PairInfo {
    code: String,
    pc_id: String,
    expires_at: SystemTime,
    qr: Option<QrMatrix>,
}

/// The pair request page.
struct RequestInfo {
    name: String,
    id: String,
    sas: String,
}

struct RevokeTarget {
    id: String,
    name: String,
    connected: bool,
}

/// Everything the window shows, and the rules for how events change it.
pub struct Model {
    pc_name: String,
    pc_id: String,
    relay_host: String,
    pub relay_online: bool,
    rows: Vec<DeviceRow>,
    rows_version: u64,
    new_id: Option<String>,
    page: Page,
    status: Option<(String, StatusKind)>,
    pair: Option<PairInfo>,
    request: Option<RequestInfo>,
    decided: bool,
    revoke: Option<RevokeTarget>,
}

impl Model {
    pub fn new(info: &DevicesWindowInfo) -> Self {
        Self {
            pc_name: info.pc_name.clone(),
            pc_id: info.pc_id.clone(),
            relay_host: relay_host(&info.relay_origin).to_owned(),
            relay_online: info.relay_online.load(Ordering::Relaxed),
            rows: Vec::new(),
            rows_version: 0,
            new_id: None,
            page: Page::List,
            status: None,
            pair: None,
            request: None,
            decided: false,
            revoke: None,
        }
    }

    pub fn page(&self) -> Page {
        self.page
    }

    /// Applies one host event.
    pub fn apply(&mut self, event: DevicesEvent) {
        match event {
            DevicesEvent::List(rows) => {
                self.rows = rows;
                self.rows_version += 1;
                if let Some(target) = &self.revoke {
                    if !self.rows.iter().any(|row| row.id == target.id) {
                        self.revoke = None;
                    }
                }
            }
            DevicesEvent::PairWaiting {
                code_display,
                qr_uri,
                pc_id,
                expires_at,
            } => {
                self.pair = Some(PairInfo {
                    code: code_display,
                    pc_id,
                    expires_at,
                    qr: qr_matrix(&qr_uri),
                });
                self.request = None;
                self.decided = false;
                self.status = None;
                self.new_id = None;
                self.rows_version += 1;
                self.page = Page::Waiting;
            }
            DevicesEvent::PairExpired => {
                if matches!(self.page, Page::Waiting | Page::Request | Page::Expired) {
                    self.end_pairing(Page::Expired);
                }
            }
            DevicesEvent::PairStopped => {
                // After Cancel the window is already back on the list; only a pairing that is
                // still on screen turns into the "stopped" page.
                if matches!(self.page, Page::Waiting | Page::Request) {
                    self.end_pairing(Page::Stopped);
                }
            }
            DevicesEvent::PairFailed => {
                if matches!(self.page, Page::Waiting | Page::Request) {
                    self.end_pairing(Page::Failed);
                }
            }
            DevicesEvent::PairRequest {
                id,
                name,
                sas_display,
            } => {
                self.request = Some(RequestInfo {
                    name,
                    id,
                    sas: sas_display,
                });
                self.decided = false;
                self.page = Page::Request;
            }
            DevicesEvent::PairDone { id, name } => {
                self.end_pairing(Page::List);
                self.status = Some((
                    format!("{} is paired.", display_name(&name)),
                    StatusKind::Ok,
                ));
                self.new_id = Some(id);
                self.rows_version += 1;
            }
            DevicesEvent::PairDenied => {
                self.end_pairing(Page::List);
                self.status = Some((
                    "Request denied. The device was not added.".to_owned(),
                    StatusKind::Neutral,
                ));
            }
            DevicesEvent::SaveError(_) => {
                // The raw error can carry file paths; the window says what matters.
                self.end_pairing(Page::List);
                self.status = Some((
                    "Couldn\u{2019}t save the device list. Nothing changed. Try again.".to_owned(),
                    StatusKind::Error,
                ));
            }
        }
    }

    fn end_pairing(&mut self, page: Page) {
        self.pair = None;
        self.request = None;
        self.decided = false;
        self.page = page;
    }

    /// The local pairing countdown: true once the code has run out and the page switched.
    pub fn tick(&mut self, now: SystemTime) -> bool {
        if self.page != Page::Waiting {
            return false;
        }
        let expired = self.pair.as_ref().is_none_or(|pair| pair.expires_at <= now);
        if expired {
            self.end_pairing(Page::Expired);
        }
        expired
    }

    /// Back to the list after Cancel, Close or Esc. `true` if a pairing was open, so the caller
    /// must tell the host.
    pub fn back_to_list(&mut self) -> bool {
        let was_open = matches!(self.page, Page::Waiting | Page::Request);
        self.end_pairing(Page::List);
        was_open
    }

    pub fn set_relay_online(&mut self, online: bool) {
        self.relay_online = online;
    }

    /// The line above the list: the last result, or the offline note.
    pub fn status_line(&self) -> Option<(String, StatusKind)> {
        if let Some(status) = &self.status {
            return Some(status.clone());
        }
        (!self.relay_online).then(|| {
            (
                "Pairing needs the relay, which is offline.".to_owned(),
                StatusKind::Neutral,
            )
        })
    }

    pub fn clear_status(&mut self) {
        self.status = None;
    }

    pub fn pair_enabled(&self) -> bool {
        self.relay_online
    }

    pub fn countdown_text(&self, now: SystemTime) -> String {
        let remaining = self
            .pair
            .as_ref()
            .and_then(|pair| pair.expires_at.duration_since(now).ok())
            .unwrap_or_default();
        format!("Expires in {}", format_countdown(remaining))
    }

    /// The controls shown on the current page.
    pub fn visible(&self) -> Vec<Ctl> {
        match self.page {
            Page::List => {
                let mut shown = vec![
                    Ctl::Band,
                    Ctl::ThisPc,
                    Ctl::PcName,
                    Ctl::PcId,
                    Ctl::RelayFixed,
                    Ctl::RelayWord,
                    Ctl::PairButton,
                    Ctl::PairedHead,
                    Ctl::ListFrame,
                    Ctl::LvHead,
                    Ctl::ColName,
                    Ctl::ColId,
                    Ctl::ColSeen,
                ];
                if self.status_line().is_some() {
                    shown.push(Ctl::Status);
                }
                if self.rows.is_empty() {
                    shown.push(Ctl::Empty);
                }
                shown
            }
            Page::Waiting => vec![
                Ctl::Title,
                Ctl::Qr,
                Ctl::QrCaption,
                Ctl::CodeLabel,
                Ctl::Code,
                Ctl::PairPcIdLabel,
                Ctl::PairPcId,
                Ctl::Instruction,
                Ctl::Countdown,
                Ctl::Cancel,
            ],
            Page::Expired => vec![
                Ctl::Title,
                Ctl::Message,
                Ctl::MessageBody,
                Ctl::NewCode,
                Ctl::Close,
            ],
            Page::Stopped | Page::Failed => {
                vec![Ctl::Title, Ctl::Message, Ctl::MessageBody, Ctl::NewCode]
            }
            Page::Request => vec![
                Ctl::Title,
                Ctl::DeviceLabel,
                Ctl::DeviceName,
                Ctl::DeviceId,
                Ctl::NumberLabel,
                Ctl::Number,
                Ctl::NumberBody,
                Ctl::Allow,
                Ctl::Deny,
            ],
        }
    }

    /// The button Enter presses on the current page. On the pair request it is Deny.
    pub fn default_button(&self) -> Option<Ctl> {
        match self.page {
            Page::List => self.pair_enabled().then_some(Ctl::PairButton),
            Page::Waiting => Some(Ctl::Cancel),
            Page::Expired | Page::Stopped | Page::Failed => Some(Ctl::NewCode),
            Page::Request => Some(Ctl::Deny),
        }
    }

    /// The control that takes focus when the page appears.
    pub fn first_focus(&self) -> Option<Ctl> {
        match self.page {
            Page::List => Some(Ctl::PairButton),
            _ => self.default_button(),
        }
    }

    pub fn button_enabled(&self, button: Ctl) -> bool {
        match button {
            Ctl::PairButton => self.pair_enabled(),
            Ctl::Allow | Ctl::Deny => !self.decided,
            _ => true,
        }
    }

    pub fn text(&self, ctl: Ctl, now: SystemTime) -> Option<String> {
        let text = match ctl {
            Ctl::ThisPc => "This PC".to_owned(),
            Ctl::PcName => self.pc_name.clone(),
            Ctl::PcId => self.pc_id.clone(),
            Ctl::RelayFixed => format!("Relay {} \u{b7} ", self.relay_host),
            Ctl::RelayWord => (if self.relay_online { "online" } else { "offline" }).to_owned(),
            Ctl::PairButton => "Pair device".to_owned(),
            Ctl::PairedHead => "Paired devices".to_owned(),
            Ctl::Status => self.status_line().map(|(text, _)| text).unwrap_or_default(),
            Ctl::ColName => "Name".to_owned(),
            Ctl::ColId => "ID".to_owned(),
            Ctl::ColSeen => "Last seen".to_owned(),
            Ctl::Empty => "\r\n\r\n\r\n\r\n\r\nNo paired devices yet.\r\nPair a device to use this PC from it.".to_owned(),
            Ctl::Title => (if self.page == Page::Request {
                "Pair this device?"
            } else {
                "Pair a device"
            })
            .to_owned(),
            Ctl::QrCaption => "Scan to pair this PC".to_owned(),
            Ctl::CodeLabel => "One-time code".to_owned(),
            Ctl::Code => self.pair.as_ref().map(|pair| pair.code.clone())?,
            Ctl::PairPcIdLabel => "PC ID".to_owned(),
            Ctl::PairPcId => self.pair.as_ref().map(|pair| pair.pc_id.clone())?,
            Ctl::Instruction => format!(
                "Scan with your phone camera, or open {}/pair in a browser and enter the PC ID and code.",
                self.relay_host
            ),
            Ctl::Countdown => self.countdown_text(now),
            Ctl::Message => (match self.page {
                Page::Expired => "This code expired.",
                Page::Failed => "Pairing failed",
                _ => "Pairing stopped",
            })
            .to_owned(),
            Ctl::MessageBody => (match self.page {
                Page::Expired => "Open a new code to try again.",
                Page::Failed => "Pairing failed: the code didn't match. Open a new code.",
                _ => "Pairing stopped by the relay or the other device. Open a new code to try again.",
            })
            .to_owned(),
            Ctl::DeviceLabel => "Device".to_owned(),
            Ctl::DeviceName => self
                .request
                .as_ref()
                .map(|request| display_name(&request.name).to_owned())?,
            Ctl::DeviceId => self.request.as_ref().map(|request| request.id.clone())?,
            Ctl::NumberLabel => "Comparison number".to_owned(),
            Ctl::Number => self.request.as_ref().map(|request| request.sas.clone())?,
            Ctl::NumberBody => {
                "Check that the device shows the same number. If it doesn't, choose Deny.".to_owned()
            }
            Ctl::Cancel => "Cancel".to_owned(),
            Ctl::Close => "Close".to_owned(),
            Ctl::NewCode => "New code".to_owned(),
            Ctl::Allow => "Allow".to_owned(),
            Ctl::Deny => "Deny".to_owned(),
            Ctl::Band | Ctl::ListFrame | Ctl::LvHead | Ctl::Qr => String::new(),
        };
        Some(text)
    }

    /// Font, text colour and background of a control in the current state.
    pub fn look(&self, ctl: Ctl) -> (FontKind, Fg, Bg) {
        let stopped = matches!(self.page, Page::Stopped | Page::Failed);
        match ctl {
            Ctl::Band => (FontKind::Message, Fg::Cream, Bg::Duvet),
            Ctl::ThisPc | Ctl::RelayFixed => (FontKind::Message, Fg::Cream, Bg::Duvet),
            Ctl::PcName => (FontKind::Semi20, Fg::Milk, Bg::Duvet),
            Ctl::PcId => (FontKind::Mono15, Fg::Milk, Bg::Duvet),
            Ctl::RelayWord => (
                FontKind::MessageBold,
                if self.relay_online {
                    Fg::Apricot
                } else {
                    Fg::Pastel
                },
                Bg::Duvet,
            ),
            Ctl::PairedHead => (FontKind::Semi13, Fg::Ink, Bg::Milk),
            Ctl::Status => match self.status_line() {
                Some((_, StatusKind::Error)) => (FontKind::Message, Fg::Error, Bg::Milk),
                Some((_, StatusKind::Ok)) => (FontKind::MessageBold, Fg::Ink, Bg::Milk),
                _ => (FontKind::Message, Fg::Ink, Bg::Milk),
            },
            Ctl::ListFrame => (FontKind::Message, Fg::Ink, Bg::Soft),
            Ctl::LvHead => (FontKind::Message, Fg::Ink, Bg::Cloth),
            Ctl::ColName | Ctl::ColId | Ctl::ColSeen => (FontKind::Semi12, Fg::Ink, Bg::Cloth),
            Ctl::Empty => (FontKind::Message, Fg::Taupe, Bg::Raised),
            Ctl::Title => (FontKind::Semi20, Fg::Ink, Bg::Milk),
            Ctl::QrCaption => (FontKind::Message, Fg::Taupe, Bg::Milk),
            Ctl::CodeLabel | Ctl::PairPcIdLabel | Ctl::DeviceLabel | Ctl::NumberLabel => {
                (FontKind::Message, Fg::Taupe, Bg::Milk)
            }
            Ctl::Code => (FontKind::Mono36, Fg::Ink, Bg::Raised),
            Ctl::PairPcId | Ctl::DeviceId => (FontKind::Mono16, Fg::Ink, Bg::Milk),
            Ctl::Instruction | Ctl::NumberBody => (FontKind::Message, Fg::Ink, Bg::Milk),
            Ctl::Countdown => (FontKind::Semi13, Fg::Ink, Bg::Milk),
            Ctl::Message => (
                FontKind::Semi16,
                if stopped { Fg::Error } else { Fg::Ink },
                Bg::Milk,
            ),
            Ctl::MessageBody => (
                FontKind::Message,
                if stopped { Fg::Error } else { Fg::Ink },
                Bg::Milk,
            ),
            Ctl::DeviceName => (FontKind::Semi16, Fg::Ink, Bg::Milk),
            Ctl::Number => (FontKind::Mono72, Fg::Ink, Bg::Raised),
            Ctl::Qr
            | Ctl::PairButton
            | Ctl::Cancel
            | Ctl::Close
            | Ctl::NewCode
            | Ctl::Allow
            | Ctl::Deny => (FontKind::Message, Fg::Ink, Bg::Milk),
        }
    }

    /// Colours of one list cell. `index` is the row, `column` 0 name, 1 ID, 2 last seen.
    pub fn row_look(&self, index: usize, column: usize) -> (FontKind, Fg, Bg) {
        let new = self.row_is_new(index);
        let font = match column {
            1 => FontKind::Mono12,
            0 if new => FontKind::MessageBold,
            _ => FontKind::Message,
        };
        (font, Fg::Ink, if new { Bg::Moss } else { Bg::Raised })
    }

    pub fn row_is_new(&self, index: usize) -> bool {
        match (&self.new_id, self.rows.get(index)) {
            (Some(new), Some(row)) => &row.id == new,
            _ => false,
        }
    }

    pub fn row_texts(&self, index: usize, now_unix: i64) -> Option<[String; 3]> {
        let row = self.rows.get(index)?;
        let mut name = display_name(&row.name).to_owned();
        if self.row_is_new(index) {
            name.push_str(" (new)");
        }
        Some([name, row.id.clone(), last_seen_text(row, now_unix)])
    }
}

// ---------------------------------------------------------------------------------------------
// Win32 half.
// ---------------------------------------------------------------------------------------------

const ROW_HEIGHT: i32 = 44;
const CLIENT_W: i32 = 560;
const CLIENT_H: i32 = 408;
// Inside the list frame: below the column band, clear of the frame's 1 px border on three sides.
const LIST_BODY: [i32; 4] = [17, 178, 526, 213];
const ROWS_ID: i32 = 1500;
const ID_REVOKE_BASE: i32 = 2000;
const ID_CELL_BASE: i32 = 3000;
const RV_TEXT: i32 = 301;
const RV_REVOKE: i32 = 302;
const RV_CANCEL: i32 = 303;
const TIMER_ID: usize = 1;
const WM_APP_EVENT: u32 = WM_APP + 1;
const WM_APP_RAISE: u32 = WM_APP + 2;
const MAIN_STYLE: WINDOW_STYLE =
    WINDOW_STYLE(WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_CLIPCHILDREN.0);
const MAIN_EX_STYLE: WINDOW_EX_STYLE = WS_EX_DLGMODALFRAME;

// Static and edit styles as plain numbers, so they combine with WINDOW_STYLE bits.
const SS_NOPREFIX_: u32 = 0x80;
const SS_CENTER_: u32 = 0x1;
const SS_CENTERIMAGE_: u32 = 0x200;
const SS_ENDELLIPSIS_: u32 = 0x4000;
const SS_OWNERDRAW_: u32 = 0xD;
const ES_READONLY_: u32 = 0x800;
const ES_CENTER_: u32 = 0x1;
const ES_MULTILINE_: u32 = 0x4;
const ES_AUTOHSCROLL_: u32 = 0x80;
const BS_PUSHBUTTON_: u32 = 0x0;
const BS_DEFPUSHBUTTON_: u32 = 0x1;
const BS_NOTIFY_: u32 = 0x4000;

fn colorref((r, g, b): (u8, u8, u8)) -> COLORREF {
    COLORREF(u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Where a control sits, in 96-dpi client pixels: `[x, y, width, height]`.
fn place(ctl: Ctl, page: Page) -> [i32; 4] {
    match ctl {
        Ctl::Band => [0, 0, CLIENT_W, 96],
        Ctl::ThisPc => [16, 8, 200, 16],
        Ctl::PcName => [16, 22, 360, 28],
        Ctl::PcId => [16, 50, 360, 20],
        Ctl::RelayFixed | Ctl::RelayWord => [16, 72, 380, 18],
        Ctl::PairButton => [404, 28, 140, 38],
        Ctl::PairedHead => [16, 104, 300, 20],
        Ctl::Status => [16, 124, 528, 20],
        Ctl::ListFrame => [16, 150, 528, 242],
        Ctl::LvHead => [17, 151, 526, 26],
        Ctl::ColName => [28, 156, 110, 18],
        Ctl::ColId => [152, 156, 110, 18],
        Ctl::ColSeen => [334, 156, 110, 18],
        Ctl::Empty => LIST_BODY,
        Ctl::Title => [24, 18, 512, 30],
        Ctl::Qr => [24, 64, 196, 196],
        Ctl::QrCaption => [24, 266, 196, 32],
        Ctl::CodeLabel => [244, 62, 292, 16],
        Ctl::Code => [244, 80, 292, 62],
        Ctl::PairPcIdLabel => [244, 158, 292, 16],
        Ctl::PairPcId => [244, 176, 292, 22],
        Ctl::Instruction => [244, 212, 292, 66],
        Ctl::Countdown => [244, 300, 292, 20],
        Ctl::Message => [24, 64, 512, 24],
        Ctl::MessageBody => [24, 94, 512, 60],
        Ctl::DeviceLabel => [24, 62, 120, 16],
        Ctl::DeviceName => [24, 80, 512, 22],
        Ctl::DeviceId => [24, 104, 512, 20],
        Ctl::NumberLabel => [24, 142, 512, 16],
        Ctl::Number => [24, 162, 512, 104],
        Ctl::NumberBody => [24, 282, 512, 40],
        Ctl::Cancel | Ctl::Close | Ctl::Deny => [444, 352, 100, 32],
        Ctl::NewCode => {
            if page == Page::Expired {
                [336, 352, 100, 32]
            } else {
                [444, 352, 100, 32]
            }
        }
        Ctl::Allow => [336, 352, 100, 32],
    }
}

/// Window class name, extra style bits and tab stop for each control.
fn spec(ctl: Ctl) -> (&'static str, u32) {
    let label = SS_NOPREFIX_;
    match ctl {
        Ctl::Band | Ctl::ListFrame | Ctl::LvHead => ("STATIC", label),
        Ctl::PcId | Ctl::PairPcId | Ctl::DeviceId => {
            ("EDIT", ES_READONLY_ | ES_AUTOHSCROLL_ | WS_TABSTOP.0)
        }
        Ctl::Code => (
            "EDIT",
            ES_READONLY_ | ES_CENTER_ | ES_MULTILINE_ | WS_BORDER.0 | WS_TABSTOP.0,
        ),
        Ctl::Number => ("STATIC", label | SS_CENTER_ | SS_CENTERIMAGE_ | WS_BORDER.0),
        Ctl::Qr => ("STATIC", SS_OWNERDRAW_),
        Ctl::Empty => ("STATIC", label | SS_CENTER_),
        Ctl::ColName | Ctl::ColId | Ctl::ColSeen => ("STATIC", label | SS_ENDELLIPSIS_),
        Ctl::PairButton | Ctl::Cancel | Ctl::Close | Ctl::NewCode | Ctl::Allow | Ctl::Deny => {
            ("BUTTON", BS_PUSHBUTTON_ | WS_TABSTOP.0)
        }
        Ctl::QrCaption => ("STATIC", label | SS_CENTER_),
        _ => ("STATIC", label),
    }
}

/// The window thread's shared slot: closed, being created, or open with its handle.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Closed,
    Starting,
    Open(isize),
}

struct Inner {
    slot: Slot,
    queue: VecDeque<DevicesEvent>,
}

struct Shared {
    info: DevicesWindowInfo,
    commands: Sender<DevicesCommand>,
    inner: Mutex<Inner>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn send(&self, command: DevicesCommand) {
        let _ = self.commands.send(command);
    }
}

/// The Devices window. Cheap to clone; every clone drives the same single window.
#[derive(Clone)]
pub struct DevicesWindow {
    shared: Arc<Shared>,
}

impl DevicesWindow {
    pub fn new(info: DevicesWindowInfo, commands: Sender<DevicesCommand>) -> Self {
        Self {
            shared: Arc::new(Shared {
                info,
                commands,
                inner: Mutex::new(Inner {
                    slot: Slot::Closed,
                    queue: VecDeque::new(),
                }),
            }),
        }
    }

    /// Opens the window, or brings the open one to the front.
    pub fn open(&self) {
        let mut inner = self.shared.lock();
        if let Slot::Open(handle) = inner.slot {
            // SAFETY: IsWindow accepts any handle value.
            let alive = unsafe { IsWindow(Some(HWND(handle as *mut _))).as_bool() };
            if alive {
                // SAFETY: posting to a window of another thread is allowed.
                let _ = unsafe {
                    PostMessageW(
                        Some(HWND(handle as *mut _)),
                        WM_APP_RAISE,
                        WPARAM(0),
                        LPARAM(0),
                    )
                };
                return;
            }
            inner.slot = Slot::Closed;
        }
        if inner.slot == Slot::Starting {
            return;
        }
        inner.slot = Slot::Starting;
        let shared = Arc::clone(&self.shared);
        let spawned = std::thread::Builder::new()
            .name("devices-window".into())
            .spawn(move || window_thread(shared));
        if spawned.is_err() {
            inner.slot = Slot::Closed;
        }
    }
}

impl DevicesUi for DevicesWindow {
    fn show(&mut self, event: DevicesEvent) {
        let mut inner = self.shared.lock();
        match inner.slot {
            Slot::Closed => {}
            Slot::Starting => inner.queue.push_back(event),
            Slot::Open(handle) => {
                inner.queue.push_back(event);
                // SAFETY: posting to a window of another thread is allowed.
                let _ = unsafe {
                    PostMessageW(
                        Some(HWND(handle as *mut _)),
                        WM_APP_EVENT,
                        WPARAM(0),
                        LPARAM(0),
                    )
                };
            }
        }
    }
}

struct Fonts {
    handles: HashMap<FontKind, HFONT>,
}

impl Fonts {
    fn get(&self, kind: FontKind) -> HFONT {
        self.handles
            .get(&kind)
            .copied()
            .unwrap_or(HFONT(std::ptr::null_mut()))
    }
}

/// The window's state. It lives on the window thread's stack; the window procedures reach it
/// through the window's user data.
struct Ui {
    shared: Arc<Shared>,
    model: RefCell<Model>,
    hwnd: Cell<HWND>,
    rows_hwnd: Cell<HWND>,
    confirm: Cell<HWND>,
    dpi: Cell<u32>,
    high_contrast: Cell<bool>,
    fonts: RefCell<Fonts>,
    brushes: RefCell<HashMap<(u8, u8, u8), HBRUSH>>,
    /// What was last applied to each control: text, look, visibility, enabled.
    applied_text: RefCell<HashMap<i32, String>>,
    applied_look: RefCell<HashMap<i32, (FontKind, Fg, Bg)>>,
    applied_shown: RefCell<HashMap<i32, bool>>,
    applied_enabled: RefCell<HashMap<i32, bool>>,
    applied_page: Cell<Option<Page>>,
    applied_default: Cell<Option<Ctl>>,
    rows_built: Cell<u64>,
    row_children: RefCell<Vec<HWND>>,
    scroll: Cell<i32>,
    last_minute: Cell<i64>,
    qr_cache: RefCell<Option<QrMatrix>>,
    /// Test harness: keep the windows far off every monitor and never activate them.
    offscreen: Cell<bool>,
}

fn window_thread(shared: Arc<Shared>) {
    // SAFETY: everything below is created, pumped and destroyed on this thread; `ui` outlives
    // the message loop and is reached from the window procedures only through user data.
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let ui = Ui::new(Arc::clone(&shared));
        let created = ui.create();
        {
            let mut inner = shared.lock();
            match created {
                Some(hwnd) => inner.slot = Slot::Open(hwnd.0 as isize),
                None => {
                    inner.slot = Slot::Closed;
                    inner.queue.clear();
                }
            }
        }
        if let Some(hwnd) = created {
            ui.drain_events();
            ui.sync();
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            ui.focus_first();
            pump(&ui, hwnd);
        }
        {
            let mut inner = shared.lock();
            inner.slot = Slot::Closed;
            inner.queue.clear();
        }
        ui.release_resources();
    }
}

/// Runs the message loop until the window is destroyed. The Revoke confirmation is a second
/// window on this thread, so its keyboard messages are tried first.
unsafe fn pump(ui: &Ui, main: HWND) {
    let mut msg = MSG::default();
    loop {
        let status = GetMessageW(&mut msg, None, 0, 0).0;
        if status == -1 || status == 0 {
            break;
        }
        let confirm = ui.confirm.get();
        let handled = (!confirm.is_invalid()
            && IsWindow(Some(confirm)).as_bool()
            && IsDialogMessageW(confirm, &msg).as_bool())
            || IsDialogMessageW(main, &msg).as_bool();
        if !handled {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn register_class(
    name: PCWSTR,
    proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
    instance: HINSTANCE,
) -> bool {
    // SAFETY: the class data is plain values that RegisterClassW copies. A class that is already
    // registered by an earlier window counts as success.
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance,
            lpszClassName: name,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class) != 0
            || windows::Win32::Foundation::GetLastError()
                == windows::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS
    }
}

impl Ui {
    fn new(shared: Arc<Shared>) -> Self {
        let model = Model::new(&shared.info);
        Self {
            shared,
            model: RefCell::new(model),
            hwnd: Cell::new(HWND::default()),
            rows_hwnd: Cell::new(HWND::default()),
            confirm: Cell::new(HWND::default()),
            dpi: Cell::new(96),
            high_contrast: Cell::new(false),
            fonts: RefCell::new(Fonts {
                handles: HashMap::new(),
            }),
            brushes: RefCell::new(HashMap::new()),
            applied_text: RefCell::default(),
            applied_look: RefCell::default(),
            applied_shown: RefCell::default(),
            applied_enabled: RefCell::default(),
            applied_page: Cell::new(None),
            applied_default: Cell::new(None),
            rows_built: Cell::new(u64::MAX),
            row_children: RefCell::default(),
            scroll: Cell::new(0),
            last_minute: Cell::new(now_unix() / 60),
            qr_cache: RefCell::new(None),
            offscreen: Cell::new(false),
        }
    }

    fn px(&self, value: i32) -> i32 {
        (i64::from(value) * i64::from(self.dpi.get()) / 96) as i32
    }

    unsafe fn create(&self) -> Option<HWND> {
        let instance: HINSTANCE = GetModuleHandleW(PCWSTR::null()).ok()?.into();
        if !register_class(w!("BubblePlayDevices"), main_proc, instance)
            || !register_class(w!("BubblePlayDeviceRows"), rows_proc, instance)
            || !register_class(w!("BubblePlayDeviceRevoke"), confirm_proc, instance)
        {
            return None;
        }
        let title = wide("BubblePlay \u{2014} Devices");
        let extra = if self.offscreen.get() {
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
        } else {
            WINDOW_EX_STYLE(0)
        };
        let hwnd = CreateWindowExW(
            MAIN_EX_STYLE | extra,
            w!("BubblePlayDevices"),
            PCWSTR(title.as_ptr()),
            MAIN_STYLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CLIENT_W,
            CLIENT_H,
            None,
            None,
            Some(instance),
            Some(self as *const Ui as *const _),
        )
        .ok()?;
        self.hwnd.set(hwnd);
        self.dpi.set(GetDpiForWindow(hwnd).max(96));
        self.detect_high_contrast();
        self.make_fonts();
        if self.create_controls(hwnd, instance).is_none() {
            let _ = DestroyWindow(hwnd);
            return None;
        }
        self.resize_window(None);
        self.layout();
        let _ = SetTimer(Some(hwnd), TIMER_ID, 1000, None);
        Some(hwnd)
    }

    unsafe fn create_controls(&self, hwnd: HWND, instance: HINSTANCE) -> Option<()> {
        // New children go to the bottom of the z-order, so creation order alone would put the
        // first-created Band on top. With WS_CLIPSIBLINGS the upper sibling wins where controls
        // overlap, so the background controls and the list container move down after the loop.
        for ctl in ALL_CTLS {
            let (class, style) = spec(ctl);
            let class = wide(class);
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class.as_ptr()),
                w!(""),
                WS_CHILD | WS_CLIPSIBLINGS | WINDOW_STYLE(style),
                0,
                0,
                0,
                0,
                Some(hwnd),
                Some(HMENU(ctl.id() as usize as *mut _)),
                Some(instance),
                None,
            )
            .ok()?;
            if ctl == Ctl::ColSeen {
                let rows = CreateWindowExW(
                    WS_EX_CONTROLPARENT,
                    w!("BubblePlayDeviceRows"),
                    w!(""),
                    WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                    0,
                    0,
                    0,
                    0,
                    Some(hwnd),
                    Some(HMENU(ROWS_ID as usize as *mut _)),
                    Some(instance),
                    Some(self as *const Ui as *const _),
                )
                .ok()?;
                self.rows_hwnd.set(rows);
            }
        }
        // Bottom-up: Band, ListFrame, LvHead, then the list container above the frame and column
        // band but below the empty text and every other control.
        let mut lowered = vec![self.rows_hwnd.get()];
        for ctl in BACKGROUND_CTLS.iter().rev() {
            lowered.push(self.ctl(*ctl)?);
        }
        for handle in lowered {
            SetWindowPos(
                handle,
                Some(HWND_BOTTOM),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOREDRAW,
            )
            .ok()?;
        }
        Some(())
    }

    fn ctl(&self, ctl: Ctl) -> Option<HWND> {
        // SAFETY: GetDlgItem only reads the child list of the window.
        unsafe { GetDlgItem(Some(self.hwnd.get()), ctl.id()).ok() }
    }

    unsafe fn detect_high_contrast(&self) {
        let mut info = HIGHCONTRASTW {
            cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
            ..Default::default()
        };
        let read = SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            info.cbSize,
            Some(std::ptr::addr_of_mut!(info).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        self.high_contrast
            .set(read.is_ok() && info.dwFlags.0 & HCF_HIGHCONTRASTON.0 != 0);
    }

    fn brush(&self, rgb: (u8, u8, u8)) -> HBRUSH {
        if self.high_contrast.get() {
            // SAFETY: system brushes are shared and never freed by the caller.
            return unsafe { GetSysColorBrush(COLOR_WINDOW) };
        }
        *self
            .brushes
            .borrow_mut()
            .entry(rgb)
            // SAFETY: CreateSolidBrush has no preconditions; the brush is freed in
            // `release_resources`.
            .or_insert_with(|| unsafe { CreateSolidBrush(colorref(rgb)) })
    }

    fn text_colour(&self, fg: Fg) -> COLORREF {
        if self.high_contrast.get() {
            // SAFETY: GetSysColor has no preconditions.
            return COLORREF(unsafe { GetSysColor(COLOR_WINDOWTEXT) });
        }
        colorref(fg_rgb(fg))
    }

    unsafe fn make_fonts(&self) {
        let mut fonts = self.fonts.borrow_mut();
        for font in fonts.handles.values() {
            let _ = DeleteObject((*font).into());
        }
        fonts.handles.clear();
        let message = message_font_logfont(self.dpi.get());
        for kind in FONT_KINDS {
            let (px, weight, face): (i32, i32, &str) = match kind {
                FontKind::Message => (0, 0, ""),
                FontKind::MessageBold => (0, 700, ""),
                FontKind::Semi20 => (20, 600, "Segoe UI Semibold"),
                FontKind::Semi16 => (16, 600, "Segoe UI Semibold"),
                FontKind::Semi13 => (13, 600, "Segoe UI Semibold"),
                FontKind::Semi12 => (12, 600, "Segoe UI Semibold"),
                FontKind::Mono15 => (15, 400, "Consolas"),
                FontKind::Mono16 => (16, 600, "Consolas"),
                FontKind::Mono12 => (12, 400, "Consolas"),
                FontKind::Mono36 => (36, 700, "Consolas"),
                FontKind::Mono72 => (72, 700, "Consolas"),
            };
            let mut log = message;
            if px != 0 {
                log.lfHeight = -self.px(px);
                log.lfWeight = weight;
                log.lfFaceName = [0; 32];
                for (slot, unit) in log.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
                    *slot = unit;
                }
            } else {
                if weight != 0 {
                    log.lfWeight = weight;
                }
            }
            log.lfQuality = CLEARTYPE_QUALITY;
            let handle = CreateFontIndirectW(&log);
            fonts.handles.insert(kind, handle);
        }
    }

    unsafe fn release_resources(&self) {
        for font in self
            .fonts
            .borrow_mut()
            .handles
            .drain()
            .map(|(_, font)| font)
        {
            let _ = DeleteObject(font.into());
        }
        for brush in self.brushes.borrow_mut().drain().map(|(_, brush)| brush) {
            let _ = DeleteObject(brush.into());
        }
    }

    /// Sizes the window so its client area is 560 x 408 at the current DPI, centred on screen.
    unsafe fn resize_window(&self, suggested: Option<RECT>) {
        let hwnd = self.hwnd.get();
        if let Some(rect) = suggested {
            let _ = SetWindowPos(
                hwnd,
                None,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            return;
        }
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: self.px(CLIENT_W),
            bottom: self.px(CLIENT_H),
        };
        let _ =
            AdjustWindowRectExForDpi(&mut rect, MAIN_STYLE, false, MAIN_EX_STYLE, self.dpi.get());
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        let (left, top) = if self.offscreen.get() {
            (-30_000, -30_000)
        } else {
            (
                ((GetSystemMetrics(SM_CXSCREEN) - width) / 2).max(0),
                ((GetSystemMetrics(SM_CYSCREEN) - height) / 2).max(0),
            )
        };
        let _ = SetWindowPos(
            hwnd,
            None,
            left,
            top,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }

    /// Positions every control for the current page and DPI.
    unsafe fn layout(&self) {
        let page = self.model.borrow().page();
        for ctl in ALL_CTLS {
            let Some(handle) = self.ctl(ctl) else {
                continue;
            };
            let [x, y, width, height] = place(ctl, page);
            let mut width = self.px(width);
            if ctl == Ctl::RelayFixed {
                width = self.measure(handle, ctl).min(self.px(380));
            }
            let mut left = self.px(x);
            if ctl == Ctl::RelayWord {
                left += self.measure(self.ctl(Ctl::RelayFixed).unwrap_or(handle), Ctl::RelayFixed);
                width = self.px(80);
            }
            let _ = MoveWindow(handle, left, self.px(y), width, self.px(height), true);
            if ctl == Ctl::Code {
                // Single-line text sits at the top of an edit; centre it in the box.
                let line = self.px(36) * 6 / 5;
                let inset = ((self.px(height) - 2 - line) / 2).max(0);
                let rect = RECT {
                    left: 0,
                    top: inset,
                    right: width - 4,
                    bottom: self.px(height) - 2,
                };
                let _ = SendMessageW(
                    handle,
                    EM_SETRECT,
                    Some(WPARAM(0)),
                    Some(LPARAM(std::ptr::addr_of!(rect) as isize)),
                );
            }
        }
        let [x, y, width, height] = LIST_BODY;
        let _ = MoveWindow(
            self.rows_hwnd.get(),
            self.px(x),
            self.px(y),
            self.px(width),
            self.px(height),
            true,
        );
        self.rows_built.set(u64::MAX);
    }

    /// Width in pixels of the control's current text in its font.
    unsafe fn measure(&self, handle: HWND, ctl: Ctl) -> i32 {
        let font = self.model.borrow().look(ctl).0;
        let text = self.applied_text.borrow().get(&ctl.id()).cloned();
        let text: Vec<u16> = match text {
            Some(text) => text.encode_utf16().collect(),
            None => self
                .model
                .borrow()
                .text(ctl, SystemTime::now())
                .unwrap_or_default()
                .encode_utf16()
                .collect(),
        };
        let dc = GetDC(Some(handle));
        let old = SelectObject(dc, HGDIOBJ(self.fonts.borrow().get(font).0));
        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &text, &mut size);
        SelectObject(dc, old);
        ReleaseDC(Some(handle), dc);
        size.cx + 2
    }

    fn drain_events(&self) {
        let events: Vec<DevicesEvent> = {
            let mut inner = self.shared.lock();
            inner.queue.drain(..).collect()
        };
        if events.is_empty() {
            return;
        }
        let mut close_confirm = false;
        {
            let mut model = self.model.borrow_mut();
            for event in events {
                model.apply(event);
            }
            if model.revoke.is_none() {
                close_confirm = true;
            }
        }
        if close_confirm {
            // SAFETY: closes a window of this thread.
            unsafe { self.close_confirm() };
        }
        // SAFETY: runs on the window thread.
        unsafe { self.sync() };
    }

    /// Brings every control in line with the model. Only what changed is touched.
    unsafe fn sync(&self) {
        let hwnd = self.hwnd.get();
        if hwnd.is_invalid() {
            return;
        }
        let now = SystemTime::now();
        let (page, visible, default, focus, qr) = {
            let model = self.model.borrow();
            (
                model.page(),
                model.visible(),
                model.default_button(),
                model.first_focus(),
                model.pair.as_ref().and_then(|pair| pair.qr.clone()),
            )
        };
        let page_changed = self.applied_page.get() != Some(page);
        if page_changed {
            self.layout();
        }
        if *self.qr_cache.borrow() != qr {
            *self.qr_cache.borrow_mut() = qr;
            if let Some(handle) = self.ctl(Ctl::Qr) {
                let _ = InvalidateRect(Some(handle), None, true);
            }
        }
        for ctl in ALL_CTLS {
            let Some(handle) = self.ctl(ctl) else {
                continue;
            };
            let shown = visible.contains(&ctl);
            let (text, look, enabled) = {
                let model = self.model.borrow();
                (
                    model.text(ctl, now),
                    model.look(ctl),
                    model.button_enabled(ctl),
                )
            };
            if let Some(text) = text {
                if shown && self.applied_text.borrow().get(&ctl.id()) != Some(&text) {
                    let wide_text = wide(&text);
                    let _ = SetWindowTextW(handle, PCWSTR(wide_text.as_ptr()));
                    self.applied_text.borrow_mut().insert(ctl.id(), text);
                    if ctl == Ctl::RelayFixed {
                        self.layout();
                    }
                }
            }
            if self.applied_look.borrow().get(&ctl.id()) != Some(&look) {
                let _ = SendMessageW(
                    handle,
                    WM_SETFONT,
                    Some(WPARAM(self.fonts.borrow().get(look.0).0 as usize)),
                    Some(LPARAM(1)),
                );
                let _ = InvalidateRect(Some(handle), None, true);
                self.applied_look.borrow_mut().insert(ctl.id(), look);
            }
            if self.applied_enabled.borrow().get(&ctl.id()) != Some(&enabled) {
                let _ = EnableWindow(handle, enabled);
                self.applied_enabled.borrow_mut().insert(ctl.id(), enabled);
            }
            if self.applied_shown.borrow().get(&ctl.id()) != Some(&shown) {
                let _ = ShowWindow(handle, if shown { SW_SHOWNA } else { SW_HIDE });
                self.applied_shown.borrow_mut().insert(ctl.id(), shown);
            }
        }
        // The container shows with the list page when there are rows.
        let rows_visible = page == Page::List && !self.model.borrow().rows.is_empty();
        let _ = ShowWindow(
            self.rows_hwnd.get(),
            if rows_visible { SW_SHOWNA } else { SW_HIDE },
        );
        let version = self.model.borrow().rows_version;
        if self.rows_built.get() != version {
            self.rebuild_rows();
        }
        if self.applied_default.get() != default || page_changed {
            for button in [
                Ctl::PairButton,
                Ctl::Cancel,
                Ctl::Close,
                Ctl::NewCode,
                Ctl::Allow,
                Ctl::Deny,
            ] {
                if let Some(handle) = self.ctl(button) {
                    let style = if Some(button) == default {
                        BS_DEFPUSHBUTTON_
                    } else {
                        BS_PUSHBUTTON_
                    };
                    let _ = SendMessageW(
                        handle,
                        BM_SETSTYLE,
                        Some(WPARAM(style as usize)),
                        Some(LPARAM(1)),
                    );
                }
            }
            self.applied_default.set(default);
        }
        if page_changed {
            self.applied_page.set(Some(page));
            if self.confirm.get().is_invalid() {
                if let Some(target) = focus.and_then(|ctl| self.ctl(ctl)) {
                    let _ = SetFocus(Some(target));
                }
            }
        }
    }

    unsafe fn focus_first(&self) {
        let focus = self.model.borrow().first_focus();
        if let Some(target) = focus.and_then(|ctl| self.ctl(ctl)) {
            let _ = SetFocus(Some(target));
        }
    }

    /// Replaces the row controls with ones for the current list.
    unsafe fn rebuild_rows(&self) {
        let container = self.rows_hwnd.get();
        if container.is_invalid() {
            return;
        }
        let old = std::mem::take(&mut *self.row_children.borrow_mut());
        for child in old {
            let _ = DestroyWindow(child);
        }
        let Ok(instance) = GetModuleHandleW(PCWSTR::null()) else {
            return;
        };
        let instance: HINSTANCE = instance.into();
        let count = self.model.borrow().rows.len();
        let now = now_unix();
        let mut made = Vec::new();
        for index in 0..count {
            let (texts, looks) = {
                let model = self.model.borrow();
                (
                    model.row_texts(index, now),
                    [0, 1, 2].map(|column| model.row_look(index, column)),
                )
            };
            let Some(texts) = texts else { continue };
            for (column, cell_text) in texts.iter().enumerate() {
                let text = wide(cell_text);
                let cell = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    PCWSTR(text.as_ptr()),
                    WS_CHILD
                        | WS_VISIBLE
                        | WS_CLIPSIBLINGS
                        | WINDOW_STYLE(SS_NOPREFIX_ | SS_ENDELLIPSIS_),
                    0,
                    0,
                    0,
                    0,
                    Some(container),
                    Some(HMENU(
                        (ID_CELL_BASE + index as i32 * 4 + column as i32) as usize as *mut _,
                    )),
                    Some(instance),
                    None,
                );
                if let Ok(cell) = cell {
                    let _ = SendMessageW(
                        cell,
                        WM_SETFONT,
                        Some(WPARAM(self.fonts.borrow().get(looks[column].0).0 as usize)),
                        Some(LPARAM(0)),
                    );
                    made.push(cell);
                }
            }
            let button = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                w!("Revoke"),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_CLIPSIBLINGS
                    | WINDOW_STYLE(BS_PUSHBUTTON_ | BS_NOTIFY_ | WS_TABSTOP.0),
                0,
                0,
                0,
                0,
                Some(container),
                Some(HMENU((ID_REVOKE_BASE + index as i32) as usize as *mut _)),
                Some(instance),
                None,
            );
            if let Ok(button) = button {
                let _ = SendMessageW(
                    button,
                    WM_SETFONT,
                    Some(WPARAM(
                        self.fonts.borrow().get(FontKind::Message).0 as usize,
                    )),
                    Some(LPARAM(0)),
                );
                made.push(button);
            }
        }
        *self.row_children.borrow_mut() = made;
        self.rows_built.set(self.model.borrow().rows_version);
        self.scroll.set(0);
        self.update_scroll();
        self.place_rows();
        let _ = InvalidateRect(Some(container), None, true);
    }

    /// Content and page heights of the list, in device pixels.
    unsafe fn list_metrics(&self) -> (i32, i32) {
        let count = self.model.borrow().rows.len() as i32;
        let mut rect = RECT::default();
        let _ = GetClientRect(self.rows_hwnd.get(), &mut rect);
        (count * self.px(ROW_HEIGHT), rect.bottom - rect.top)
    }

    /// Shows the scroll bar only when the rows do not fit.
    unsafe fn update_scroll(&self) {
        let container = self.rows_hwnd.get();
        let (content, page) = self.list_metrics();
        let needs = content > page;
        let _ = ShowScrollBar(container, SB_VERT, needs);
        let (content, page) = self.list_metrics();
        let max = (content - page).max(0);
        self.scroll.set(self.scroll.get().clamp(0, max));
        let info = SCROLLINFO {
            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
            nMin: 0,
            nMax: content.max(1) - 1,
            nPage: page.max(0) as u32,
            nPos: self.scroll.get(),
            nTrackPos: 0,
        };
        SetScrollInfo(container, SB_VERT, &info, true);
    }

    /// Moves the row controls to their place for the current scroll offset.
    unsafe fn place_rows(&self) {
        let container = self.rows_hwnd.get();
        let mut client = RECT::default();
        let _ = GetClientRect(container, &mut client);
        let width = client.right - client.left;
        let offset = self.scroll.get();
        let children = self.row_children.borrow().clone();
        let mut next = children.iter();
        let count = self.model.borrow().rows.len();
        for index in 0..count {
            let top = index as i32 * self.px(ROW_HEIGHT) - offset;
            for (left, cell_width) in [(11, 120), (135, 180), (317, 118)] {
                if let Some(cell) = next.next() {
                    let _ = MoveWindow(
                        *cell,
                        self.px(left),
                        top + self.px(13),
                        self.px(cell_width),
                        self.px(18),
                        true,
                    );
                }
            }
            if let Some(button) = next.next() {
                let _ = MoveWindow(
                    *button,
                    width - self.px(87),
                    top + self.px(8),
                    self.px(76),
                    self.px(28),
                    true,
                );
            }
        }
        let _ = InvalidateRect(Some(container), None, true);
    }

    unsafe fn scroll_to(&self, position: i32) {
        let (content, page) = self.list_metrics();
        let max = (content - page).max(0);
        let position = position.clamp(0, max);
        if position != self.scroll.get() {
            self.scroll.set(position);
            self.update_scroll();
            self.place_rows();
        }
    }

    unsafe fn ensure_row_visible(&self, index: usize) {
        let (_, page) = self.list_metrics();
        let top = index as i32 * self.px(ROW_HEIGHT);
        let bottom = top + self.px(ROW_HEIGHT);
        if top < self.scroll.get() {
            self.scroll_to(top);
        } else if bottom > self.scroll.get() + page {
            self.scroll_to(bottom - page);
        }
    }

    unsafe fn on_vscroll(&self, code: i32) {
        let container = self.rows_hwnd.get();
        let (_, page) = self.list_metrics();
        let line = self.px(ROW_HEIGHT) / 2;
        let current = self.scroll.get();
        let target = match code {
            c if c == SB_LINEUP.0 => current - line,
            c if c == SB_LINEDOWN.0 => current + line,
            c if c == SB_PAGEUP.0 => current - page,
            c if c == SB_PAGEDOWN.0 => current + page,
            c if c == SB_TOP.0 => 0,
            c if c == SB_BOTTOM.0 => i32::MAX / 2,
            c if c == SB_THUMBTRACK.0 || c == SB_THUMBPOSITION.0 => {
                let mut info = SCROLLINFO {
                    cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_TRACKPOS,
                    ..Default::default()
                };
                let _ = GetScrollInfo(container, SB_VERT, &mut info);
                info.nTrackPos
            }
            _ => return,
        };
        self.scroll_to(target);
    }

    /// Runs on every command from a button or Enter/Esc.
    unsafe fn on_command(&self, id: i32, notify: u32) {
        if (ID_REVOKE_BASE..ID_REVOKE_BASE + 4096).contains(&id) {
            let index = (id - ID_REVOKE_BASE) as usize;
            if notify == BN_SETFOCUS {
                self.ensure_row_visible(index);
            } else if notify == BN_CLICKED {
                self.ask_revoke(index);
            }
            return;
        }
        if id == IDOK.0 {
            let default = self.model.borrow().default_button();
            if let Some(button) = default {
                self.press(button);
            }
            return;
        }
        if id == IDCANCEL.0 {
            self.escape();
            return;
        }
        if notify != BN_CLICKED {
            return;
        }
        if let Some(ctl) = Ctl::from_id(id) {
            self.press(ctl);
        }
    }

    /// What a button does. The window only sends commands and changes its own page.
    unsafe fn press(&self, button: Ctl) {
        let enabled = self.model.borrow().button_enabled(button);
        if !enabled {
            return;
        }
        match button {
            Ctl::PairButton | Ctl::NewCode => {
                self.model.borrow_mut().clear_status();
                self.shared.send(DevicesCommand::StartPairing);
            }
            Ctl::Cancel | Ctl::Close => {
                let was_open = self.model.borrow_mut().back_to_list();
                if was_open {
                    self.shared.send(DevicesCommand::CancelPairing);
                }
                self.sync();
            }
            Ctl::Allow => {
                let id = self.model.borrow().request.as_ref().map(|r| r.id.clone());
                self.model.borrow_mut().decided = true;
                if let Some(id) = id {
                    self.shared.send(DevicesCommand::Allow(id));
                }
                self.sync();
            }
            Ctl::Deny => {
                let id = self.model.borrow().request.as_ref().map(|r| r.id.clone());
                self.model.borrow_mut().decided = true;
                if let Some(id) = id {
                    self.shared.send(DevicesCommand::Deny(id));
                }
                self.sync();
            }
            _ => {}
        }
    }

    /// Esc: the page's safe exit.
    unsafe fn escape(&self) {
        let page = self.model.borrow().page();
        match page {
            Page::List => {
                let _ = DestroyWindow(self.hwnd.get());
            }
            Page::Request => self.press(Ctl::Deny),
            _ => self.press(Ctl::Cancel),
        }
    }

    /// Closing the window ends an open pairing first.
    unsafe fn on_close(&self) {
        let was_open = self.model.borrow_mut().back_to_list();
        if was_open {
            self.shared.send(DevicesCommand::CancelPairing);
        }
        let _ = DestroyWindow(self.hwnd.get());
    }

    unsafe fn on_timer(&self) {
        let online = self.shared.info.relay_online.load(Ordering::Relaxed);
        {
            let mut model = self.model.borrow_mut();
            model.set_relay_online(online);
            model.tick(SystemTime::now());
        }
        self.sync();
        // Row ages move once a minute; refresh the text in place so focus stays put.
        let minute = now_unix() / 60;
        if self.last_minute.replace(minute) != minute {
            self.refresh_ages();
        }
    }

    unsafe fn refresh_ages(&self) {
        let now = now_unix();
        let children = self.row_children.borrow().clone();
        let count = self.model.borrow().rows.len();
        for index in 0..count {
            let text = self.model.borrow().row_texts(index, now);
            let cell = children.get(index * 4 + 2);
            if let (Some(texts), Some(cell)) = (text, cell) {
                let wide_text = wide(&texts[2]);
                let _ = SetWindowTextW(*cell, PCWSTR(wide_text.as_ptr()));
            }
        }
    }

    unsafe fn ask_revoke(&self, index: usize) {
        let target = {
            let model = self.model.borrow();
            model.rows.get(index).map(|row| RevokeTarget {
                id: row.id.clone(),
                name: row.name.clone(),
                connected: row.connected,
            })
        };
        let Some(target) = target else { return };
        let text = revoke_confirm_text(&target.name, &target.id, target.connected);
        self.model.borrow_mut().revoke = Some(target);
        self.open_confirm(&text);
    }

    unsafe fn open_confirm(&self, text: &str) {
        if !self.confirm.get().is_invalid() {
            return;
        }
        let Ok(instance) = GetModuleHandleW(PCWSTR::null()) else {
            return;
        };
        let instance: HINSTANCE = instance.into();
        let owner = self.hwnd.get();
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: self.px(440),
            bottom: self.px(186),
        };
        let style = WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN;
        let _ = AdjustWindowRectExForDpi(
            &mut frame,
            style,
            false,
            WS_EX_DLGMODALFRAME,
            self.dpi.get(),
        );
        let width = frame.right - frame.left;
        let height = frame.bottom - frame.top;
        let mut owner_rect = RECT::default();
        let _ = GetWindowRect(owner, &mut owner_rect);
        let left = owner_rect.left + (owner_rect.right - owner_rect.left - width) / 2;
        let top = owner_rect.top + (owner_rect.bottom - owner_rect.top - height) / 2;
        let title = wide("Revoke device");
        let Ok(window) = CreateWindowExW(
            WS_EX_DLGMODALFRAME
                | if self.offscreen.get() {
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
                } else {
                    WINDOW_EX_STYLE(0)
                },
            w!("BubblePlayDeviceRevoke"),
            PCWSTR(title.as_ptr()),
            style,
            left,
            top,
            width,
            height,
            Some(owner),
            None,
            Some(instance),
            Some(self as *const Ui as *const _),
        ) else {
            self.model.borrow_mut().revoke = None;
            return;
        };
        let body = wide(text);
        let make = |class: PCWSTR, label: PCWSTR, style: u32, rect: [i32; 4], id: i32| {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                label,
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
                self.px(rect[0]),
                self.px(rect[1]),
                self.px(rect[2]),
                self.px(rect[3]),
                Some(window),
                Some(HMENU(id as usize as *mut _)),
                Some(instance),
                None,
            )
        };
        let font = WPARAM(self.fonts.borrow().get(FontKind::Message).0 as usize);
        for control in [
            make(
                w!("STATIC"),
                PCWSTR(body.as_ptr()),
                SS_NOPREFIX_,
                [20, 20, 400, 84],
                RV_TEXT,
            ),
            make(
                w!("BUTTON"),
                w!("Revoke"),
                BS_PUSHBUTTON_ | WS_TABSTOP.0,
                [212, 126, 96, 30],
                RV_REVOKE,
            ),
            make(
                w!("BUTTON"),
                w!("Cancel"),
                BS_DEFPUSHBUTTON_ | WS_TABSTOP.0,
                [324, 126, 96, 30],
                RV_CANCEL,
            ),
        ]
        .into_iter()
        .flatten()
        {
            let _ = SendMessageW(control, WM_SETFONT, Some(font), Some(LPARAM(1)));
        }
        self.confirm.set(window);
        let _ = EnableWindow(owner, false);
        let _ = ShowWindow(
            window,
            if self.offscreen.get() {
                SW_SHOWNOACTIVATE
            } else {
                SW_SHOW
            },
        );
        if let Ok(cancel) = GetDlgItem(Some(window), RV_CANCEL) {
            let _ = SetFocus(Some(cancel));
        }
    }

    unsafe fn close_confirm(&self) {
        let window = self.confirm.get();
        if window.is_invalid() {
            return;
        }
        self.confirm.set(HWND::default());
        self.model.borrow_mut().revoke = None;
        let owner = self.hwnd.get();
        let _ = EnableWindow(owner, true);
        let _ = DestroyWindow(window);
        let _ = SetForegroundWindow(owner);
    }

    /// The Revoke button of the confirmation: sends the command once and closes.
    unsafe fn confirm_revoke(&self) {
        let id = self.model.borrow().revoke.as_ref().map(|t| t.id.clone());
        self.close_confirm();
        if let Some(id) = id {
            self.model.borrow_mut().clear_status();
            self.shared.send(DevicesCommand::Revoke(id));
            self.sync();
        }
    }

    /// Colours a control for WM_CTLCOLORSTATIC / WM_CTLCOLOREDIT.
    unsafe fn paint_control(&self, dc: HDC, control: HWND) -> LRESULT {
        let id = GetDlgCtrlID(control);
        let look = match self.model.try_borrow() {
            Err(_) => None,
            Ok(model) => {
                if let Some(ctl) = Ctl::from_id(id) {
                    Some(model.look(ctl))
                } else if (ID_CELL_BASE..ID_CELL_BASE + 4096 * 4).contains(&id) {
                    let offset = (id - ID_CELL_BASE) as usize;
                    Some(model.row_look(offset / 4, offset % 4))
                } else {
                    None
                }
            }
        };
        let (_, fg, bg) = look.unwrap_or((FontKind::Message, Fg::Ink, Bg::Milk));
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, self.text_colour(fg));
        LRESULT(self.brush(bg_rgb(bg)).0 as isize)
    }

    unsafe fn draw_qr(&self, item: &DRAWITEMSTRUCT) {
        let width = item.rcItem.right - item.rcItem.left;
        let height = item.rcItem.bottom - item.rcItem.top;
        let white = CreateSolidBrush(colorref(RGB_WHITE));
        let dark = CreateSolidBrush(colorref(RGB_BLACK));
        FillRect(item.hDC, &item.rcItem, white);
        if let Some(matrix) = self.qr_cache.borrow().as_ref() {
            if let Some((scale, offset)) = qr_layout(matrix.size, width.min(height)) {
                for y in 0..matrix.size {
                    for x in 0..matrix.size {
                        if matrix.is_dark(x, y) {
                            let left = item.rcItem.left + offset + x as i32 * scale;
                            let top = item.rcItem.top + offset + y as i32 * scale;
                            let rect = RECT {
                                left,
                                top,
                                right: left + scale,
                                bottom: top + scale,
                            };
                            FillRect(item.hDC, &rect, dark);
                        }
                    }
                }
            }
        }
        let _ = DeleteObject(white.into());
        let _ = DeleteObject(dark.into());
    }

    /// Paints the list background: raised rows, the tinted new row and row separators.
    unsafe fn erase_rows(&self, dc: HDC) {
        let container = self.rows_hwnd.get();
        let mut client = RECT::default();
        let _ = GetClientRect(container, &mut client);
        FillRect(dc, &client, self.brush(RGB_RAISED));
        let Ok(model) = self.model.try_borrow() else {
            return;
        };
        let height = self.px(ROW_HEIGHT);
        for index in 0..model.rows.len() {
            let top = index as i32 * height - self.scroll.get();
            if top + height < 0 || top > client.bottom {
                continue;
            }
            let row = RECT {
                left: client.left,
                top,
                right: client.right,
                bottom: top + height,
            };
            if model.row_is_new(index) {
                FillRect(dc, &row, self.brush(RGB_MOSS));
            }
            let line = RECT {
                top: row.bottom - 1,
                ..row
            };
            FillRect(dc, &line, self.brush(RGB_CLOTH));
        }
    }
}

/// The system message font's LOGFONT (Segoe UI 9 pt) for `dpi`, or a plain fallback.
unsafe fn message_font_logfont(dpi: u32) -> LOGFONTW {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let read = SystemParametersInfoForDpi(
        SPI_GETNONCLIENTMETRICS.0,
        metrics.cbSize,
        Some(std::ptr::addr_of_mut!(metrics).cast::<std::ffi::c_void>()),
        0,
        dpi,
    );
    if read.is_ok() {
        return metrics.lfMessageFont;
    }
    let mut log = LOGFONTW {
        lfHeight: -(12 * dpi as i32 / 96),
        lfCharSet: DEFAULT_CHARSET,
        ..Default::default()
    };
    for (slot, unit) in log.lfFaceName.iter_mut().zip("Segoe UI".encode_utf16()) {
        *slot = unit;
    }
    log
}

unsafe fn ui_from(hwnd: HWND, message: u32, lp: LPARAM) -> *const Ui {
    if message == WM_NCCREATE {
        let create = &*(lp.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Ui
}

unsafe extern "system" fn main_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = ui_from(hwnd, message, lp);
    if !ptr.is_null() {
        let ui = &*ptr;
        match message {
            WM_APP_EVENT => {
                ui.drain_events();
                return LRESULT(0);
            }
            WM_APP_RAISE => {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
                let target = if ui.confirm.get().is_invalid() {
                    hwnd
                } else {
                    ui.confirm.get()
                };
                let _ = SetForegroundWindow(target);
                return LRESULT(0);
            }
            WM_TIMER => {
                ui.on_timer();
                return LRESULT(0);
            }
            WM_COMMAND => {
                ui.on_command((wp.0 & 0xffff) as i32, ((wp.0 >> 16) & 0xffff) as u32);
                return LRESULT(0);
            }
            DM_GETDEFID => {
                let default = ui.model.borrow().default_button();
                if let Some(button) = default {
                    return LRESULT(((DC_HASDEFID as isize) << 16) | button.id() as isize);
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                ui.on_close();
                return LRESULT(0);
            }
            WM_ERASEBKGND => {
                let dc = HDC(wp.0 as *mut _);
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                FillRect(dc, &rect, ui.brush(RGB_MILK));
                return LRESULT(1);
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORBTN => {
                return ui.paint_control(HDC(wp.0 as *mut _), HWND(lp.0 as *mut _));
            }
            WM_DRAWITEM => {
                let item = &*(lp.0 as *const DRAWITEMSTRUCT);
                if wp.0 as i32 == Ctl::Qr.id() {
                    ui.draw_qr(item);
                    return LRESULT(1);
                }
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xffff) as u16 as i16 as i32;
                ui.scroll_to(ui.scroll.get() - delta * ui.px(ROW_HEIGHT) / 120);
                return LRESULT(0);
            }
            WM_DPICHANGED => {
                ui.dpi.set(((wp.0 >> 16) & 0xffff) as u32);
                let suggested = *(lp.0 as *const RECT);
                ui.make_fonts();
                ui.applied_look.borrow_mut().clear();
                ui.resize_window(Some(suggested));
                ui.layout();
                ui.sync();
                return LRESULT(0);
            }
            WM_SYSCOLORCHANGE | WM_SETTINGCHANGE => {
                ui.detect_high_contrast();
                let _ = InvalidateRect(Some(hwnd), None, true);
                return LRESULT(0);
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), TIMER_ID);
                let confirm = ui.confirm.get();
                if !confirm.is_invalid() {
                    ui.confirm.set(HWND::default());
                    let _ = DestroyWindow(confirm);
                }
                {
                    let mut inner = ui.shared.lock();
                    inner.slot = Slot::Closed;
                    inner.queue.clear();
                }
                PostQuitMessage(0);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, message, wp, lp)
}

unsafe extern "system" fn rows_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = ui_from(hwnd, message, lp);
    if !ptr.is_null() {
        let ui = &*ptr;
        match message {
            WM_ERASEBKGND => {
                ui.erase_rows(HDC(wp.0 as *mut _));
                return LRESULT(1);
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                return ui.paint_control(HDC(wp.0 as *mut _), HWND(lp.0 as *mut _));
            }
            WM_COMMAND => {
                ui.on_command((wp.0 & 0xffff) as i32, ((wp.0 >> 16) & 0xffff) as u32);
                return LRESULT(0);
            }
            WM_VSCROLL => {
                ui.on_vscroll((wp.0 & 0xffff) as i32);
                return LRESULT(0);
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xffff) as u16 as i16 as i32;
                ui.scroll_to(ui.scroll.get() - delta * ui.px(ROW_HEIGHT) / 120);
                return LRESULT(0);
            }
            WM_SIZE => {
                ui.update_scroll();
                ui.place_rows();
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, message, wp, lp)
}

unsafe extern "system" fn confirm_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    let ptr = ui_from(hwnd, message, lp);
    if !ptr.is_null() {
        let ui = &*ptr;
        match message {
            WM_COMMAND => {
                let id = (wp.0 & 0xffff) as i32;
                if id == RV_REVOKE {
                    ui.confirm_revoke();
                } else if id == RV_CANCEL || id == IDCANCEL.0 || id == IDOK.0 {
                    ui.close_confirm();
                }
                return LRESULT(0);
            }
            DM_GETDEFID => {
                return LRESULT(((DC_HASDEFID as isize) << 16) | RV_CANCEL as isize);
            }
            WM_CLOSE => {
                ui.close_confirm();
                return LRESULT(0);
            }
            WM_ERASEBKGND => {
                let dc = HDC(wp.0 as *mut _);
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                FillRect(dc, &rect, ui.brush(RGB_MILK));
                return LRESULT(1);
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                let dc = HDC(wp.0 as *mut _);
                let _ = SetBkMode(dc, TRANSPARENT);
                let _ = SetTextColor(dc, ui.text_colour(Fg::Ink));
                return LRESULT(ui.brush(RGB_MILK).0 as isize);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, message, wp, lp)
}

// ---------------------------------------------------------------------------------------------
// Tests: the parts that need no window.
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn info(online: bool) -> DevicesWindowInfo {
        DevicesWindowInfo {
            pc_name: "Studio PC".into(),
            pc_id: "7HQ2-K9MX-4RTD-P0ZB".into(),
            relay_origin: "https://play.example.com".into(),
            relay_online: Arc::new(AtomicBool::new(online)),
        }
    }

    fn row(id: &str, name: &str, seen: Option<&str>, connected: bool) -> DeviceRow {
        DeviceRow {
            id: id.into(),
            name: name.into(),
            last_seen: seen.map(str::to_owned),
            connected,
        }
    }

    fn waiting(expires_in: Duration) -> DevicesEvent {
        DevicesEvent::PairWaiting {
            code_display: "70Q1K-9MXR1".into(),
            qr_uri: "bubbleplay://pair?relay=https%3A%2F%2Fexample.test&pc=A&code=B".into(),
            pc_id: "7HQ2-K9MX-4RTD-P0ZB".into(),
            expires_at: SystemTime::now() + expires_in,
        }
    }

    fn fixture_uri() -> String {
        let text = include_str!("../../fixtures/device-keys-contract.json");
        let value: serde_json::Value = serde_json::from_str(text).expect("fixture parses");
        value["phase2"]["pairing"]["qr"]["android"]
            .as_str()
            .expect("qr fixture")
            .to_owned()
    }

    #[test]
    fn countdown_reads_minutes_and_rounds_up() {
        assert_eq!(format_countdown(Duration::from_secs(272)), "4:32");
        assert_eq!(format_countdown(Duration::from_secs(300)), "5:00");
        assert_eq!(format_countdown(Duration::from_millis(900)), "0:01");
        assert_eq!(format_countdown(Duration::from_secs(61)), "1:01");
        assert_eq!(format_countdown(Duration::ZERO), "0:00");
    }

    #[test]
    fn rfc3339_parses_zones_and_rejects_junk() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(
            parse_rfc3339("2000-03-01T02:00:00+02:00"),
            Some(951_868_800)
        );
        assert_eq!(
            parse_rfc3339("2000-02-29T22:00:00.123-02:00"),
            Some(951_868_800)
        );
        for bad in ["", "yesterday", "2026-13-01T00:00:00Z", "2026-10-27 22:14"] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn last_seen_says_connected_or_how_long_ago() {
        let now = parse_rfc3339("2026-10-28T12:00:00Z").unwrap();
        let ago = |text: &str| last_seen_text(&row("A", "n", Some(text), false), now);
        assert_eq!(
            last_seen_text(&row("A", "n", None, true), now),
            "now \u{b7} connected"
        );
        assert_eq!(last_seen_text(&row("A", "n", None, false), now), "Never");
        assert_eq!(ago("2026-10-28T11:59:40Z"), "just now");
        assert_eq!(ago("2026-10-28T11:15:00Z"), "45 min ago");
        assert_eq!(ago("2026-10-28T07:00:00Z"), "5 h ago");
        assert_eq!(ago("2026-10-27T11:00:00Z"), "yesterday");
        assert_eq!(ago("2026-09-24T12:00:00Z"), "34 days ago");
        assert_eq!(ago("not a time"), "Unknown");
    }

    #[test]
    fn revoke_text_names_the_device_and_id() {
        let text = revoke_confirm_text("Pixel 8", "B7QK-2M9X-H4PD-R8TN", true);
        assert!(text.starts_with("Revoke Pixel 8 (B7QK-2M9X-H4PD-R8TN)? It is disconnected now"));
        assert!(revoke_confirm_text("", "X", false).starts_with("Revoke Unnamed device (X)?"));
        assert!(!revoke_confirm_text("n", "X", false).contains("disconnected now"));
    }

    #[test]
    fn relay_host_drops_the_scheme() {
        assert_eq!(relay_host("https://play.example.com"), "play.example.com");
        assert_eq!(relay_host("http://127.0.0.1:8787"), "127.0.0.1:8787");
        assert_eq!(relay_host("play.example.com"), "play.example.com");
    }

    fn finder_ok(matrix: &QrMatrix, left: usize, top: usize) -> bool {
        (0..7).all(|y| {
            (0..7).all(|x| {
                let ring = x == 0 || x == 6 || y == 0 || y == 6;
                let core = (2..=4).contains(&x) && (2..=4).contains(&y);
                matrix.is_dark(left + x, top + y) == (ring || core)
            })
        })
    }

    #[test]
    fn qr_matrix_for_the_fixture_uri_has_a_valid_version_and_fits_the_box() {
        let uri = fixture_uri();
        let matrix = qr_matrix(&uri).expect("the fixture URI encodes");
        assert!(matrix.version >= 1, "a normal QR version");
        assert_eq!(matrix.size, 17 + 4 * matrix.version as usize);
        let last = matrix.size - 7;
        assert!(finder_ok(&matrix, 0, 0), "top-left finder");
        assert!(finder_ok(&matrix, last, 0), "top-right finder");
        assert!(finder_ok(&matrix, 0, last), "bottom-left finder");
        // The 196 px box leaves at least 3 px per module, which scanners still read.
        let (scale, offset) = qr_layout(matrix.size, 196).expect("fits 196 px");
        assert!(scale >= 3, "scale {scale}");
        assert!(offset >= 4 * scale, "quiet zone of 4 modules");
        assert!(offset + scale * matrix.size as i32 <= 196);
    }

    #[test]
    fn qr_layout_scales_to_whole_pixels_and_refuses_when_too_small() {
        assert_eq!(qr_layout(21, 196), Some((6, 35)));
        assert_eq!(qr_layout(21, 28), None);
        assert_eq!(qr_layout(21, 29), Some((1, 4)));
    }

    #[test]
    fn list_page_shows_rows_or_the_empty_text() {
        let mut model = Model::new(&info(true));
        assert!(model.visible().contains(&Ctl::Empty));
        assert!(!model.visible().contains(&Ctl::Status));
        model.apply(DevicesEvent::List(vec![row("A", "Pixel 8", None, true)]));
        assert!(!model.visible().contains(&Ctl::Empty));
        assert_eq!(model.default_button(), Some(Ctl::PairButton));
    }

    #[test]
    fn offline_relay_disables_pairing_and_says_why() {
        let mut model = Model::new(&info(false));
        assert!(!model.pair_enabled());
        assert!(!model.button_enabled(Ctl::PairButton));
        assert_eq!(model.default_button(), None);
        assert_eq!(
            model.status_line().map(|line| line.0),
            Some("Pairing needs the relay, which is offline.".to_owned())
        );
        assert_eq!(
            model.text(Ctl::RelayWord, SystemTime::now()).as_deref(),
            Some("offline")
        );
        model.set_relay_online(true);
        assert!(model.status_line().is_none());
        assert_eq!(model.look(Ctl::RelayWord).1, Fg::Apricot);
    }

    #[test]
    fn each_page_shows_exactly_its_controls() {
        let mut model = Model::new(&info(true));
        model.apply(waiting(Duration::from_secs(300)));
        assert_eq!(model.page(), Page::Waiting);
        let shown = model.visible();
        for ctl in [
            Ctl::Qr,
            Ctl::Code,
            Ctl::Countdown,
            Ctl::Cancel,
            Ctl::PairPcId,
        ] {
            assert!(shown.contains(&ctl), "{ctl:?}");
        }
        for ctl in [Ctl::PairButton, Ctl::Allow, Ctl::NewCode, Ctl::Band] {
            assert!(!shown.contains(&ctl), "{ctl:?}");
        }
        assert_eq!(model.default_button(), Some(Ctl::Cancel));

        model.apply(DevicesEvent::PairExpired);
        assert_eq!(model.page(), Page::Expired);
        assert!(model.visible().contains(&Ctl::NewCode));
        assert!(model.visible().contains(&Ctl::Close));
        assert_eq!(model.default_button(), Some(Ctl::NewCode));
        assert_eq!(model.look(Ctl::Message).1, Fg::Ink);

        model.apply(waiting(Duration::from_secs(300)));
        model.apply(DevicesEvent::PairStopped);
        assert_eq!(model.page(), Page::Stopped);
        assert!(!model.visible().contains(&Ctl::Close));
        assert_eq!(model.look(Ctl::Message).1, Fg::Error);
    }

    #[test]
    fn pair_request_makes_deny_the_default_and_shows_the_number() {
        let mut model = Model::new(&info(true));
        model.apply(waiting(Duration::from_secs(300)));
        model.apply(DevicesEvent::PairRequest {
            id: "B7QK-2M9X-H4PD-R8TN".into(),
            name: String::new(),
            sas_display: "482 913".into(),
        });
        assert_eq!(model.page(), Page::Request);
        assert_eq!(model.default_button(), Some(Ctl::Deny));
        assert_eq!(model.first_focus(), Some(Ctl::Deny));
        let now = SystemTime::now();
        assert_eq!(model.text(Ctl::Number, now).as_deref(), Some("482 913"));
        assert_eq!(
            model.text(Ctl::DeviceName, now).as_deref(),
            Some("Unnamed device")
        );
        let shown = model.visible();
        assert!(shown.contains(&Ctl::Allow) && shown.contains(&Ctl::Deny));
        assert_eq!(model.look(Ctl::Number).0, FontKind::Mono72);
        model.decided = true;
        assert!(!model.button_enabled(Ctl::Allow));
        assert!(!model.button_enabled(Ctl::Deny));
    }

    #[test]
    fn done_and_denied_return_to_the_list_with_a_status() {
        let mut model = Model::new(&info(true));
        model.apply(DevicesEvent::List(vec![row("B7QK", "Pixel 8", None, true)]));
        model.apply(waiting(Duration::from_secs(300)));
        model.apply(DevicesEvent::PairDone {
            id: "B7QK".into(),
            name: "Pixel 8".into(),
        });
        assert_eq!(model.page(), Page::List);
        assert_eq!(
            model.status_line(),
            Some(("Pixel 8 is paired.".to_owned(), StatusKind::Ok))
        );
        assert!(model.row_is_new(0));
        assert_eq!(model.row_texts(0, 0).unwrap()[0], "Pixel 8 (new)");
        assert_eq!(model.row_look(0, 0).2, Bg::Moss);
        assert_eq!(model.row_look(0, 1).0, FontKind::Mono12);

        model.apply(waiting(Duration::from_secs(300)));
        model.apply(DevicesEvent::PairDenied);
        assert_eq!(model.page(), Page::List);
        assert_eq!(
            model.status_line().map(|line| line.0),
            Some("Request denied. The device was not added.".to_owned())
        );
        assert!(!model.row_is_new(0), "a new pairing clears the marker");
    }

    #[test]
    fn save_error_shows_the_fixed_sentence_not_the_raw_error() {
        let mut model = Model::new(&info(true));
        model.apply(DevicesEvent::SaveError(r"C:\secret\path: disk full".into()));
        let (text, kind) = model.status_line().expect("status");
        assert_eq!(kind, StatusKind::Error);
        assert!(text.contains("Nothing changed") && !text.contains("secret"));
        assert_eq!(model.look(Ctl::Status).1, Fg::Error);
    }

    #[test]
    fn stale_stop_after_cancel_is_ignored() {
        let mut model = Model::new(&info(true));
        model.apply(waiting(Duration::from_secs(300)));
        assert!(model.back_to_list(), "an open pairing must be cancelled");
        model.apply(DevicesEvent::PairStopped);
        assert_eq!(model.page(), Page::List);
        assert!(!model.back_to_list(), "nothing left to cancel");
    }

    #[test]
    fn countdown_runs_out_into_the_expired_page() {
        let mut model = Model::new(&info(true));
        model.apply(waiting(Duration::from_secs(272)));
        let now = SystemTime::now();
        let text = model.countdown_text(now);
        assert!(
            text == "Expires in 4:32" || text == "Expires in 4:33",
            "{text}"
        );
        assert!(!model.tick(now));
        assert!(model.tick(now + Duration::from_secs(301)));
        assert_eq!(model.page(), Page::Expired);
        assert!(!model.tick(now + Duration::from_secs(400)), "only once");
    }

    #[test]
    fn debug_free_secret_handling_keeps_the_code_out_of_list_pages() {
        let mut model = Model::new(&info(true));
        model.apply(waiting(Duration::from_secs(300)));
        let now = SystemTime::now();
        assert_eq!(model.text(Ctl::Code, now).as_deref(), Some("70Q1K-9MXR1"));
        model.apply(DevicesEvent::PairDenied);
        assert!(model.text(Ctl::Code, now).is_none());
        assert!(model.pair.is_none() && model.request.is_none());
    }

    #[test]
    fn list_event_drops_a_revoke_target_that_vanished() {
        let mut model = Model::new(&info(true));
        model.apply(DevicesEvent::List(vec![row("A", "n", None, false)]));
        model.revoke = Some(RevokeTarget {
            id: "A".into(),
            name: "n".into(),
            connected: false,
        });
        model.apply(DevicesEvent::List(vec![row("A", "n", None, false)]));
        assert!(model.revoke.is_some());
        model.apply(DevicesEvent::List(Vec::new()));
        assert!(model.revoke.is_none());
    }

    #[test]
    fn control_ids_round_trip() {
        for ctl in ALL_CTLS {
            assert_eq!(Ctl::from_id(ctl.id()), Some(ctl));
        }
        assert_eq!(Ctl::from_id(1), None);
        let ids: std::collections::HashSet<i32> = ALL_CTLS.iter().map(|c| c.id()).collect();
        assert_eq!(ids.len(), ALL_CTLS.len());
        assert!(ALL_CTLS.iter().all(|c| c.id() < ID_REVOKE_BASE));
    }

    #[test]
    fn every_control_has_a_place_on_every_page() {
        for page in [
            Page::List,
            Page::Waiting,
            Page::Expired,
            Page::Stopped,
            Page::Failed,
            Page::Request,
        ] {
            for ctl in ALL_CTLS {
                let [x, y, width, height] = place(ctl, page);
                assert!(x >= 0 && y >= 0 && width > 0 && height > 0, "{ctl:?}");
                assert!(x + width <= CLIENT_W && y + height <= CLIENT_H, "{ctl:?}");
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Screenshot harness: `devices_window_screenshots`, run through win-check with
    // `-- --ignored` and BP_SCREENSHOT_DIR set. The windows are created far outside every
    // monitor with tool-window and no-activate styles, are never activated, and are captured
    // with PrintWindow(PW_RENDERFULLCONTENT) into memory bitmaps. That copies what DWM composes,
    // so sibling clipping shows up as it does on screen (WM_PRINT painted every child in turn
    // and hid z-order bugs).
    // -----------------------------------------------------------------------------------------

    #[link(name = "user32")]
    extern "system" {
        fn PrintWindow(hwnd: HWND, hdc: HDC, flags: u32) -> i32;
    }

    const PW_RENDERFULLCONTENT: u32 = 0x2;

    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, RedrawWindow, BITMAPINFO, BITMAPINFOHEADER,
        BI_RGB, DIB_RGB_COLORS, RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE,
        RDW_UPDATENOW,
    };

    fn rfc3339_ago(secs: i64) -> String {
        let total = now_unix() - secs;
        let days = total.div_euclid(86_400);
        let rest = total.rem_euclid(86_400);
        // Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            rest / 3600,
            rest % 3600 / 60,
            rest % 60
        )
    }

    /// Writes a 32-bit BMP of `window` by asking it to paint itself into a memory bitmap.
    unsafe fn capture(window: HWND, path: &std::path::Path) {
        let mut rect = RECT::default();
        GetWindowRect(window, &mut rect).expect("window rect");
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let header = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let info = BITMAPINFO {
            bmiHeader: header,
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            .expect("dib section");
        let old = SelectObject(dc, HGDIOBJ(bitmap.0));
        // Repaint everything now and let DWM compose a frame, so its copy is current.
        let _ = RedrawWindow(
            Some(window),
            None,
            None,
            RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW | RDW_FRAME,
        );
        settle();
        let _ = windows::Win32::Graphics::Dwm::DwmFlush();
        assert!(
            PrintWindow(window, dc, PW_RENDERFULLCONTENT) != 0,
            "PrintWindow failed"
        );
        let size = (width * height * 4) as usize;
        let pixels = std::slice::from_raw_parts(bits as *const u8, size).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        let mut file = Vec::with_capacity(54 + size);
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&((54 + size) as u32).to_le_bytes());
        file.extend_from_slice(&[0; 4]);
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&width.to_le_bytes());
        file.extend_from_slice(&height.to_le_bytes()); // positive: rows are written bottom-up
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&[0; 24]);
        for row in pixels.chunks(width as usize * 4).rev() {
            file.extend_from_slice(row);
        }
        std::fs::write(path, file).expect("write bmp");
    }

    unsafe fn settle() {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    /// Stacking layer of a main-window child: a child may only sit above an overlapping sibling
    /// of the same or a lower layer.
    fn z_layer(id: i32) -> u8 {
        match Ctl::from_id(id) {
            Some(Ctl::Band | Ctl::ListFrame) => 0,
            Some(Ctl::LvHead) => 1,
            _ if id == ROWS_ID => 2,
            _ => 3,
        }
    }

    /// Every overlapping pair of children, on every page, stacks by layer. With
    /// WS_CLIPSIBLINGS a background above a label hides the label on screen.
    #[test]
    fn background_controls_sit_below_what_they_overlap() {
        let (commands, _unused) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            info: info(true),
            commands,
            inner: Mutex::new(Inner {
                slot: Slot::Closed,
                queue: VecDeque::new(),
            }),
        });
        let rows = vec![row("B7QK-2M9X-H4PD-R8TN", "Pixel 8", None, true)];
        let request = || DevicesEvent::PairRequest {
            id: "B7QK-2M9X-H4PD-R8TN".into(),
            name: "Pixel 8".into(),
            sas_display: "482 913".into(),
        };
        let wait = || waiting(Duration::from_secs(272));
        let pages: Vec<(Page, Vec<DevicesEvent>)> = vec![
            (Page::List, vec![DevicesEvent::List(rows.clone())]),
            (Page::List, vec![DevicesEvent::List(vec![])]),
            (Page::Waiting, vec![wait()]),
            (Page::Expired, vec![wait(), DevicesEvent::PairExpired]),
            (Page::Stopped, vec![wait(), DevicesEvent::PairStopped]),
            (Page::Failed, vec![wait(), DevicesEvent::PairFailed]),
            (Page::Request, vec![wait(), request()]),
        ];
        // SAFETY: the window is created, never shown, inspected and destroyed on this thread.
        unsafe {
            let ui = Ui::new(Arc::clone(&shared));
            ui.offscreen.set(true);
            let window = ui.create().expect("window");
            for (page, events) in pages {
                for event in events {
                    ui.model.borrow_mut().apply(event);
                }
                ui.sync();
                settle();
                assert_eq!(ui.model.borrow().page(), page);
                // GW_CHILD is the top of the sibling z-order; GW_HWNDNEXT walks down.
                let mut stack: Vec<(i32, RECT)> = Vec::new();
                let mut child = GetWindow(window, GW_CHILD).ok();
                while let Some(handle) = child {
                    let mut rect = RECT::default();
                    GetWindowRect(handle, &mut rect).expect("child rect");
                    stack.push((GetDlgCtrlID(handle), rect));
                    child = GetWindow(handle, GW_HWNDNEXT).ok();
                }
                assert_eq!(stack.len(), ALL_CTLS.len() + 1, "{page:?}");
                for (above, (upper, a)) in stack.iter().enumerate() {
                    for (lower, b) in &stack[above + 1..] {
                        let overlap = a.left < b.right
                            && b.left < a.right
                            && a.top < b.bottom
                            && b.top < a.bottom;
                        assert!(
                            !overlap || z_layer(*upper) >= z_layer(*lower),
                            "{page:?}: {:?} ({upper}) is above {:?} ({lower}) and covers it",
                            Ctl::from_id(*upper),
                            Ctl::from_id(*lower),
                        );
                    }
                }
            }
            let _ = DestroyWindow(window);
            settle();
            ui.release_resources();
        }
    }

    #[test]
    #[ignore = "renders off-screen; run through win-check with BP_SCREENSHOT_DIR"]
    fn devices_window_screenshots() {
        let dir = std::path::PathBuf::from(
            std::env::var("BP_SCREENSHOT_DIR").expect("BP_SCREENSHOT_DIR is set"),
        );
        std::fs::create_dir_all(&dir).expect("screenshot dir");
        let (commands, _unused) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            info: info(true),
            commands,
            inner: Mutex::new(Inner {
                slot: Slot::Closed,
                queue: VecDeque::new(),
            }),
        });
        let uri = fixture_uri();
        let rows = || {
            vec![
                row("B7QK-2M9X-H4PD-R8TN", "Pixel 8", None, true),
                row(
                    "3FWD-8NCH-QX2J-V6LA",
                    "Work laptop",
                    Some(&rfc3339_ago(26 * 3600)),
                    false,
                ),
                row(
                    "M2ZT-5PRE-9KGY-C4HW",
                    "",
                    Some(&rfc3339_ago(34 * 86_400)),
                    false,
                ),
            ]
        };
        let waiting = || DevicesEvent::PairWaiting {
            code_display: "70Q1K-9MXR1".into(),
            qr_uri: uri.clone(),
            pc_id: "7HQ2-K9MX-4RTD-P0ZB".into(),
            expires_at: SystemTime::now() + Duration::from_secs(272),
        };
        let request = |name: &str| DevicesEvent::PairRequest {
            id: "B7QK-2M9X-H4PD-R8TN".into(),
            name: name.into(),
            sas_display: "482 913".into(),
        };
        // SAFETY: the whole harness runs on this one thread, which owns every window it makes.
        unsafe {
            let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let ui = Ui::new(Arc::clone(&shared));
            ui.offscreen.set(true);
            let window = ui.create().expect("window");
            let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
            let shot = |ui: &Ui, name: &str, events: Vec<DevicesEvent>| {
                for event in events {
                    ui.model.borrow_mut().apply(event);
                }
                ui.sync();
                settle();
                capture(ui.hwnd.get(), &dir.join(format!("{name}.bmp")));
            };
            shot(&ui, "01-list-online", vec![DevicesEvent::List(rows())]);
            ui.model.borrow_mut().set_relay_online(false);
            shot(&ui, "02-list-offline", vec![]);
            ui.model.borrow_mut().set_relay_online(true);
            shot(&ui, "03-list-empty", vec![DevicesEvent::List(vec![])]);
            shot(&ui, "04-revoke-list", vec![DevicesEvent::List(rows())]);
            ui.ask_revoke(0);
            settle();
            capture(ui.confirm.get(), &dir.join("04b-revoke-dialog.bmp"));
            ui.close_confirm();
            settle();
            shot(&ui, "05-pair-waiting", vec![waiting()]);
            shot(&ui, "06-pair-expired", vec![DevicesEvent::PairExpired]);
            shot(
                &ui,
                "07-pair-stopped",
                vec![waiting(), DevicesEvent::PairStopped],
            );
            shot(&ui, "08-pair-request", vec![waiting(), request("Pixel 8")]);
            shot(
                &ui,
                "09-pair-done",
                vec![DevicesEvent::PairDone {
                    id: "B7QK-2M9X-H4PD-R8TN".into(),
                    name: "Pixel 8".into(),
                }],
            );
            shot(
                &ui,
                "10-pair-denied",
                vec![waiting(), request("Pixel 8"), DevicesEvent::PairDenied],
            );
            shot(
                &ui,
                "11-save-error",
                vec![DevicesEvent::SaveError("disk full".into())],
            );
            shot(&ui, "12-pair-request-unnamed", vec![waiting(), request("")]);
            shot(
                &ui,
                "13-pair-failed",
                vec![waiting(), DevicesEvent::PairFailed],
            );
            let _ = DestroyWindow(window);
            settle();
            ui.release_resources();
        }
    }
}
