//! Opens the Flutter Devices app from the tray: find and raise its window, or
//! start it detached. The decision is a pure function; the Win32 calls sit
//! behind [`AppOps`] so tests never start or touch a real app.

use std::io;
use std::path::{Path, PathBuf};

/// File name of the Flutter app, installed beside the tray exe.
pub const APP_EXE: &str = "bubbleplay_app.exe";
/// Window class of the Flutter Windows runner.
pub const APP_WINDOW_CLASS: &str = "FLUTTER_RUNNER_WIN32_WINDOW";
/// Argument that makes the app open on the This PC section.
pub const APP_SECTION_ARG: &str = "--section=this-pc";

/// What the tray does for the Devices action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPlan {
    /// No app installed: write `open-devices` (the Win32 Devices window).
    LegacyWindow,
    /// The app is running: bring its window to the front.
    FocusApp,
    /// The app is not running: start it detached.
    StartApp,
}

pub fn plan_open(app_exists: bool, window_found: bool) -> OpenPlan {
    match (app_exists, window_found) {
        (false, _) => OpenPlan::LegacyWindow,
        (true, true) => OpenPlan::FocusApp,
        (true, false) => OpenPlan::StartApp,
    }
}

/// Path of the app beside the tray exe.
pub fn app_exe_beside(tray_exe: &Path) -> PathBuf {
    tray_exe
        .parent()
        .map(|dir| dir.join(APP_EXE))
        .unwrap_or_else(|| PathBuf::from(APP_EXE))
}

/// The OS side of opening the app.
pub trait AppOps {
    /// Top-level app window (class [`APP_WINDOW_CLASS`]) whose process image is `exe`.
    fn find_window(&self, exe: &Path) -> Option<isize>;
    /// Restores and raises the window; false when Windows refused.
    fn focus(&self, window: isize) -> bool;
    /// Starts `exe --section=this-pc` detached.
    fn spawn(&self, exe: &Path) -> io::Result<()>;
}

/// What happened, so the caller knows which stdin line to write.
#[derive(Debug, PartialEq, Eq)]
pub enum OpenOutcome {
    /// The app is not installed: the caller writes `open-devices`.
    Legacy,
    /// The app was raised or started: the caller writes `show-devices-app`.
    /// `focus_refused` is true when Windows refused to raise the window.
    App { focus_refused: bool },
}

/// Runs the plan. `app_exe` is `None` when the app is not installed.
pub fn open_app(app_exe: Option<&Path>, ops: &dyn AppOps) -> io::Result<OpenOutcome> {
    let window = app_exe.and_then(|exe| ops.find_window(exe));
    match (
        plan_open(app_exe.is_some(), window.is_some()),
        app_exe,
        window,
    ) {
        (OpenPlan::FocusApp, _, Some(window)) => Ok(OpenOutcome::App {
            focus_refused: !ops.focus(window),
        }),
        (OpenPlan::StartApp, Some(exe), _) => {
            ops.spawn(exe)?;
            Ok(OpenOutcome::App {
                focus_refused: false,
            })
        }
        _ => Ok(OpenOutcome::Legacy),
    }
}

/// The real Win32 implementation.
pub struct Win32AppOps;

impl AppOps for Win32AppOps {
    fn find_window(&self, exe: &Path) -> Option<isize> {
        win32::find_window(exe)
    }
    fn focus(&self, window: isize) -> bool {
        win32::focus(window)
    }
    fn spawn(&self, exe: &Path) -> io::Result<()> {
        win32::spawn(exe)
    }
}

mod win32 {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use windows::core::{BOOL, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, EnumWindows, GetClassNameW, GetWindowThreadProcessId, IsIconic,
        SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    fn normalize(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    fn same_path(a: &Path, b: &Path) -> bool {
        normalize(a)
            .to_string_lossy()
            .eq_ignore_ascii_case(&normalize(b).to_string_lossy())
    }

    fn process_image(pid: u32) -> Option<PathBuf> {
        // SAFETY: the handle is closed below; the buffer outlives the call.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buffer = [0u16; 1024];
            let mut len = buffer.len() as u32;
            let queried = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut len,
            );
            let _ = CloseHandle(process);
            queried.ok()?;
            Some(PathBuf::from(String::from_utf16_lossy(
                &buffer[..len as usize],
            )))
        }
    }

