//! Owner-facing PC key export and import from the tray menu. Each action runs on its own thread
//! with its own windows and message loop, so the tray never waits on a dialog or on Argon2.
//! Passphrases and key text live in `Zeroizing` buffers and are never logged.

use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use bubbleplay_host::device_key::{DeviceKeyError, KeyFile};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, GetSaveFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY,
    OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;
use zeroize::{Zeroize, Zeroizing};

use super::device_key_file;
use super::platform::wide;

const PASSPHRASE_FIELD: i32 = 201;
const CONFIRM_FIELD: i32 = 202;
const ERROR_LABEL: i32 = 203;
/// Longest passphrase read from a field, in UTF-16 units. Longer input is cut at this length.
const FIELD_UNITS: usize = 1024;
const FILE_UNITS: usize = 1024;
const WRONG_PASSPHRASE: &str = "Wrong passphrase. Try again.";
const CREATE_NOTE: &str =
    "Without a passphrase, anyone with this file can act as this PC. A passphrase is recommended.";
const UNLOCK_NOTE: &str = "This key file is protected. Enter its passphrase to import it.";

/// The owner's action from the tray menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Export,
    Import,
}

/// How a flow ended. `Replaced` is the only result that changes the PC key.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Exported(PathBuf),
    Replaced(String),
    Unchanged,
    Failed(Flow, String),
}

impl Outcome {
    fn announce(&self) {
        match self {
            Outcome::Exported(path) => show_message(
                flow_title(Flow::Export),
                &format!("PC key exported to:\n{}", path.display()),
                MB_ICONINFORMATION,
            ),
            Outcome::Replaced(id) => show_message(
                flow_title(Flow::Import),
                &format!("PC key replaced. This PC's ID is now {id}. The host is restarting with the new key."),
                MB_ICONINFORMATION,
            ),
            Outcome::Failed(flow, text) => {
                show_message(flow_title(*flow), text, MB_ICONERROR);
            }
            Outcome::Unchanged => {}
        }
    }
}

fn flow_title(flow: Flow) -> &'static str {
    match flow {
        Flow::Export => "Export PC key",
        Flow::Import => "Import PC key",
    }
}

/// Runs `flow` on a new thread. `done` receives `true` once the PC key was replaced, which tells
/// the tray to restart the host. It runs before the result message is shown.
pub fn start(
    flow: Flow,
    key_path: PathBuf,
    done: impl FnOnce(bool) + Send + 'static,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("pc-key-ui".to_owned())
        .spawn(move || {
            let outcome = match flow {
                Flow::Export => export(&key_path),
                Flow::Import => import(&key_path),
            };
            done(matches!(outcome, Outcome::Replaced(_)));
            outcome.announce();
        })
        .map(|_| ())
}

fn export(key_path: &Path) -> Outcome {
    match export_file(key_path) {
        Ok(Some(path)) => Outcome::Exported(path),
        Ok(None) => Outcome::Unchanged,
        Err(text) => Outcome::Failed(Flow::Export, text),
    }
}

fn export_file(key_path: &Path) -> std::result::Result<Option<PathBuf>, String> {
    let current = load_current(key_path).map_err(|error| {
        format!("The PC key could not be read, so nothing was exported. {error:#}")
    })?;
    let key = KeyFile::parse(&current).map_err(|error| {
        format!("The PC key could not be read, so nothing was exported. {error}")
    })?;
    let Some(passphrase) = ask_passphrase(flow_title(Flow::Export), PassphraseMode::Create, None)
    else {
        return Ok(None);
    };
    let Some(path) = file_dialog(
        flow_title(Flow::Export),
        &export_file_name(&key.id().to_string()),
        true,
    ) else {
        return Ok(None);
    };
    let text = export_text(&current, non_empty(&passphrase))
        .map_err(|error| format!("The PC key could not be exported. {error}"))?;
    fs::write(&path, text.as_bytes())
        .map_err(|error| format!("The key file could not be written. {error}"))?;
    Ok(Some(path))
}

