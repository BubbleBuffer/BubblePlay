//! Repro: a foreground window stops pumping messages while a remote session injects input.
//!
//! Drives the real interactive desktop (it makes its own windows foreground and injects
//! clicks), so it only runs where `BUBBLEPLAY_FOREGROUND_TESTS=1` and `GITHUB_ACTIONS=true`
//! (the Host checks job on a GitHub-hosted runner). Everywhere else it returns at once, so a
//! local `cargo test` never touches a user's desktop.
//!
//! Window A becomes foreground and then stops pumping. Window B on its own thread stays
//! healthy. Control clicks use plain `SendInput` from this elevated process (the call the admin
//! agent makes). Once Windows replaced A with DWM's System-integrity `Ghost` window, UIPI drops
//! that call, so the recovery click on B goes through the host's own input path instead:
//! `InputSink` packets -> `SecureHelperLink` -> the installed LocalSystem secure helper (CI
//! installs it with host/scripts/ci-install-secure-helper.ps1), which verifies the ghost
//! foreground itself and injects on the default desktop. The test also checks that the stall
//! report names A behind the ghost, and times the Win32 calls the host makes on its
//! foreground/cursor/clipboard paths while A is hung.

#![cfg(windows)]

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::MOUSEKEYS;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

static A_DOWN: AtomicU32 = AtomicU32::new(0);
static B_DOWN: AtomicU32 = AtomicU32::new(0);
static HOOK_DOWN: AtomicU32 = AtomicU32::new(0);
static HANG_A: AtomicBool = AtomicBool::new(false);
static A_HWND: AtomicIsize = AtomicIsize::new(0);
const WM_HANG: u32 = WM_APP + 7;
const WM_STOP: u32 = WM_APP + 8;

unsafe extern "system" fn proc_a(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            A_DOWN.fetch_add(1, Ordering::AcqRel);
        }
        WM_HANG => {
            // Stop pumping, like WhatsApp's UI thread: no message retrieval until released.
            while HANG_A.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(10));
            }
            return LRESULT(0);
        }
        WM_STOP => {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe extern "system" fn proc_b(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            B_DOWN.fetch_add(1, Ordering::AcqRel);
        }
        WM_STOP => {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe extern "system" fn mouse_ll(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && wp.0 as u32 == WM_LBUTTONDOWN {
        HOOK_DOWN.fetch_add(1, Ordering::AcqRel);
    }
    CallNextHookEx(None, code, wp, lp)
}

/// A top-level window on its own pumping thread. Returns (hwnd, thread).
fn spawn_window(
    class: PCWSTR,
    title: PCWSTR,
    proc: WNDPROC,
    x: i32,
) -> (isize, std::thread::JoinHandle<()>) {
    let class = class.0 as usize;
    let title = title.0 as usize;
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || unsafe {
        let module = GetModuleHandleW(None).unwrap();
        let class = PCWSTR(class as *const u16);
        let wc = WNDCLASSW {
            lpfnWndProc: proc,
            hInstance: module.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST,
            class,
            PCWSTR(title as *const u16),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            x,
            120,
            360,
            300,
            None,
            None,
            Some(module.into()),
            None,
        )
        .unwrap();
        let _ = ShowWindow(hwnd, SW_SHOW);
        tx.send(hwnd.0 as isize).unwrap();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = DestroyWindow(hwnd);
    });
    (rx.recv().unwrap(), thread)
}

fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut _)
}

fn center(window: HWND) -> POINT {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(window, &mut rect) }.unwrap();
    POINT {
        x: (rect.left + rect.right) / 2,
        y: (rect.top + rect.bottom) / 2 + 20,
    }
}

fn mouse(
    dx: i32,
    dy: i32,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

/// Plain `SendInput` click at a screen point, as the admin agent relays absolute packets.
fn click(point: POINT) -> u32 {
    let (vx, vy) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    };
    let (vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        )
    };
    let dx = ((point.x - vx) * 65535) / (vw - 1);
    let dy = ((point.y - vy) * 65535) / (vh - 1);
    let abs = MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
    let inputs = [
        mouse(dx, dy, MOUSEEVENTF_MOVE | abs),
        mouse(dx, dy, MOUSEEVENTF_LEFTDOWN | abs),
        mouse(dx, dy, MOUSEEVENTF_LEFTUP | abs),
    ];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) }
}

fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    done()
}

fn class_name(window: HWND) -> String {
    let mut buf = [0u16; 128];
    let len = unsafe { GetClassNameW(window, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..len])
}

fn integrity_of(process: HANDLE) -> Option<u32> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let mut buf = vec![0u8; 256];
        let mut len = 0;
        let ok = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr().cast()),
            buf.len() as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        ok.ok()?;
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let count = *GetSidSubAuthorityCount(label.Label.Sid);
        Some(*GetSidSubAuthority(label.Label.Sid, (count - 1) as u32))
    }
}

/// (exe, integrity) of the process owning `window`; None where access is denied.
fn owner(window: HWND) -> (u32, Option<String>, Option<u32>) {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return (pid, None, None);
    };
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    let exe = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()
    .map(|_| String::from_utf16_lossy(&buf[..len as usize]));
    let integrity = integrity_of(process);
    let _ = unsafe { CloseHandle(process) };
    (pid, exe, integrity)
}

fn timed(report: &mut String, name: &str, f: impl FnOnce()) -> u128 {
    let start = Instant::now();
    f();
    let ms = start.elapsed().as_millis();
    let _ = writeln!(report, "  {name}: {ms} ms");
    ms
}

/// The host's own foreground, cursor, MouseKeys and clipboard calls while the foreground is hung.
fn probe_host_calls(report: &mut String, phase: &str) -> u128 {
    let _ = writeln!(report, "host-path call timings ({phase}):");
    let fg = unsafe { GetForegroundWindow() };
    let mut worst = 0;
    worst = worst.max(timed(report, "GetClassNameW(foreground)", || {
        let _ = class_name(fg);
    }));
    worst = worst.max(timed(report, "IsHungAppWindow(foreground)", || {
        let _ = unsafe { IsHungAppWindow(fg) };
    }));
    worst = worst.max(timed(report, "GetGUIThreadInfo(0)", || {
        let mut gui = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let _ = unsafe { GetGUIThreadInfo(0, &mut gui) };
    }));
    worst = worst.max(timed(
        report,
        "foreground process token (privilege probe)",
        || {
            let _ = owner(fg);
        },
    ));
    worst = worst.max(timed(report, "GetCursorInfo", || {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        let _ = unsafe { GetCursorInfo(&mut info) };
    }));
    worst = worst.max(timed(
        report,
        "SPI_GET/SETMOUSEKEYS (same value, no broadcast)",
        || {
            let mut keys = MOUSEKEYS {
                cbSize: std::mem::size_of::<MOUSEKEYS>() as u32,
                ..Default::default()
            };
            unsafe {
                let _ = SystemParametersInfoW(
                    SPI_GETMOUSEKEYS,
                    keys.cbSize,
                    Some((&raw mut keys).cast()),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                );
                let _ = SystemParametersInfoW(
                    SPI_SETMOUSEKEYS,
                    keys.cbSize,
                    Some((&raw mut keys).cast()),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                );
            }
        },
    ));
    worst = worst.max(timed(report, "OpenClipboard/CloseClipboard", || unsafe {
        if OpenClipboard(None).is_ok() {
            let _ = CloseClipboard();
        }
    }));
    worst
}

struct Release;
impl Drop for Release {
    fn drop(&mut self) {
        HANG_A.store(false, Ordering::Release);
    }
}