    struct Search<'a> {
        exe: &'a Path,
        found: Option<isize>,
    }

    unsafe extern "system" fn visit(window: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the &mut Search passed to EnumWindows below.
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut class = [0u16; 64];
        // SAFETY: the buffer is valid for the call.
        let len = unsafe { GetClassNameW(window, &mut class) };
        if len <= 0 || String::from_utf16_lossy(&class[..len as usize]) != super::APP_WINDOW_CLASS {
            return BOOL(1);
        }
        let mut pid = 0u32;
        // SAFETY: pid is a valid out pointer.
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        if process_image(pid).is_some_and(|image| same_path(&image, search.exe)) {
            search.found = Some(window.0 as isize);
            return BOOL(0);
        }
        BOOL(1)
    }

    pub fn find_window(exe: &Path) -> Option<isize> {
        let mut search = Search { exe, found: None };
        // SAFETY: the callback only runs during this call and `search` outlives it.
        // EnumWindows reports an error when the callback stops it early; ignored.
        let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize)) };
        search.found
    }

    pub fn focus(window: isize) -> bool {
        let hwnd = HWND(window as *mut _);
        // SAFETY: plain window calls on a handle that may have gone stale, which they tolerate.
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = BringWindowToTop(hwnd);
            SetForegroundWindow(hwnd).as_bool()
        }
    }

    pub fn spawn(exe: &Path) -> std::io::Result<()> {
        let mut command = Command::new(exe);
        command
            .arg(super::APP_SECTION_ARG)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // A GUI app: no CREATE_NO_WINDOW. Detached so it outlives the tray.
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        if let Some(dir) = exe.parent() {
            command.current_dir(dir);
        }
        command.spawn().map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn decision_table() {
        assert_eq!(plan_open(false, false), OpenPlan::LegacyWindow);
        assert_eq!(plan_open(false, true), OpenPlan::LegacyWindow);
        assert_eq!(plan_open(true, true), OpenPlan::FocusApp);
        assert_eq!(plan_open(true, false), OpenPlan::StartApp);
    }

    #[test]
    fn app_is_looked_up_beside_the_tray() {
        assert_eq!(
            app_exe_beside(Path::new(r"C:\x\bubbleplay_tray.exe")),
            PathBuf::from(r"C:\x\bubbleplay_app.exe")
        );
    }

    #[derive(Default)]
    struct Fake {
        window: Option<isize>,
        focus_ok: bool,
        spawn_fails: bool,
        calls: RefCell<Vec<String>>,
    }

    impl AppOps for Fake {
        fn find_window(&self, _: &Path) -> Option<isize> {
            self.calls.borrow_mut().push("find".into());
            self.window
        }
        fn focus(&self, window: isize) -> bool {
            self.calls.borrow_mut().push(format!("focus {window}"));
            self.focus_ok
        }
        fn spawn(&self, _: &Path) -> io::Result<()> {
            self.calls.borrow_mut().push("spawn".into());
            if self.spawn_fails {
                Err(io::Error::other("no"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn missing_app_is_legacy_and_touches_nothing() {
        let fake = Fake::default();
        assert_eq!(open_app(None, &fake).unwrap(), OpenOutcome::Legacy);
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn existing_window_is_focused_not_spawned() {
        let fake = Fake {
            window: Some(7),
            focus_ok: true,
            ..Fake::default()
        };
        assert_eq!(
            open_app(Some(Path::new("a.exe")), &fake).unwrap(),
            OpenOutcome::App {
                focus_refused: false
            }
        );
        assert_eq!(*fake.calls.borrow(), ["find", "focus 7"]);
    }

    #[test]
    fn refused_focus_is_reported_not_fatal() {
        let fake = Fake {
            window: Some(7),
            ..Fake::default()
        };
        assert_eq!(
            open_app(Some(Path::new("a.exe")), &fake).unwrap(),
            OpenOutcome::App {
                focus_refused: true
            }
        );
    }

    #[test]
    fn no_window_spawns_and_spawn_failure_surfaces() {
        let fake = Fake::default();
        assert_eq!(
            open_app(Some(Path::new("a.exe")), &fake).unwrap(),
            OpenOutcome::App {
                focus_refused: false
            }
        );
        assert_eq!(*fake.calls.borrow(), ["find", "spawn"]);
        let failing = Fake {
            spawn_fails: true,
            ..Fake::default()
        };
        assert!(open_app(Some(Path::new("a.exe")), &failing).is_err());
    }
}