fn import(key_path: &Path) -> Outcome {
    let Some(source_path) = file_dialog(flow_title(Flow::Import), "", false) else {
        return Outcome::Unchanged;
    };
    let source = match fs::read_to_string(&source_path) {
        Ok(text) => Zeroizing::new(text),
        Err(error) => {
            return Outcome::Failed(
                Flow::Import,
                format!("The key file could not be read. {error}"),
            );
        }
    };
    let mut passphrase: Option<Zeroizing<String>> = None;
    loop {
        let attempt = import_key(
            key_path,
            &source,
            passphrase.as_ref().map(|text| text.as_str()),
            confirm_replace,
        );
        let retry_message = match attempt {
            Ok(new_id) => return Outcome::Replaced(new_id),
            Err(ImportError::Declined) => return Outcome::Unchanged,
            Err(ImportError::Failed(text)) => return Outcome::Failed(Flow::Import, text),
            Err(ImportError::NeedsPassphrase) => None,
            Err(ImportError::WrongPassphrase) => Some(WRONG_PASSPHRASE),
        };
        let Some(text) = ask_passphrase(
            flow_title(Flow::Import),
            PassphraseMode::Unlock,
            retry_message,
        ) else {
            return Outcome::Unchanged;
        };
        passphrase = Some(text);
    }
}

/// Why an import stopped before the key was replaced.
#[derive(Debug)]
pub enum ImportError {
    NeedsPassphrase,
    WrongPassphrase,
    /// The owner said no at the confirmation; nothing changed.
    Declined,
    Failed(String),
}

/// What an import replaces: the readable PC key and its ID, or a key file that cannot be read.
#[derive(Debug, PartialEq, Eq)]
pub enum Previous<'a> {
    Id(&'a str),
    Unreadable,
}

/// The ID of the PC key now at `key_path`, or `None` when the file exists but cannot be read.
/// A missing file is created first, as the host does at start.
fn current_id(key_path: &Path) -> Option<String> {
    let text = load_current(key_path).ok()?;
    KeyFile::parse(&text).ok().map(|key| key.id().to_string())
}

