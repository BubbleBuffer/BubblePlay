//! "Report a problem" without the app: a small Win32 window on its own thread (multiline box,
//! kind list, Send and Cancel) that stores a spec-4 report with `FeedbackStore::save_local`.
//! The tray never waits on it. The report building and the log readers are plain functions
//! with unit tests; the window itself is exercised by compile only.

use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::SystemTime;

use bubbleplay_host::feedback_store::{
    FeedbackStore, MAX_ERROR_ENTRIES, MAX_LOG_ENTRIES, MAX_TEXT_BYTES,
};
use bubbleplay_host::logging;
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetSysColor, GetSysColorBrush, SetBkMode, SetTextColor, COLOR_WINDOW,
    COLOR_WINDOWTEXT, HBRUSH, HDC, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::EM_SETLIMITTEXT;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::key_ui::{child, drain_quit, message_font, pump_until_quit, set_child_font};
use super::platform::wide;

/// Log sources whose files go into a report (`<src>-YYYY-MM-DD[.N].jsonl` in the logs folder).
const SOURCES: [&str; 2] = ["tray", "host"];
/// How many recent lines per source are scanned for the last error lines.
const ERROR_WINDOW: usize = 2000;
/// Most bytes read from the end of one log file.
const TAIL_BYTES: u64 = 2 * 1024 * 1024;
/// Characters the box accepts; at most 3 bytes each, so the text always fits `MAX_TEXT_BYTES`.
const BOX_CHARS: usize = MAX_TEXT_BYTES / 4;

const TEXT_FIELD: i32 = 301;
const KIND_LIST: i32 = 302;
const LOGGED_NOTE: &str = "Saved to the logs folder.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bug,
    Complaint,
    Idea,
    Other,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Bug, Kind::Complaint, Kind::Idea, Kind::Other];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Bug => "Bug",
            Kind::Complaint => "Complaint",
            Kind::Idea => "Idea",
            Kind::Other => "Other",
        }
    }

    /// The `kind` value of a report.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Bug => "bug",
            Kind::Complaint => "complaint",
            Kind::Idea => "idea",
            Kind::Other => "other",
        }
    }

    pub fn from_index(index: usize) -> Kind {
        Kind::ALL.get(index).copied().unwrap_or(Kind::Bug)
    }
}

/// The balloon text once a report is stored.
pub const SAVED_BALLOON: &str = LOGGED_NOTE;

// ----------------------------------------------------------------- pure logic

/// The last `n` non-empty lines of a file, reading at most `TAIL_BYTES` from its end.
pub fn read_last_lines(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if start > 0 && !lines.is_empty() {
        // The cut may fall inside a line.
        lines.remove(0);
    }
    let lines: Vec<String> = lines
        .into_iter()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    let skip = lines.len().saturating_sub(n);
    Ok(lines.into_iter().skip(skip).collect())
}

/// Log files of one source, newest first.
fn source_files(dir: &Path, src: &str) -> Vec<PathBuf> {
    let prefix = format!("{src}-");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(&prefix) && name.ends_with(".jsonl"))
        .collect();
    names.sort_unstable_by(|a, b| b.cmp(a));
    names.into_iter().map(|name| dir.join(name)).collect()
}

/// The last `n` JSON lines of one source across its day files; unreadable or non-JSON lines are skipped.
pub fn last_source_lines(dir: &Path, src: &str, n: usize) -> Vec<Value> {
    let mut chunks: Vec<Vec<Value>> = Vec::new();
    let mut have = 0usize;
    for path in source_files(dir, src) {
        if have >= n {
            break;
        }
        let Ok(lines) = read_last_lines(&path, n - have) else {
            continue;
        };
        let values: Vec<Value> = lines
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(Value::is_object)
            .collect();
        have += values.len();
        chunks.push(values);
    }
    // Chunks run newest file first; each holds its lines oldest first.
    chunks.into_iter().rev().flatten().collect()
}

fn ts_of(line: &Value) -> &str {
    line.get("ts").and_then(Value::as_str).unwrap_or("")
}

/// The last `n` lines of the tray and host logs merged by timestamp.
pub fn recent_lines(dir: &Path, n: usize) -> Vec<Value> {
    let mut all: Vec<Value> = SOURCES
        .iter()
        .flat_map(|src| last_source_lines(dir, src, n))
        .collect();
    all.sort_by(|a, b| ts_of(a).cmp(ts_of(b)));
    let skip = all.len().saturating_sub(n);
    all.split_off(skip)
}

