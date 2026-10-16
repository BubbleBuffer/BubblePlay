//! VM-only desktop qualification. Every entry point is ignored and needs the
//! explicit environment opt-in. Default cargo test never creates a window,
//! installs a hook, injects input, takes foreground, or clips the pointer here.
use crate::input::Mode;
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Registry::*},
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};
static LIVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static CLIENT_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn opt_in() -> PathBuf {
    assert_eq!(
        std::env::var("BUBBLEPLAY_INPUT_LIVE_TESTS").as_deref(),
        Ok("1"),
        "live input tests require VM 101 opt-in"
    );
    let root = PathBuf::from(
        std::env::var_os("BUBBLEPLAY_INPUT_TEST_DIR")
            .expect("BUBBLEPLAY_INPUT_TEST_DIR points at the copied VM artifacts"),
    );
    assert!(root.is_absolute());
    root
}
struct OwnedChild {
    child: Child,
}
impl OwnedChild {
    fn start(exe: &Path, args: &[String], log: &Path) -> Self {
        let stdout = File::create(log).unwrap();
        let stderr = stdout.try_clone().unwrap();
        let child = Command::new(exe)
            .args(args)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("launch owned fixture process");
        Self { child }
    }
    fn finish(&mut self, limit: Duration) {
        let until = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "fixture process exited {status}");
                return;
            }
            assert!(Instant::now() < until, "fixture failed to exit by itself");
            thread::sleep(Duration::from_millis(30));
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
struct Cleanup {
    game: bool,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.game {
            unsafe {
                let _ = ClipCursor(None);
                SetCursor(LoadCursorW(None, IDC_ARROW).ok());
            }
        }
    }
}
struct FindWindow {
    pid: u32,
    hwnd: HWND,
}
unsafe extern "system" fn enumerate(hwnd: HWND, lp: LPARAM) -> windows::core::BOOL {
    let found = &mut *(lp.0 as *mut FindWindow);
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == found.pid && IsWindowVisible(hwnd).as_bool() {
        found.hwnd = hwnd;
        return false.into();
    }
    true.into()
}
fn client_window(pid: u32) -> HWND {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        let mut found = FindWindow {
            pid,
            hwnd: HWND::default(),
        };
        unsafe {
            let _ = EnumWindows(
                Some(enumerate),
                LPARAM((&mut found as *mut FindWindow) as isize),
            );
        }
        if !found.hwnd.is_invalid() {
            return found.hwnd;
        }
        assert!(Instant::now() < until, "client window did not appear");
        thread::sleep(Duration::from_millis(30));
    }
}
/// Owner pid, class and title of a window, for foreground-failure diagnostics.
fn describe(hwnd: HWND) -> String {
    let mut pid = 0;
    let mut class = [0u16; 128];
    let mut title = [0u16; 128];
    let (class_len, title_len) = unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (GetClassNameW(hwnd, &mut class), GetWindowTextW(hwnd, &mut title))
    };
    format!(
        "{hwnd:?} pid {pid} class {:?} title {:?}",
        String::from_utf16_lossy(&class[..class_len.max(0) as usize]),
        String::from_utf16_lossy(&title[..title_len.max(0) as usize])
    )
}
fn foreground(hwnd: HWND) {
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }
    let until = Instant::now() + Duration::from_secs(2);
    while unsafe { GetForegroundWindow() } != hwnd && Instant::now() < until {
        thread::sleep(Duration::from_millis(20));
    }
    let current = unsafe { GetForegroundWindow() };
    assert!(
        current == hwnd,
        "foreground acquisition failed; refusing SendInput: foreground is {}, wanted {}",
        describe(current),
        describe(hwnd)
    );
}
fn inject(hwnd: HWND, events: &[INPUT]) {
    let current = unsafe { GetForegroundWindow() };
    assert!(
        current == hwnd,
        "foreground changed; refusing SendInput: foreground is {}, wanted {}",
        describe(current),
        describe(hwnd)
    );
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    assert_eq!(
        pid,
        CLIENT_PID.load(std::sync::atomic::Ordering::Acquire),
        "refusing injection outside the owned client process"
    );
    let count = unsafe { SendInput(events, std::mem::size_of::<INPUT>() as i32) };
    assert_eq!(
        count as usize,
        events.len(),
        "SendInput did not inject all events"
    );
}
fn key(vk: u16, scan: u16, extended: bool, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}
fn tap(hwnd: HWND, vk: u16, scan: u16, extended: bool) {
    inject(
        hwnd,
        &[
            key(vk, scan, extended, false),
            key(vk, scan, extended, true),
        ],
    );
    thread::sleep(Duration::from_millis(100));
}
fn logical_scan(hwnd: HWND, vk: u16) -> (u16, bool) {
    let mapped = unsafe {
        let thread = GetWindowThreadProcessId(hwnd, None);
        MapVirtualKeyExW(
            vk as u32,
            MAPVK_VK_TO_VSC_EX,
            Some(GetKeyboardLayout(thread)),
        )
    };
    assert_ne!(
        mapped, 0,
        "escape key cannot be mapped in the client's keyboard layout"
    );
    ((mapped & 255) as u16, mapped & 0xff00 == 0xe000)
}
fn escape(hwnd: HWND, quit: bool) {
    let vk = if quit { 0x51 } else { 0x5a };
    let (scan, extended) = logical_scan(hwnd, vk);
    inject(
        hwnd,
        &[
            key(0xa2, 0x1d, false, false),
            key(0xa4, 0x38, false, false),
            key(0xa0, 0x2a, false, false),
            key(vk, scan, extended, false),
            key(vk, scan, extended, true),
            key(0xa0, 0x2a, false, true),
            key(0xa4, 0x38, false, true),
            key(0xa2, 0x1d, false, true),
        ],
    );
    thread::sleep(Duration::from_millis(300));
}
fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}
fn point_at_video(hwnd: HWND) {
    let mut client = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut client).unwrap();
    }
    let mut center = POINT {
        x: (client.right - client.left) / 2,
        y: (client.bottom - client.top) / 2,
    };
    unsafe {
        assert!(windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut center).as_bool());
    }
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let (x, y) = (
        (center.x as i64 * 65535 / (sw - 1) as i64) as i32,
        (center.y as i64 * 65535 / (sh - 1) as i64) as i32,
    );
    inject(
        hwnd,
        &[mouse(x, y, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE)],
    );
}
fn click(hwnd: HWND) {
    point_at_video(hwnd);
    inject(
        hwnd,
        &[
            mouse(0, 0, 0, MOUSEEVENTF_LEFTDOWN),
            mouse(0, 0, 0, MOUSEEVENTF_LEFTUP),
        ],
    );
    thread::sleep(Duration::from_millis(120));
}
fn records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
fn packet_records(path: &Path) -> Vec<Value> {
    records(path)
        .into_iter()
        .filter(|r| r["type"].is_number())
        .collect()
}
fn release_count(path: &Path) -> usize {
    packet_records(path)
        .iter()
        .filter(|r| r["type"] == 6)
        .count()
}
fn wait_until(started: Instant, seconds: f64) {
    let until = started + Duration::from_secs_f64(seconds);
    while Instant::now() < until {
        thread::sleep(Duration::from_millis(20));
    }
}
fn wait_ready(path: &Path) {
    let until = Instant::now() + Duration::from_secs(8);
    while release_count(path) == 0 {
        assert!(Instant::now() < until, "input gate did not open");
        thread::sleep(Duration::from_millis(30));
    }
}
unsafe extern "system" fn dummy_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}
struct FocusWindow(HWND);
impl FocusWindow {
    fn create() -> Self {
        unsafe {
            let module = GetModuleHandleW(None).unwrap();
            let class = WNDCLASSW {
                hInstance: module.into(),
                lpfnWndProc: Some(dummy_proc),
                lpszClassName: w!("BubblePlay.InputTestFocus"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class.lpszClassName,
                w!("BubblePlay input focus-loss fixture"),
                WS_OVERLAPPEDWINDOW,
                10,
                10,
                200,
                100,
                None,
                None,
                Some(module.into()),
                None,
            )
            .unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            Self(hwnd)
        }
    }
}
impl Drop for FocusWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

fn timeout_ms() -> (Option<u64>, u64) {
    unsafe {
        let mut data = [0u16; 128];
        let mut size = std::mem::size_of_val(&data) as u32;
        let mut kind = REG_VALUE_TYPE::default();
        let result = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Control Panel\\Desktop"),
            w!("LowLevelHooksTimeout"),
            RRF_RT_REG_SZ | RRF_RT_REG_DWORD,
            Some(&mut kind),
            Some(data.as_mut_ptr().cast()),
            Some(&mut size),
        );
        let configured = if result.0 == 0 {
            if kind == REG_DWORD {
                Some(u32::from_le_bytes(
                    std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), 4)
                        .try_into()
                        .unwrap(),
                ) as u64)
            } else {
                String::from_utf16_lossy(&data[..(size as usize / 2).min(data.len())])
                    .trim_end_matches('\0')
                    .parse()
                    .ok()
            }
        } else {
            None
        };
        (configured, configured.unwrap_or(1000))
    }
}
fn has_key(rows: &[Value], scan: u16, extended: bool) -> bool {
    rows.iter().any(|r| {
        r["type"] == 13 && r["decoded"]["scan"] == scan && r["decoded"]["extended"] == extended
    })
}