/// Checks the `.bpkey` text in `source` and, once `confirm` agrees, replaces the PC key at
/// `key_path` with its plain form. Nothing is replaced unless every check passes. Returns the new
/// PC ID. `confirm` gets what is being replaced and the new ID. An unreadable current key is
/// renamed beside itself, never deleted, before the new key is written.
pub fn import_key(
    key_path: &Path,
    source: &str,
    passphrase: Option<&str>,
    confirm: impl FnOnce(Previous<'_>, &str) -> bool,
) -> std::result::Result<String, ImportError> {
    let file = KeyFile::parse(source).map_err(|error| {
        ImportError::Failed(format!("This is not a BubblePlay key file. {error}"))
    })?;
    let file_passphrase = if file.is_protected() {
        if passphrase.is_none() {
            return Err(ImportError::NeedsPassphrase);
        }
        passphrase
    } else {
        None
    };
    let unlocked = file.rewrap(file_passphrase, None).map_err(unlock_error)?;
    let plain = unlocked.to_json().map(Zeroizing::new).map_err(|error| {
        ImportError::Failed(format!("The key file could not be prepared. {error}"))
    })?;
    let new_id = file.id().to_string();
    let old_id = current_id(key_path);
    let previous = match &old_id {
        Some(id) => Previous::Id(id.as_str()),
        None => Previous::Unreadable,
    };
    if !confirm(previous, &new_id) {
        return Err(ImportError::Declined);
    }
    let moved = match old_id {
        Some(_) => None,
        None => Some(
            device_key_file::quarantine_unreadable(key_path).map_err(|error| {
                ImportError::Failed(format!(
                    "The unreadable PC key could not be renamed, so nothing changed. {error:#}"
                ))
            })?,
        ),
    };
    if let Err(error) = device_key_file::replace_pc_key(key_path, &plain) {
        let text = match moved {
            None => format!("The PC key was not replaced, so nothing changed. {error:#}"),
            Some(moved) => match device_key_file::restore_quarantined(&moved, key_path) {
                Ok(()) => format!("The PC key was not replaced, so nothing changed. {error:#}"),
                Err(restore) => format!(
                    "The PC key was not replaced ({error:#}). The unreadable key could not be put back; it is kept at {}. {restore:#}",
                    moved.display()
                ),
            },
        };
        return Err(ImportError::Failed(text));
    }
    Ok(new_id)
}

fn unlock_error(error: DeviceKeyError) -> ImportError {
    match error {
        DeviceKeyError::AuthenticationFailed => ImportError::WrongPassphrase,
        other => ImportError::Failed(format!("The key file could not be unlocked. {other}")),
    }
}

/// The plain key text for `plain`, protected by `passphrase` when one is given.
pub fn export_text(
    plain: &str,
    passphrase: Option<&str>,
) -> std::result::Result<Zeroizing<String>, DeviceKeyError> {
    let key = KeyFile::parse(plain)?;
    let wrapped = key.rewrap(None, passphrase)?;
    wrapped.to_json().map(Zeroizing::new)
}

/// Default file name for an exported key: the PC ID with the `.bpkey` extension.
pub fn export_file_name(pc_id: &str) -> String {
    let stem: String = pc_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let stem = if stem.is_empty() {
        "bubbleplay-pc".to_owned()
    } else {
        stem
    };
    format!("{stem}.bpkey")
}

/// Both fields must match. Two empty fields mean no passphrase.
pub fn validate_new_passphrase(
    passphrase: &str,
    confirmation: &str,
) -> std::result::Result<(), &'static str> {
    if passphrase == confirmation {
        Ok(())
    } else {
        Err("The passphrases do not match. Type the same passphrase in both fields.")
    }
}

pub fn replace_confirm_text(old_id: &str, new_id: &str) -> String {
    if old_id == new_id {
        return format!(
            "Replace this PC's key? The PC ID stays {new_id}. The key file is replaced with the one you chose."
        );
    }
    format!(
        "Replace this PC's key? The PC ID changes from {old_id} to {new_id}. Viewers that know the old ID will not find this PC until they use the new one."
    )
}

/// Confirmation text when the current key file cannot be read. The unreadable file is kept.
pub fn unreadable_confirm_text(new_id: &str) -> String {
    format!(
        "The current PC key can't be read. Replace it? This PC gets a new ID, {new_id}. The unreadable file is kept in the same folder, not deleted."
    )
}

fn non_empty(passphrase: &Zeroizing<String>) -> Option<&str> {
    Some(passphrase.as_str()).filter(|text| !text.is_empty())
}