/// Splits recent lines into the report's `logs` (last 200) and `errors` (last 20 error lines).
pub fn split_logs(lines: Vec<Value>) -> (Vec<Value>, Vec<Value>) {
    let mut errors: Vec<Value> = lines
        .iter()
        .filter(|line| line.get("level").and_then(Value::as_str) == Some("error"))
        .cloned()
        .collect();
    let skip = errors.len().saturating_sub(MAX_ERROR_ENTRIES);
    let errors = errors.split_off(skip);
    let mut logs = lines;
    let skip = logs.len().saturating_sub(MAX_LOG_ENTRIES);
    let logs = logs.split_off(skip);
    (logs, errors)
}

/// Cut `text` to at most `max` bytes on a character boundary.
pub fn clip_text(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A random version 4 UUID in text form.
pub fn new_id() -> String {
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].concat(),
        h[4..6].concat(),
        h[6..8].concat(),
        h[8..10].concat(),
        h[10..16].concat()
    )
}

/// Everything a report needs besides the logs.
pub struct ReportInput<'a> {
    pub id: String,
    pub created: String,
    pub text: &'a str,
    pub kind: Kind,
    pub version: &'a str,
    pub commit: &'a str,
    pub device_name: &'a str,
}

/// Builds a spec-4 report for a report made on this PC. `lines` are recent log lines, oldest first.
pub fn build_report(input: &ReportInput<'_>, lines: Vec<Value>) -> Value {
    let (logs, errors) = split_logs(lines);
    json!({
        "version": 1,
        "id": input.id,
        "created": input.created,
        "text": clip_text(input.text.trim(), MAX_TEXT_BYTES),
        "kind": input.kind.as_str(),
        "app": {
            "version": input.version,
            "build": input.version,
            "commit": input.commit,
        },
        "platform": "windows",
        "device": { "id": "this-pc", "name": input.device_name, "model": "" },
        "pc": null,
        "session": null,
        "route": "tray",
        "locale": "und",
        "logs": logs,
        "errors": errors,
    })
}

/// Reads the logs, builds the report and stores it. Returns the stored path.
pub fn save_report(
    logs_dir: &Path,
    text: &str,
    kind: Kind,
    now: SystemTime,
) -> Result<PathBuf, String> {
    let lines = recent_lines(logs_dir, ERROR_WINDOW);
    let version = env!("CARGO_PKG_VERSION");
    let commit = option_env!("BUBBLEPLAY_COMMIT").unwrap_or("unknown");
    let device_name = std::env::var("COMPUTERNAME").unwrap_or_default();
    let (created, _) = logging::timestamp(now);
    let report = build_report(
        &ReportInput {
            id: new_id(),
            created,
            text,
            kind,
            version,
            commit,
            device_name: &device_name,
        },
        lines,
    );
    let store = FeedbackStore::open(logs_dir).map_err(|e| e.to_string())?;
    store.save_local(report, None).map_err(|e| format!("{e:?}"))
}

// -------------------------------------------------------------------- window

static RUNNING: AtomicBool = AtomicBool::new(false);

struct Running;

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

struct FeedbackWindow {
    result: RefCell<Option<(String, Kind)>>,
}

/// Opens the report window on a new thread. `done` gets `true` once a report was stored.
/// Returns `Ok(false)` when a report window is already open.
pub fn start(logs_dir: PathBuf, done: impl FnOnce(bool) + Send + 'static) -> std::io::Result<bool> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Ok(false);
    }
    let spawned = std::thread::Builder::new()
        .name("feedback-ui".to_owned())
        .spawn(move || {
            let _running = Running;
            let saved = match ask() {
                Some((text, kind)) => {
                    match save_report(&logs_dir, &text, kind, SystemTime::now()) {
                        Ok(path) => {
                            tracing::info!(path = %path.display(), "report stored from the tray");
                            true
                        }
                        Err(error) => {
                            tracing::warn!(%error, "report could not be stored");
                            show_error("The report could not be saved.");
                            false
                        }
                    }
                }
                None => false,
            };
            drop(_running);
            done(saved);
        });
    match spawned {
        Ok(_) => Ok(true),
        Err(error) => {
            RUNNING.store(false, Ordering::SeqCst);
            Err(error)
        }
    }
}

fn show_error(text: &str) {
    let title = wide("Report a problem");
    let text = wide(text);
    // SAFETY: both buffers are null-terminated and outlive the synchronous call.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR | MB_TOPMOST,
        );
    }
}