#[test]
fn input_reaches_other_windows_while_the_foreground_app_is_hung() {
    if std::env::var("BUBBLEPLAY_FOREGROUND_TESTS").as_deref() != Ok("1")
        || std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true")
    {
        eprintln!("skipped: drives the interactive desktop; CI only");
        return;
    }
    let mut report = String::new();
    let me = unsafe { GetCurrentProcess() };
    let _ = writeln!(
        report,
        "test process integrity: {:?}",
        integrity_of(me).map(|r| format!("{r:#x}"))
    );

    // LL hook on its own pumping thread: shows whether injected clicks enter the system at all.
    let (hook_tx, hook_rx) = mpsc::channel::<u32>();
    let hook_thread = std::thread::spawn(move || unsafe {
        let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll), None, 0).ok();
        hook_tx
            .send(windows::Win32::System::Threading::GetCurrentThreadId())
            .unwrap();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        if let Some(hook) = hook {
            let _ = UnhookWindowsHookEx(hook);
        }
    });
    let hook_tid = hook_rx.recv().unwrap();

    let (a, a_thread) = spawn_window(w!("BpHungA"), w!("hung app"), Some(proc_a), 80);
    let (b, b_thread) = spawn_window(w!("BpHealthyB"), w!("healthy app"), Some(proc_b), 520);
    A_HWND.store(a, Ordering::Release);
    let release = Release;

    // Control: injected clicks reach both windows while A is healthy.
    let b0 = B_DOWN.load(Ordering::Acquire);
    let sent = click(center(hwnd(b)));
    let control_b = wait_for(Duration::from_secs(2), || {
        B_DOWN.load(Ordering::Acquire) > b0
    });
    let a0 = A_DOWN.load(Ordering::Acquire);
    click(center(hwnd(a)));
    let control_a = wait_for(Duration::from_secs(2), || {
        A_DOWN.load(Ordering::Acquire) > a0
    });
    let a_foreground = wait_for(
        Duration::from_secs(2),
        || unsafe { GetForegroundWindow() } == hwnd(a),
    );
    let _ = writeln!(
        report,
        "control: SendInput sent {sent}/3, click reached B: {control_b}, reached A: {control_a}, A foreground: {a_foreground}"
    );

    // The host's input path with the real secure helper, connected before the hang as a
    // running session would be. The helper's own checks (console user, admin policy, protected
    // image) apply unchanged.
    let virtual_desktop = unsafe {
        bubbleplay_host::input::DesktopRegion::new(
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN) as u32,
            GetSystemMetrics(SM_CYVIRTUALSCREEN) as u32,
        )
    };
    let link = connect_helper(virtual_desktop);
    let _ = writeln!(report, "secure helper link: {:?}", link.as_ref().err());
    let link = link.ok().map(|link| Arc::new(Mutex::new(link)));
    let mut sink = bubbleplay_host::input::InputSink::new().with_secure_helper(link.clone());
    sink.set_absolute_region(virtual_desktop);

    // A stops pumping while it is the foreground window.
    HANG_A.store(true, Ordering::Release);
    unsafe { PostMessageW(Some(hwnd(a)), WM_HANG, WPARAM(0), LPARAM(0)) }.unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let pre_worst = probe_host_calls(&mut report, "A hung, before ghosting");
    // Queue input for A so Windows notices the hang and ghosts it (as a user clicking a frozen app).
    click(center(hwnd(a)));
    let hung = wait_for(Duration::from_secs(10), || {
        unsafe { IsHungAppWindow(hwnd(a)) }.as_bool()
    });
    click(center(hwnd(a)));
    let ghost = wait_for(Duration::from_secs(15), || {
        class_name(unsafe { GetForegroundWindow() }) == "Ghost"
    });
    let fg = unsafe { GetForegroundWindow() };
    let (ghost_pid, ghost_exe, ghost_il) = owner(fg);
    let _ = writeln!(
        report,
        "A hung: {hung}; foreground class {:?} ghost: {ghost}; owner pid {ghost_pid} exe {ghost_exe:?} integrity {:?}",
        class_name(fg),
        ghost_il.map(|r| format!("{r:#x}"))
    );
    let ghost_worst = probe_host_calls(&mut report, "ghost foreground");

    let observed = bubbleplay_host::input_stall::observe_foreground(false);
    let ghost_names_a = observed
        .ghost_for
        .as_ref()
        .map(|(pid, class)| (*pid, class.as_str()))
        == Some((std::process::id(), "BpHungA"));
    let _ = writeln!(
        report,
        "stall report: foreground {:?} ghost for {:?} (A: {ghost_names_a})",
        observed.class, observed.ghost_for
    );

    // The session's recovery move: click a healthy window, through the host's input path.
    sink.refresh_foreground_status(); // Reads the admin policy, as the session tick does.
    let ready = wait_for(Duration::from_secs(3), || {
        link.as_ref()
            .is_some_and(|link| link.lock().unwrap().elevated_input_ready())
    });
    let b1 = B_DOWN.load(Ordering::Acquire);
    let h1 = HOOK_DOWN.load(Ordering::Acquire);
    let target = center(hwnd(b));
    let axis = |pixel: i32, origin: i32, extent: u32| {
        (((pixel - origin).max(0) as u64 * 65535) / u64::from(extent.max(2) - 1)) as u16
    };
    let x = axis(target.x, virtual_desktop.x, virtual_desktop.width).to_le_bytes();
    let y = axis(target.y, virtual_desktop.y, virtual_desktop.height).to_le_bytes();
    sink.handle(&[TYPE_MOUSE_ABS, x[0], x[1], y[0], y[1]]);
    sink.handle(&[TYPE_BUTTONS, 1]);
    sink.handle(&[TYPE_BUTTONS, 0]);
    let ghost_b = wait_for(Duration::from_secs(3), || {
        B_DOWN.load(Ordering::Acquire) > b1
    });
    let hook_saw = HOOK_DOWN.load(Ordering::Acquire) > h1;
    let after = unsafe { GetForegroundWindow() };
    let receipts: Vec<String> = link
        .as_ref()
        .map(|link| {
            let mut link = link.lock().unwrap();
            std::iter::from_fn(|| link.take_input_receipt())
                .map(|r| format!("{r:?}"))
                .collect()
        })
        .unwrap_or_default();
    let _ = writeln!(
        report,
        "while ghosted, via host -> helper: helper route ready: {ready}, LL hook saw click: {hook_saw}, click reached B: {ghost_b}, foreground after: {:?} (B: {}), receipts: {receipts:?}",
        class_name(after),
        after == hwnd(b)
    );
    drop(sink);
    drop(link);

    drop(release);
    for window in [a, b] {
        let _ = unsafe { PostMessageW(Some(hwnd(window)), WM_STOP, WPARAM(0), LPARAM(0)) };
    }
    let _ = unsafe { PostThreadMessageW(hook_tid, WM_QUIT, WPARAM(0), LPARAM(0)) };
    a_thread.join().unwrap();
    b_thread.join().unwrap();
    hook_thread.join().unwrap();

    eprintln!("{report}");
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(path) {
            let _ = writeln!(file, "### hung foreground input repro\n```\n{report}```");
        }
    }
    assert!(
        control_b && control_a,
        "runner has no interactive input; repro inconclusive"
    );
    assert!(hung, "the stalled window was never reported hung");
    assert!(
        pre_worst < 200 && ghost_worst < 200,
        "a host-path call blocked on the hung foreground"
    );
    assert!(ghost, "the hung window was never replaced by a ghost");
    assert!(
        ghost_names_a,
        "the stall report did not name the hung window behind the ghost"
    );
    assert!(
        ready,
        "the secure helper never offered the elevated-foreground input route"
    );
    assert!(
        ghost_b,
        "a click on a healthy window was lost while the foreground app was hung"
    );
}

const TYPE_MOUSE_ABS: u8 = 3;
const TYPE_BUTTONS: u8 = 4;

/// Start the installed helper task and take the Host role, as the probe does.
fn connect_helper(
    region: bubbleplay_host::input::DesktopRegion,
) -> Result<bubbleplay_host::secure_helper::client::SecureHelperLink, String> {
    let system = std::env::var("SystemRoot").map_err(|e| e.to_string())?;
    let status = std::process::Command::new(format!(r"{system}\System32\schtasks.exe"))
        .args(["/run", "/tn", r"\BubblePlay\SecureDesktop"])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("schtasks /run failed: {status}"));
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match bubbleplay_host::secure_helper::client::SecureHelperLink::connect_host(region) {
            Ok(link) => return Ok(link),
            Err(e) if Instant::now() >= deadline => return Err(format!("{e:?}")),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}