fn load_current(key_path: &Path) -> Result<Zeroizing<String>> {
    device_key_file::handoff(
        key_path,
        &device_key_file::default_label(),
        &device_key_file::now_rfc3339(),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PassphraseMode {
    /// Export: a passphrase, optional, typed twice.
    Create,
    /// Import: the passphrase of a protected file.
    Unlock,
}

struct PassphraseWindow {
    mode: PassphraseMode,
    result: RefCell<Option<Zeroizing<String>>>,
}

/// Shows the passphrase window and returns what the owner typed, or `None` on cancel. In create
/// mode an empty result means no passphrase.
fn ask_passphrase(
    title: &str,
    mode: PassphraseMode,
    error: Option<&str>,
) -> Option<Zeroizing<String>> {
    static CLASS: OnceLock<bool> = OnceLock::new();
    let window = PassphraseWindow {
        mode,
        result: RefCell::new(None),
    };
    let title = wide(title);
    let (width, height) = match mode {
        PassphraseMode::Create => (478, 300),
        PassphraseMode::Unlock => (478, 250),
    };
    // SAFETY: the window and its controls are created, pumped and destroyed on this thread.
    // `window` outlives the message loop and is reached only through the window's user data.
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(PCWSTR::null()).ok()?.into();
        let class = w!("BubblePlayKeyPassphrase");
        if !*CLASS.get_or_init(|| {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(passphrase_proc),
                hInstance: instance,
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
                ..Default::default()
            }) != 0
        }) {
            return None;
        }
        let left = (GetSystemMetrics(SM_CXSCREEN) - width) / 2;
        let top = (GetSystemMetrics(SM_CYSCREEN) - height) / 2;
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_DLGMODALFRAME,
            class,
            PCWSTR(title.as_ptr()),
            WS_CAPTION | WS_SYSMENU,
            left,
            top,
            width,
            height,
            None,
            None,
            Some(instance),
            Some(&window as *const PassphraseWindow as *const _),
        )
        .ok()?;
        let focus = match create_controls(hwnd, instance, mode, error) {
            Ok(field) => field,
            Err(_) => {
                let _ = DestroyWindow(hwnd);
                drain_quit();
                return None;
            }
        };
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(focus));
        pump_until_quit(hwnd);
        window.result.take()
    }
}

/// Runs this thread's message loop for `dialog` until it is destroyed and posts WM_QUIT.
unsafe fn pump_until_quit(dialog: HWND) {
    let mut msg = MSG::default();
    loop {
        let status = GetMessageW(&mut msg, None, 0, 0).0;
        if status == -1 || status == 0 {
            break;
        }
        if !IsDialogMessageW(dialog, &msg).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    drain_quit();
}

/// Removes any WM_QUIT left by a destroyed window, so the next dialog on this thread is not cut short.
unsafe fn drain_quit() {
    let mut msg = MSG::default();
    while PeekMessageW(&mut msg, None, WM_QUIT, WM_QUIT, PM_REMOVE).as_bool() {}
}

/// Creates the labels, fields and buttons. Returns the field that takes focus first.
unsafe fn create_controls(
    hwnd: HWND,
    instance: HINSTANCE,
    mode: PassphraseMode,
    error: Option<&str>,
) -> windows::core::Result<HWND> {
    let create = mode == PassphraseMode::Create;
    let note = wide(if create { CREATE_NOTE } else { UNLOCK_NOTE });
    let label = wide(if create {
        "Passphrase (optional)"
    } else {
        "Passphrase"
    });
    let (error_top, button_top) = if create { (170, 214) } else { (118, 162) };
    let password = ES_PASSWORD as u32 | ES_AUTOHSCROLL as u32 | WS_BORDER.0 | WS_TABSTOP.0;
    child(hwnd, instance, w!("STATIC"), &note, 0, [16, 12, 428, 46], 0)?;
    child(
        hwnd,
        instance,
        w!("STATIC"),
        &label,
        0,
        [16, 66, 428, 18],
        0,
    )?;
    let field = child(
        hwnd,
        instance,
        w!("EDIT"),
        &wide(""),
        password,
        [16, 86, 428, 24],
        PASSPHRASE_FIELD,
    )?;
    if create {
        let confirm = wide("Confirm passphrase");
        child(
            hwnd,
            instance,
            w!("STATIC"),
            &confirm,
            0,
            [16, 118, 428, 18],
            0,
        )?;
        child(
            hwnd,
            instance,
            w!("EDIT"),
            &wide(""),
            password,
            [16, 138, 428, 24],
            CONFIRM_FIELD,
        )?;
    }
    let error = wide(error.unwrap_or(""));
    child(
        hwnd,
        instance,
        w!("STATIC"),
        &error,
        0,
        [16, error_top, 428, 34],
        ERROR_LABEL,
    )?;
    child(
        hwnd,
        instance,
        w!("BUTTON"),
        &wide("OK"),
        (BS_DEFPUSHBUTTON as u32) | WS_TABSTOP.0,
        [240, button_top, 96, 30],
        IDOK.0,
    )?;
    child(
        hwnd,
        instance,
        w!("BUTTON"),
        &wide("Cancel"),
        WS_TABSTOP.0,
        [348, button_top, 96, 30],
        IDCANCEL.0,
    )?;
    Ok(field)
}

/// Creates one child control. `text` must be null-terminated, as `wide` returns it.
unsafe fn child(
    parent: HWND,
    instance: HINSTANCE,
    class: PCWSTR,
    text: &[u16],
    style: u32,
    [x, y, width, height]: [i32; 4],
    id: i32,
) -> windows::core::Result<HWND> {
    CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
        x,
        y,
        width,
        height,
        Some(parent),
        Some(HMENU(id as usize as *mut _)),
        Some(instance),
        None,
    )
}