fn scaled(value: i32, dpi: u32) -> i32 {
    (i64::from(value) * i64::from(dpi) / 96) as i32
}

/// Shows the window and returns the text and kind on Send; `None` on Cancel, Escape or close.
fn ask() -> Option<(String, Kind)> {
    static CLASS: OnceLock<bool> = OnceLock::new();
    let window = FeedbackWindow {
        result: RefCell::new(None),
    };
    let title = wide("Report a problem");
    // SAFETY: the window and its controls are created, pumped and destroyed on this thread.
    // `window` outlives the message loop and is reached only through the window's user data.
    unsafe {
        let dpi = GetDpiForSystem().max(96);
        let instance: HINSTANCE = GetModuleHandleW(PCWSTR::null()).ok()?.into();
        let class = w!("BubblePlayFeedbackFallback");
        if !*CLASS.get_or_init(|| {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(feedback_proc),
                hInstance: instance,
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
                ..Default::default()
            }) != 0
        }) {
            return None;
        }
        let style = WS_CAPTION | WS_SYSMENU;
        let ex_style = WS_EX_TOPMOST | WS_EX_DLGMODALFRAME;
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: scaled(480, dpi),
            bottom: scaled(260, dpi),
        };
        let _ = AdjustWindowRectEx(&mut frame, style, false, ex_style);
        let width = frame.right - frame.left;
        let height = frame.bottom - frame.top;
        let left = (GetSystemMetrics(SM_CXSCREEN) - width) / 2;
        let top = (GetSystemMetrics(SM_CYSCREEN) - height) / 2;
        let hwnd = CreateWindowExW(
            ex_style,
            class,
            PCWSTR(title.as_ptr()),
            style,
            left,
            top,
            width,
            height,
            None,
            None,
            Some(instance),
            Some(&window as *const FeedbackWindow as *const _),
        )
        .ok()?;
        let focus = match create_controls(hwnd, instance, dpi) {
            Ok(field) => field,
            Err(_) => {
                let _ = DestroyWindow(hwnd);
                drain_quit();
                return None;
            }
        };
        let font = message_font();
        let _ = EnumChildWindows(Some(hwnd), Some(set_child_font), LPARAM(font.0 as isize));
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(focus));
        pump_until_quit(hwnd);
        let _ = DeleteObject(font.into());
        window.result.take()
    }
}

/// Creates the label, text box, kind list and buttons. Returns the text box.
unsafe fn create_controls(
    hwnd: HWND,
    instance: HINSTANCE,
    dpi: u32,
) -> windows::core::Result<HWND> {
    let rect = |x, y, w, h| {
        [
            scaled(x, dpi),
            scaled(y, dpi),
            scaled(w, dpi),
            scaled(h, dpi),
        ]
    };
    child(
        hwnd,
        instance,
        w!("STATIC"),
        &wide("What happened?"),
        0,
        rect(16, 12, 448, 18),
        0,
    )?;
    let edit_style = ES_MULTILINE as u32
        | ES_AUTOVSCROLL as u32
        | ES_WANTRETURN as u32
        | WS_VSCROLL.0
        | WS_BORDER.0
        | WS_TABSTOP.0;
    let field = child(
        hwnd,
        instance,
        w!("EDIT"),
        &wide(""),
        edit_style,
        rect(16, 34, 448, 150),
        TEXT_FIELD,
    )?;
    let _ = SendMessageW(
        field,
        EM_SETLIMITTEXT,
        Some(WPARAM(BOX_CHARS)),
        Some(LPARAM(0)),
    );
    child(
        hwnd,
        instance,
        w!("STATIC"),
        &wide("Kind"),
        0,
        rect(16, 198, 120, 18),
        0,
    )?;
    let list = child(
        hwnd,
        instance,
        w!("COMBOBOX"),
        &wide(""),
        CBS_DROPDOWNLIST as u32 | WS_TABSTOP.0 | WS_VSCROLL.0,
        rect(16, 218, 160, 200),
        KIND_LIST,
    )?;
    for kind in Kind::ALL {
        let label = wide(kind.label());
        let _ = SendMessageW(
            list,
            CB_ADDSTRING,
            Some(WPARAM(0)),
            Some(LPARAM(label.as_ptr() as isize)),
        );
    }
    let _ = SendMessageW(list, CB_SETCURSEL, Some(WPARAM(0)), Some(LPARAM(0)));
    child(
        hwnd,
        instance,
        w!("BUTTON"),
        &wide("Send"),
        (BS_DEFPUSHBUTTON as u32) | WS_TABSTOP.0,
        rect(264, 218, 96, 30),
        IDOK.0,
    )?;
    child(
        hwnd,
        instance,
        w!("BUTTON"),
        &wide("Cancel"),
        WS_TABSTOP.0,
        rect(368, 218, 96, 30),
        IDCANCEL.0,
    )?;
    Ok(field)
}