fn qualification(mode: Mode, stall: bool) {
    let _serialized = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = opt_in();
    let prefix = mode.stream_mode();
    let input_path = root.join(format!("{prefix}-input.jsonl"));
    let report_path = root.join(format!("{prefix}-client.json"));
    let _ = fs::remove_file(&input_path);
    let _ = fs::remove_file(&report_path);
    let port = {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().port()
    };
    let host_args = vec![
        "--streams-dir".into(),
        root.join("streams").display().to_string(),
        "--stream".into(),
        "h264".into(),
        "--port".into(),
        port.to_string(),
        "--exit-after".into(),
        "24".into(),
        "--socket-drop-at".into(),
        "15".into(),
        "--input-record".into(),
        input_path.display().to_string(),
        "--stats".into(),
        root.join(format!("{prefix}-host.json"))
            .display()
            .to_string(),
    ];
    let args = vec![
        "--dev-wss".into(),
        format!("ws://127.0.0.1:{port}"),
        "--dev-input-only".into(),
        "--mode".into(),
        prefix.into(),
        "--exit-after".into(),
        "22".into(),
        "--stats".into(),
        report_path.display().to_string(),
    ];
    let _cleanup = Cleanup {
        game: mode == Mode::Game,
    };
    let mut client = OwnedChild::start(
        &root.join("bubbleplay-client.exe"),
        &args,
        &root.join(format!("{prefix}-client.log")),
    );
    CLIENT_PID.store(client.child.id(), std::sync::atomic::Ordering::Release);
    let hwnd = client_window(client.child.id());
    foreground(hwnd);
    let toggle_scan = logical_scan(hwnd, 0x5a);
    let quit_scan = logical_scan(hwnd, 0x51);
    let mut host = OwnedChild::start(
        &root.join("fake-wss-host.exe"),
        &host_args,
        &root.join(format!("{prefix}-host.log")),
    );
    let launch = Instant::now();
    let until = launch + Duration::from_secs(4);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < until, "fake host did not listen");
        thread::sleep(Duration::from_millis(30));
    }
    wait_ready(&input_path);
    // Launching the host can move the foreground; take it back and re-verify.
    foreground(hwnd);
    click(hwnd);
    tap(hwnd, 0x41, 0x1e, false);
    inject(
        hwnd,
        &[
            mouse(19, -11, 0, MOUSEEVENTF_MOVE),
            mouse(0, 0, 120, MOUSEEVENTF_WHEEL),
            mouse(0, 0, 120, MOUSEEVENTF_HWHEEL),
        ],
    );
    thread::sleep(Duration::from_millis(200));
    if mode == Mode::Game {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            let flags = unsafe {
                SendMessageW(
                    hwnd,
                    crate::session::input_dev::STATUS_MESSAGE,
                    Some(WPARAM(0)),
                    Some(LPARAM(0)),
                )
            }
            .0;
            if flags & 15 == 15 {
                break;
            }
            assert!(
                Instant::now() < until,
                "Game hook/clip/readiness not confirmed; refusing system-key injection"
            );
            thread::sleep(Duration::from_millis(20));
        }
        tap(hwnd, 0x5b, 0x5b, true);
        inject(
            hwnd,
            &[
                key(0xa4, 0x38, false, false),
                key(0x09, 0x0f, false, false),
                key(0x09, 0x0f, false, true),
                key(0xa4, 0x38, false, true),
            ],
        );
        assert_eq!(
            unsafe { GetForegroundWindow() },
            hwnd,
            "Game system keys escaped the hook"
        );
    }
    let release_before = release_count(&input_path);
    escape(hwnd, false);
    assert!(
        release_count(&input_path) > release_before,
        "release chord did not send release-all"
    );
    let count = packet_records(&input_path).len();
    tap(hwnd, 0x42, 0x30, false);
    inject(
        hwnd,
        &[
            mouse(4, 3, 0, MOUSEEVENTF_MOVE),
            mouse(0, 0, 120, MOUSEEVENTF_WHEEL),
        ],
    );
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        packet_records(&input_path).len(),
        count,
        "input was sent while released"
    );
    escape(hwnd, false);
    click(hwnd);
    tap(hwnd, 0x43, 0x2e, false);
    let release_before = release_count(&input_path);
    inject(hwnd, &[key(0x44, 0x20, false, false)]);
    thread::sleep(Duration::from_millis(100));
    let focus = FocusWindow::create();
    foreground(focus.0);
    thread::sleep(Duration::from_millis(300));
    assert!(
        release_count(&input_path) > release_before,
        "focus loss did not send release-all"
    );
    foreground(hwnd);
    inject(hwnd, &[key(0x44, 0x20, false, true)]);
    drop(focus);
    click(hwnd);
    let mut stall_result = Value::Null;
    if stall {
        let (configured, timeout) = timeout_ms();
        assert!(
            timeout <= 5000,
            "configured LowLevelHooksTimeout is too long for this bounded fixture"
        );
        let stall_ms = timeout + 400;
        unsafe {
            PostMessageW(
                Some(hwnd),
                crate::session::input_dev::STALL_MESSAGE,
                WPARAM(stall_ms as usize),
                LPARAM(0),
            )
            .unwrap();
        }
        thread::sleep(Duration::from_millis(100));
        tap(hwnd, 0x45, 0x12, false);
        thread::sleep(Duration::from_millis(100));
        tap(hwnd, 0x46, 0x21, false);
        thread::sleep(Duration::from_millis(stall_ms));
        tap(hwnd, 0x47, 0x22, false);
        let rows = packet_records(&input_path);
        assert!(
            has_key(&rows, 0x12, false)
                && has_key(&rows, 0x21, false)
                && has_key(&rows, 0x22, false),
            "hook did not forward keys during and after stalled UI"
        );
        stall_result = json!({"registry_ms":configured,"assumed_if_absent_ms":1000,"stall_ms":stall_ms,"during_and_after_forwarded":true});
    }
    wait_until(launch, 14.2);
    foreground(hwnd);
    inject(hwnd, &[key(0x48, 0x23, false, false)]);
    let old_epoch = packet_records(&input_path).last().unwrap()["epoch"].clone();
    let reconnect_deadline = Instant::now() + Duration::from_secs(7);
    loop {
        let all = records(&input_path);
        let dropped = all
            .iter()
            .any(|r| r["event"] == "disconnect" && r["reason"] == "socket-drop");
        let resumed = all
            .iter()
            .any(|r| r["type"] == 6 && r["epoch"] != old_epoch);
        if dropped && resumed {
            break;
        }
        assert!(
            Instant::now() < reconnect_deadline,
            "socket-drop lifecycle and new ready epoch did not arrive"
        );
        thread::sleep(Duration::from_millis(30));
    }
    foreground(hwnd);
    inject(hwnd, &[key(0x48, 0x23, false, true)]);
    click(hwnd);
    tap(hwnd, 0x49, 0x17, false);
    inject(hwnd, &[mouse(0, 0, 120, MOUSEEVENTF_WHEEL)]);
    thread::sleep(Duration::from_millis(120));
    wait_until(launch, 18.2);
    escape(hwnd, true);
    client.finish(Duration::from_secs(5));
    // The host's own --exit-after 24 counts from when it has loaded the stream
    // and listens, which can be seconds after `launch` on the VM; allow for it.
    host.finish(Duration::from_secs(15));
    let report: Value = serde_json::from_str(&fs::read_to_string(&report_path).unwrap()).unwrap();
    let rows = packet_records(&input_path);
    let lifecycle = records(&input_path);
    let dropped = lifecycle
        .iter()
        .find(|r| r["event"] == "disconnect" && r["reason"] == "socket-drop")
        .expect("recorded real socket drop");
    assert!(
        dropped["held_before"]["keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|key| key["scan"] == 0x23),
        "socket-drop fixture did not hold H"
    );
    assert!(
        dropped["held_after"]["keys"].as_array().unwrap().is_empty()
            && dropped["held_after"]["buttons"] == 0,
        "disconnect left input held"
    );
    assert!(rows.iter().all(|r| r["valid"] == true));
    assert!(
        has_key(&rows, 0x1e, false) && has_key(&rows, 0x2e, false) && has_key(&rows, 0x17, false)
    );
    assert!(
        !has_key(&rows, toggle_scan.0, toggle_scan.1) && !has_key(&rows, quit_scan.0, quit_scan.1),
        "escape terminal keys were forwarded"
    );
    assert!(!has_key(&rows, 0x30, false), "released key B was forwarded");
    assert!(
        !has_key(&rows, 0x1d, false) && !has_key(&rows, 0x2a, false),
        "fresh escape Ctrl/Shift presses were forwarded"
    );
    let sequenced: Vec<_> = rows.iter().filter(|r| r["seq"].is_number()).collect();
    let mut last = std::collections::BTreeMap::<String, u16>::new();
    for row in sequenced {
        let epoch = row["epoch"].as_str().unwrap().to_owned();
        let seq = row["seq"].as_u64().unwrap() as u16;
        if let Some(previous) = last.insert(epoch, seq) {
            assert!(
                seq.wrapping_sub(previous) > 0 && seq.wrapping_sub(previous) < 32768,
                "non-monotonic input sequence"
            );
        } else {
            assert_eq!(seq, 0, "new epoch did not reset sequence");
        }
    }
    assert!(last.len() >= 2, "socket drop did not reconnect");
    assert!(rows.last().is_some_and(|r| r["held"]["keys"]
        .as_array()
        .is_some_and(|keys| keys.is_empty())
        && r["held"]["buttons"] == 0));
    assert!(
        report["elapsed_ms"].as_f64().unwrap() < 21000.0,
        "quit chord did not leave before timed exit"
    );
    assert_eq!(report["capture_state"]["hook_installed"], false);
    assert_eq!(report["capture_state"]["clipped"], false);
    assert!(
        report["input"]["cursor_states"].as_u64().unwrap() > 0
            && report["input"]["cursor_kinds"].as_u64().unwrap() > 0
    );
    let events = report["capture_events"].as_array().unwrap();
    if mode == Mode::Work {
        assert!(
            !has_key(&rows, 0x38, false),
            "fresh escape Alt was forwarded"
        );
        assert!(rows.iter().any(|r| r["type"] == 8) && rows.iter().any(|r| r["type"] == 9));
        assert!(events
            .iter()
            .all(|e| e["state"]["hook_installed"] == false && e["state"]["clipped"] == false));
    } else {
        assert!(rows.iter().any(|r| r["type"] == 7));
        assert!(!rows.iter().any(|r| r["type"] == 8 || r["type"] == 9));
        assert!(has_key(&rows, 0x5b, true) && has_key(&rows, 0x0f, false));
        assert!(events
            .iter()
            .any(|e| e["state"]["hook_installed"] == true && e["state"]["clipped"] == true));
    }
    let raw_relative = rows.iter().any(|r| {
        r["type"] == 7
            && (r["decoded"]["dx"].as_i64().unwrap_or(0) != 0
                || r["decoded"]["dy"].as_i64().unwrap_or(0) != 0)
    });
    let outcome = json!({"mode":prefix,"packets":rows.len(),"epochs":last.len(),"mode_packets":true,"monotonic_sequences":true,"release_chord":true,"focus_loss_release":true,"no_input_while_released":true,"quit_chord":true,"no_keys_left_held_at_exit":true,"cursor_128_129":true,"hook_stall":stall_result,"raw_relative_observed":raw_relative,"work_system_keys":"not injected: Windows could change foreground mid-chord; no-hook/no-clip state asserted","socket_drop_release":"both sockets are dropped: no outgoing packet can be guaranteed; new-epoch release and disconnect cleanup are asserted"});
    fs::write(
        root.join(format!("{prefix}-assertions.json")),
        serde_json::to_string_pretty(&outcome).unwrap(),
    )
    .unwrap();
    println!("{outcome}");
}

#[test]
#[ignore = "VM 101 interactive desktop only: foreground and SendInput"]
fn work_wss_input_e2e() {
    qualification(Mode::Work, false);
}

#[test]
#[ignore = "VM 101 interactive desktop only: hook, foreground, SendInput and ClipCursor"]
fn game_wss_input_e2e_and_hook_survival() {
    qualification(Mode::Game, true);
}