impl PassphraseWindow {
    /// Validates the fields. On success the window closes with the passphrase as the result;
    /// otherwise it stays open with the error and both fields cleared.
    unsafe fn submit(&self, hwnd: HWND) {
        let passphrase = read_field(hwnd, PASSPHRASE_FIELD);
        let problem = match self.mode {
            PassphraseMode::Create => {
                let confirmation = read_field(hwnd, CONFIRM_FIELD);
                validate_new_passphrase(&passphrase, &confirmation).err()
            }
            PassphraseMode::Unlock => passphrase.is_empty().then_some("Enter the passphrase."),
        };
        match problem {
            None => {
                *self.result.borrow_mut() = Some(passphrase);
                let _ = DestroyWindow(hwnd);
            }
            Some(text) => {
                let _ = SetDlgItemTextW(hwnd, PASSPHRASE_FIELD, w!(""));
                let _ = SetDlgItemTextW(hwnd, CONFIRM_FIELD, w!(""));
                let text = wide(text);
                let _ = SetDlgItemTextW(hwnd, ERROR_LABEL, PCWSTR(text.as_ptr()));
                if let Ok(field) = GetDlgItem(Some(hwnd), PASSPHRASE_FIELD) {
                    let _ = SetFocus(Some(field));
                }
            }
        }
    }
}

/// Reads a field into a zeroed buffer; the returned copy is wiped on drop.
fn read_field(hwnd: HWND, id: i32) -> Zeroizing<String> {
    let mut units = [0u16; FIELD_UNITS];
    // SAFETY: the buffer is FIELD_UNITS long and the call writes at most that many units.
    let length = unsafe { GetDlgItemTextW(hwnd, id, &mut units) } as usize;
    let text = Zeroizing::new(String::from_utf16_lossy(&units[..length.min(FIELD_UNITS)]));
    units.zeroize();
    text
}

unsafe extern "system" fn passphrase_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lp.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const PassphraseWindow;
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
            WM_DESTROY => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, message, wp, lp)
}

/// Yes/No confirmation for replacing the PC key. The default button is No.
fn confirm_replace(previous: Previous<'_>, new_id: &str) -> bool {
    let text = wide(match previous {
        Previous::Id(old_id) => replace_confirm_text(old_id, new_id),
        Previous::Unreadable => unreadable_confirm_text(new_id),
    });
    let title = wide(flow_title(Flow::Import));
    // SAFETY: both buffers are null-terminated and outlive the synchronous call.
    let answer = unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_TOPMOST,
        )
    };
    answer == IDYES
}

fn show_message(title: &str, text: &str, icon: MESSAGEBOX_STYLE) {
    let title = wide(title);
    let text = wide(text);
    // SAFETY: both buffers are null-terminated and outlive the synchronous call.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | icon | MB_TOPMOST,
        );
    }
}