impl FeedbackWindow {
    /// Reads the box and the kind. An empty box keeps the window open.
    unsafe fn submit(&self, hwnd: HWND) {
        let Ok(field) = GetDlgItem(Some(hwnd), TEXT_FIELD) else {
            return;
        };
        let length = GetWindowTextLengthW(field).max(0) as usize;
        let mut units = vec![0u16; length + 1];
        let read = GetWindowTextW(field, &mut units).max(0) as usize;
        let text = String::from_utf16_lossy(&units[..read.min(length)]);
        if text.trim().is_empty() {
            let _ = SetFocus(Some(field));
            return;
        }
        let selected = GetDlgItem(Some(hwnd), KIND_LIST)
            .map(|list| SendMessageW(list, CB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0)
            .unwrap_or(0);
        let kind = Kind::from_index(selected.max(0) as usize);
        *self.result.borrow_mut() = Some((text, kind));
        let _ = DestroyWindow(hwnd);
    }
}

unsafe extern "system" fn feedback_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lp.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const FeedbackWindow;
    if !ptr.is_null() {
        let window = &*ptr;
        match message {
            WM_COMMAND => match wp.0 & 0xffff {
                id if id == IDOK.0 as usize => {
                    window.submit(hwnd);
                    return LRESULT(0);
                }
                id if id == IDCANCEL.0 as usize => {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                _ => {}
            },
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            WM_CTLCOLORSTATIC => {
                let dc = HDC(wp.0 as *mut _);
                let _ = SetBkMode(dc, TRANSPARENT);
                let _ = SetTextColor(dc, COLORREF(GetSysColor(COLOR_WINDOWTEXT)));
                return LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize);
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, message, wp, lp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::AtomicUsize;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "bubbleplay-feedback-ui-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn line(ts: &str, level: &str, msg: &str) -> String {
        json!({"ts": ts, "level": level, "src": "tray", "target": "t", "msg": msg}).to_string()
    }

    fn write_lines(path: &Path, lines: &[String]) {
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn last_lines_returns_the_tail_and_skips_blanks() {
        let dir = Scratch::new();
        let path = dir.0.join("a.jsonl");
        fs::write(&path, "one\n\ntwo\r\nthree\nfour\n").unwrap();
        assert_eq!(
            read_last_lines(&path, 3).unwrap(),
            vec!["two", "three", "four"]
        );
        assert_eq!(read_last_lines(&path, 100).unwrap().len(), 4);
        assert!(read_last_lines(&dir.0.join("missing"), 3).is_err());
    }

    #[test]
    fn last_lines_drops_the_partial_first_line_of_a_long_file() {
        let dir = Scratch::new();
        let path = dir.0.join("big.jsonl");
        let row = format!("{}\n", "x".repeat(99));
        let rows = (TAIL_BYTES as usize / row.len()) + 50;
        fs::write(&path, row.repeat(rows)).unwrap();
        let lines = read_last_lines(&path, usize::MAX).unwrap();
        assert!(lines.iter().all(|l| l.len() == 99));
        assert!(lines.len() < rows);
    }

    #[test]
    fn source_lines_span_day_files_and_skip_junk() {
        let dir = Scratch::new();
        write_lines(
            &dir.0.join("tray-2026-11-01.jsonl"),
            &[
                line("2026-11-01T10:00:00.000Z", "info", "a"),
                "not json".into(),
                line("2026-11-01T10:00:01.000Z", "info", "b"),
            ],
        );
        write_lines(
            &dir.0.join("tray-2026-11-02.jsonl"),
            &[line("2026-11-02T09:00:00.000Z", "info", "c")],
        );
        write_lines(
            &dir.0.join("other-2026-11-02.jsonl"),
            &[line("x", "info", "z")],
        );
        let got = last_source_lines(&dir.0, "tray", 2);
        let msgs: Vec<&str> = got.iter().map(|v| v["msg"].as_str().unwrap()).collect();
        assert_eq!(msgs, vec!["b", "c"]);
        assert_eq!(last_source_lines(&dir.0, "tray", 10).len(), 3);
        assert!(last_source_lines(&dir.0, "agent", 10).is_empty());
    }

    #[test]
    fn recent_lines_merge_tray_and_host_by_time() {
        let dir = Scratch::new();
        write_lines(
            &dir.0.join("tray-2026-11-02.jsonl"),
            &[
                line("2026-11-02T09:00:00.000Z", "info", "t1"),
                line("2026-11-02T09:00:02.000Z", "info", "t2"),
            ],
        );
        write_lines(
            &dir.0.join("host-2026-11-02.jsonl"),
            &[line("2026-11-02T09:00:01.000Z", "warn", "h1")],
        );
        let msgs: Vec<String> = recent_lines(&dir.0, 10)
            .iter()
            .map(|v| v["msg"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(msgs, vec!["t1", "h1", "t2"]);
        assert_eq!(recent_lines(&dir.0, 2).len(), 2);
    }

    #[test]
    fn split_keeps_200_logs_and_20_errors() {
        let lines: Vec<Value> = (0..300)
            .map(|i| {
                let level = if i % 5 == 0 { "error" } else { "info" };
                serde_json::from_str(&line(
                    &format!("2026-11-02T09:{:02}:00.000Z", i % 60),
                    level,
                    &i.to_string(),
                ))
                .unwrap()
            })
            .collect();
        let (logs, errors) = split_logs(lines);
        assert_eq!(logs.len(), 200);
        assert_eq!(logs[199]["msg"], "299");
        assert_eq!(errors.len(), 20);
        assert_eq!(errors[19]["msg"], "295");
        assert!(errors.iter().all(|e| e["level"] == "error"));
    }

    #[test]
    fn clip_text_cuts_on_a_character_boundary() {
        assert_eq!(clip_text("abc", 10), "abc");
        assert_eq!(clip_text("aéb", 2), "a");
        assert_eq!(clip_text("aéb", 3), "aé");
    }

    #[test]
    fn ids_look_like_version_4_uuids_and_differ() {
        let a = new_id();
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4');
        assert_ne!(a, new_id());
    }

    #[test]
    fn kind_index_maps_the_list_order_and_defaults_to_bug() {
        let kinds: Vec<&str> = (0..4).map(|i| Kind::from_index(i).as_str()).collect();
        assert_eq!(kinds, vec!["bug", "complaint", "idea", "other"]);
        assert_eq!(Kind::from_index(9), Kind::Bug);
        assert_eq!(Kind::from_index(1).label(), "Complaint");
    }

    #[test]
    fn report_has_the_spec_fields() {
        let report = build_report(
            &ReportInput {
                id: "id-1".into(),
                created: "2026-11-02T10:00:00.000Z".into(),
                text: "  it broke  ",
                kind: Kind::Idea,
                version: "1.2.3",
                commit: "abc1234",
                device_name: "PC",
            },
            vec![serde_json::from_str(&line("2026-11-02T09:00:00.000Z", "error", "boom")).unwrap()],
        );
        assert_eq!(report["version"], 1);
        assert_eq!(report["text"], "it broke");
        assert_eq!(report["kind"], "idea");
        assert_eq!(report["platform"], "windows");
        assert_eq!(report["device"]["id"], "this-pc");
        assert_eq!(report["app"]["commit"], "abc1234");
        assert!(report["pc"].is_null() && report["session"].is_null());
        assert_eq!(report["logs"].as_array().unwrap().len(), 1);
        assert_eq!(report["errors"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn save_report_stores_a_redacted_this_pc_report() {
        let dir = Scratch::new();
        write_lines(
            &dir.0.join("tray-2026-11-02.jsonl"),
            &[line("2026-11-02T09:00:00.000Z", "error", "failed")],
        );
        let path = save_report(
            &dir.0,
            "it broke. password=hunter2hunter2",
            Kind::Bug,
            SystemTime::now(),
        )
        .unwrap();
        assert!(path.starts_with(dir.0.join("feedback")));
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.ends_with("-this-pc.json"), "{name}");
        let stored = fs::read_to_string(&path).unwrap();
        assert!(!stored.contains("hunter2hunter2"));
        let value: Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(value["kind"], "bug");
        assert_eq!(value["errors"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn saving_without_any_log_files_still_works() {
        let dir = Scratch::new();
        let path = save_report(&dir.0, "idea", Kind::Idea, SystemTime::now()).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert!(value["logs"].as_array().unwrap().is_empty());
    }
}