/// Save or open dialog for `.bpkey` files. Returns `None` on cancel.
fn file_dialog(title: &str, default_name: &str, save: bool) -> Option<PathBuf> {
    let filter: Vec<u16> = "BubblePlay key (*.bpkey)\0*.bpkey\0\0"
        .encode_utf16()
        .collect();
    let title = wide(title);
    let mut file = [0u16; FILE_UNITS];
    for (slot, unit) in file
        .iter_mut()
        .zip(default_name.encode_utf16().take(FILE_UNITS - 1))
    {
        *slot = unit;
    }
    let common = OFN_EXPLORER | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_HIDEREADONLY;
    let mut dialog = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(file.as_mut_ptr()),
        nMaxFile: FILE_UNITS as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        Flags: if save {
            common | OFN_OVERWRITEPROMPT
        } else {
            common | OFN_FILEMUSTEXIST
        },
        lpstrDefExt: w!("bpkey"),
        ..Default::default()
    };
    // SAFETY: every pointer in `dialog` refers to a buffer that lives across this synchronous call.
    let accepted = unsafe {
        if save {
            GetSaveFileNameW(&mut dialog)
        } else {
            GetOpenFileNameW(&mut dialog)
        }
    };
    if !accepted.as_bool() {
        return None;
    }
    let length = file.iter().position(|&unit| unit == 0)?;
    Some(PathBuf::from(OsString::from_wide(&file[..length])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CREATED: &str = "2026-10-15T09:30:00Z";

    /// A fresh directory under the temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "bubbleplay-key-ui-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn key(&self) -> PathBuf {
            self.0.join("device-key.bpkey")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn plain_key(label: &str) -> String {
        KeyFile::create(label, CREATED, None)
            .unwrap()
            .to_json()
            .unwrap()
    }

    fn id_of(text: &str) -> String {
        KeyFile::parse(text).unwrap().id().to_string()
    }

    #[test]
    fn export_file_name_is_the_pc_id_with_the_extension() {
        assert_eq!(export_file_name("B7QK-9TMX-4DRA"), "B7QK-9TMX-4DRA.bpkey");
        assert_eq!(export_file_name("a/b\\c:d"), "abcd.bpkey");
        assert_eq!(export_file_name(""), "bubbleplay-pc.bpkey");
    }

    #[test]
    fn passphrases_must_match_and_two_empty_fields_mean_none() {
        assert!(validate_new_passphrase("", "").is_ok());
        assert!(validate_new_passphrase("correct horse", "correct horse").is_ok());
        assert!(validate_new_passphrase("correct horse", "correct hors").is_err());
        assert!(validate_new_passphrase("", "x").is_err());
        assert!(validate_new_passphrase("x", "").is_err());
    }

    #[test]
    fn replace_confirmation_names_both_ids() {
        assert_eq!(
            replace_confirm_text("OLD1-AAAA", "NEW2-BBBB"),
            "Replace this PC's key? The PC ID changes from OLD1-AAAA to NEW2-BBBB. Viewers that know the old ID will not find this PC until they use the new one."
        );
        assert!(replace_confirm_text("SAME-1", "SAME-1").contains("The PC ID stays SAME-1."));
    }

    #[test]
    fn export_protects_with_a_passphrase_and_keeps_the_identity() {
        let plain = plain_key("PC");
        let protected = export_text(&plain, Some("pass phrase")).unwrap();
        let parsed = KeyFile::parse(&protected).unwrap();
        assert!(parsed.is_protected());
        assert!(parsed.unlock(Some("pass phrase")).is_ok());
        assert_eq!(parsed.id().to_string(), id_of(&plain));
        let unprotected = export_text(&plain, None).unwrap();
        assert!(!KeyFile::parse(&unprotected).unwrap().is_protected());
    }

    #[test]
    fn an_invalid_file_leaves_the_pc_key_untouched() {
        let dir = Scratch::new();
        let key = dir.key();
        device_key_file::replace_pc_key(&key, &plain_key("PC")).unwrap();
        let before = fs::read(&key).unwrap();
        for source in ["not a key", "{}", r#"{"format":"x"}"#] {
            let result = import_key(&key, source, None, |_, _| panic!("confirm must not run"));
            assert!(matches!(result, Err(ImportError::Failed(_))), "{source}");
            assert_eq!(fs::read(&key).unwrap(), before);
        }
    }

    #[test]
    fn a_protected_file_asks_for_its_passphrase_and_rejects_a_wrong_one() {
        let dir = Scratch::new();
        let key = dir.key();
        device_key_file::replace_pc_key(&key, &plain_key("PC")).unwrap();
        let before = fs::read(&key).unwrap();
        let source = export_text(&plain_key("Laptop"), Some("right")).unwrap();
        assert!(matches!(
            import_key(&key, &source, None, |_, _| true),
            Err(ImportError::NeedsPassphrase)
        ));
        assert!(matches!(
            import_key(&key, &source, Some("wrong"), |_, _| true),
            Err(ImportError::WrongPassphrase)
        ));
        assert_eq!(fs::read(&key).unwrap(), before);
    }

    #[test]
    fn a_confirmed_import_replaces_the_key_and_a_declined_one_does_not() {
        let dir = Scratch::new();
        let key = dir.key();
        let old = plain_key("PC");
        device_key_file::replace_pc_key(&key, &old).unwrap();
        let before = fs::read(&key).unwrap();
        let incoming = plain_key("Laptop");
        let mut seen = None;
        let result = import_key(&key, &incoming, None, |previous, new_id| {
            seen = Some((format!("{previous:?}"), new_id.to_owned()));
            false
        });
        assert!(matches!(result, Err(ImportError::Declined)));
        assert_eq!(fs::read(&key).unwrap(), before);
        let old_debug = format!("{:?}", Previous::Id(id_of(&old).as_str()));
        assert_eq!(seen, Some((old_debug, id_of(&incoming))));

        let new_id = import_key(&key, &incoming, None, |_, _| true).unwrap();
        assert_eq!(new_id, id_of(&incoming));
        let loaded = load_current(&key).unwrap();
        assert_eq!(id_of(&loaded), new_id);
    }

    #[test]
    fn an_unreadable_key_is_renamed_and_replaced_after_confirmation() {
        let dir = Scratch::new();
        let key = dir.key();
        let unreadable = b"corrupt blob".to_vec();
        fs::write(&key, &unreadable).unwrap();
        let incoming = plain_key("Laptop");
        let mut asked = None;
        let new_id = import_key(&key, &incoming, None, |previous, _| {
            asked = Some(format!("{previous:?}"));
            true
        })
        .unwrap();
        assert_eq!(asked.as_deref(), Some("Unreadable"));
        assert_eq!(new_id, id_of(&incoming));
        let copies = unreadable_copies(&dir);
        assert_eq!(copies.len(), 1);
        assert_eq!(fs::read(&copies[0]).unwrap(), unreadable);
        assert_eq!(id_of(&load_current(&key).unwrap()), new_id);
    }

    #[test]
    fn an_unreadable_key_is_untouched_when_the_replacement_is_declined() {
        let dir = Scratch::new();
        let key = dir.key();
        let unreadable = b"corrupt blob".to_vec();
        fs::write(&key, &unreadable).unwrap();
        let result = import_key(&key, &plain_key("Laptop"), None, |_, _| false);
        assert!(matches!(result, Err(ImportError::Declined)));
        assert_eq!(fs::read(&key).unwrap(), unreadable);
        assert!(unreadable_copies(&dir).is_empty());
    }

    #[test]
    fn the_unreadable_confirmation_names_the_new_id() {
        let text = unreadable_confirm_text("NEW2-BBBB");
        assert!(text.starts_with("The current PC key can't be read. Replace it?"));
        assert!(text.contains("This PC gets a new ID, NEW2-BBBB."));
    }

    fn unreadable_copies(dir: &Scratch) -> Vec<PathBuf> {
        fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("device-key.bpkey.unreadable-")
            })
            .collect()
    }
}
