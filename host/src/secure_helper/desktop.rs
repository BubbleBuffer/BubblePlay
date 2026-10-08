//! All USER/GDI/input operations occur on a fresh desktop-bound worker.
use super::{
    protocol::{self, Kind, Region},
    trust,
};
use crate::{
    admin_agent::{
        protocol::{Record, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE},
        session::{HeldInput, RateLimiter},
    },
    desktop_session::{DesktopKind, InputDesktop},
};
use anyhow::{ensure, Result};
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc,
    },
    time::{Duration, Instant},
};
use windows::{
    core::BOOL,
    Win32::{
        Foundation::{GetLastError, SetLastError, HANDLE, HWND, LPARAM, WIN32_ERROR, WPARAM},
        Graphics::Gdi::*,
        System::{
            Com::CoTaskMemFree,
            StationsAndDesktops::{EnumDesktopWindows, GetThreadDesktop},
            Threading::GetCurrentThreadId,
        },
        UI::{
            Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
            HiDpi::{
                AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext,
                SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            },
            Input::KeyboardAndMouse::*,
            Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT},
            WindowsAndMessaging::{
                DispatchMessageW, DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics,
                GetWindowThreadProcessId, PeekMessageW, PostMessageW, CURSORINFO, CURSOR_SHOWING,
                DI_NORMAL, EVENT_SYSTEM_DESKTOPSWITCH, HICON, ICONINFO, MSG, PM_REMOVE,
                SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
                WINEVENT_OUTOFCONTEXT, WM_CLOSE, WM_COMMAND,
            },
        },
    },
};
static DESKTOP_SWITCHES: AtomicU64 = AtomicU64::new(1);
/// Physical DXGI rectangles, Winlogon metrics, GDI and cursor positions must
/// share one coordinate space even with mixed monitor scaling. Thread-local
/// scope avoids changing the normal host or the helper's watcher/controller.
struct PhysicalCoordinates(DPI_AWARENESS_CONTEXT);
// The generated generic is_invalid also rejects -1, which is Windows' valid
// DPI_AWARENESS_CONTEXT_UNAWARE pseudo-handle. This API fails only with NULL.
fn dpi_context_succeeded(context: DPI_AWARENESS_CONTEXT) -> bool {
    !context.0.is_null()
}
impl PhysicalCoordinates {
    fn enter(epoch: u64) -> Result<Self> {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if !dpi_context_succeeded(previous) {
            let error_code = unsafe { GetLastError() }.0;
            tracing::warn!(
                epoch,
                physical_coordinates = false,
                error_code,
                "secure worker DPI binding failed"
            );
            anyhow::bail!("cannot establish secure worker physical DPI coordinates");
        }
        let scope = Self(previous);
        let physical = unsafe {
            AreDpiAwarenessContextsEqual(
                GetThreadDpiAwarenessContext(),
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
        }
        .as_bool();
        tracing::info!(
            epoch,
            physical_coordinates = physical,
            dpi_context = "per-monitor-v2",
            "secure worker DPI binding"
        );
        ensure!(
            physical,
            "secure worker DPI context did not become per-monitor-v2"
        );
        Ok(scope)
    }
}
impl Drop for PhysicalCoordinates {
    fn drop(&mut self) {
        let restored = unsafe { SetThreadDpiAwarenessContext(self.0) };
        if !dpi_context_succeeded(restored) {
            let error_code = unsafe { GetLastError() }.0;
            tracing::warn!(error_code, "secure worker DPI context restoration failed");
        }
    }
}
pub struct SwitchWatcher {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for SwitchWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
impl SwitchWatcher {
    pub fn new() -> Result<Self> {
        unsafe extern "system" fn switched(
            _: HWINEVENTHOOK,
            event: u32,
            _: HWND,
            _: i32,
            _: i32,
            _: u32,
            _: u32,
        ) {
            if event == EVENT_SYSTEM_DESKTOPSWITCH {
                DESKTOP_SWITCHES.fetch_add(1, Ordering::AcqRel);
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let (s, r) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let hook = unsafe {
                SetWinEventHook(
                    EVENT_SYSTEM_DESKTOPSWITCH,
                    EVENT_SYSTEM_DESKTOPSWITCH,
                    None,
                    Some(switched),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            let _ = s.send(!hook.is_invalid());
            if hook.is_invalid() {
                return;
            }
            let mut message = MSG::default();
            while !st.load(Ordering::Acquire) {
                while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                    unsafe { DispatchMessageW(&message) };
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = unsafe { UnhookWinEvent(hook) };
        });
        ensure!(r.recv().unwrap_or(false), "desktop switch hook unavailable");
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}
pub enum Command {
    Input { id: u64, records: Vec<Record> },
    Deny(u64),
}
pub enum Event {
    State {
        secure: bool,
        kind: Kind,
        admin: bool,
        available: bool,
        epoch: u64,
    },
    Pixels {
        input_id: u64,
        region: Region,
        pixels: Vec<u8>,
        hash: u64,
        epoch: u64,
    },
    Denied {
        id: u64,
        ok: bool,
        reason: String,
    },
}
pub struct Worker {
    pub send: SyncSender<Command>,
    pub recv: Receiver<Event>,
    thread: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    pub receipts: Receiver<protocol::InputReceipt>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
impl Worker {
    pub fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
    pub fn spawn(session: u32, sid: String, region: Region, epoch: u64) -> Self {
        let (s, r) = mpsc::sync_channel(32);
        let (es, er) = mpsc::sync_channel(4);
        let (receipt_send, receipts) = mpsc::sync_channel(32);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let t = std::thread::spawn(move || {
            if let Err(e) = run(
                session,
                &sid,
                region,
                epoch,
                r,
                es,
                thread_stop,
                receipt_send,
            ) {
                tracing::warn!(error=%e,"secure desktop worker unavailable");
            }
        });
        Self {
            send: s,
            recv: er,
            thread: Some(t),
            stop,
            receipts,
        }
    }
    pub fn done(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}
/// Re-resolve the console identity/profile on every request; SYSTEM never
/// consults its own LOCALAPPDATA or a path supplied by the pipe client.
pub fn admin_mode(sid: &str) -> bool {
    let Ok((token, user)) = trust::console_user() else {
        return false;
    };
    if user != sid {
        return false;
    }
    let Ok(p) =
        (unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, Some(token.0)) })
    else {
        return false;
    };
    let path = unsafe { p.to_string() };
    unsafe { CoTaskMemFree(Some(p.0.cast())) };
    let Ok(path) = path else {
        return false;
    };
    let path = std::path::PathBuf::from(path)
        .join("BubblePlay")
        .join("tray.json");
    // Cap a user-writable config before parsing; never log its contents.
    let Ok(f) = std::fs::File::open(path) else {
        return false;
    };
    use std::io::Read;
    let mut b = Vec::new();
    if f.take(65537).read_to_end(&mut b).is_err() || b.len() > 65536 {
        return false;
    }
    parse_admin_mode(&b)
}
fn parse_admin_mode(b: &[u8]) -> bool {
    if b.len() > 65536 {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(b)
        .ok()
        .and_then(|v| v.get("adminMode").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}
fn inject(records: &[Record]) -> (usize, u32) {
    let v: Vec<INPUT> = records
        .iter()
        .map(|r| match *r {
            Record::Mouse {
                dx,
                dy,
                data,
                flags,
            } => INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx,
                        dy,
                        mouseData: data,
                        dwFlags: MOUSE_EVENT_FLAGS(flags),
                        ..Default::default()
                    },
                },
            },
            Record::Key { vk, scan, flags } => INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(vk),
                        wScan: scan,
                        dwFlags: KEYBD_EVENT_FLAGS(flags),
                        ..Default::default()
                    },
                },
            },
        })
        .collect();
    unsafe { SetLastError(WIN32_ERROR(0)) };
    let applied = unsafe { SendInput(&v, std::mem::size_of::<INPUT>() as i32) } as usize;
    let error = if applied < records.len() {
        unsafe { GetLastError() }.0
    } else {
        0
    };
    (applied, error)
}
#[derive(Default)]
struct Held {
    ordinary: HeldInput,
    unicode: BTreeSet<u16>,
}
impl Held {
    fn observe(&mut self, r: &Record) {
        if let Record::Key { scan, flags, .. } = *r {
            if flags & KEYEVENTF_UNICODE != 0 {
                if flags & KEYEVENTF_KEYUP != 0 {
                    self.unicode.remove(&scan);
                } else {
                    self.unicode.insert(scan);
                }
                return;
            }
        }
        self.ordinary.observe(r);
    }
    fn take_release(&mut self) -> Vec<Record> {
        let mut r = self.ordinary.take_release();
        r.extend(
            std::mem::take(&mut self.unicode)
                .into_iter()
                .map(|scan| Record::Key {
                    vk: 0,
                    scan,
                    flags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                }),
        );
        r
    }
    fn release(&mut self) {
        let r = self.take_release();
        if !r.is_empty() {
            inject(&r);
        }
    }
}
fn current_secure() -> bool {
    InputDesktop::open().is_ok_and(|d| d.kind() == DesktopKind::Secure)
}
fn consent_windows(session: u32) -> Result<Vec<HWND>> {
    struct Context {
        session: u32,
        expected: String,
        windows: Vec<HWND>,
        failed: bool,
    }
    unsafe extern "system" fn visit(w: HWND, p: LPARAM) -> BOOL {
        let c = unsafe { &mut *(p.0 as *mut Context) };
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(w, Some(&mut pid)) };
        let image = crate::admin_agent::trust::process_image_path(pid);
        let session = crate::admin_agent::trust::process_session_id(pid);
        if let (Some(image), Some(session)) = (image, session) {
            if protocol::consent_matches(
                &image.to_string_lossy(),
                Some(session),
                &c.expected,
                c.session,
            ) {
                c.windows.push(w);
            }
        } else {
            c.failed = true;
        }
        true.into()
    }
    let root =
        std::env::var_os("SystemRoot").ok_or_else(|| anyhow::anyhow!("SystemRoot missing"))?;
    let mut c = Context {
        session,
        expected: std::path::PathBuf::from(root)
            .join("System32")
            .join("consent.exe")
            .to_string_lossy()
            .into_owned(),
        windows: Vec::new(),
        failed: false,
    };
    let d = unsafe { GetThreadDesktop(GetCurrentThreadId()) }?;
    unsafe {
        EnumDesktopWindows(
            Some(d),
            Some(visit),
            LPARAM((&mut c as *mut Context) as isize),
        )
    }?;
    ensure!(!c.failed, "consent window identity query failed");
    Ok(c.windows)
}
fn still_consent(w: HWND, session: u32) -> bool {
    current_secure() && consent_windows(session).is_ok_and(|windows| windows.contains(&w))
}
fn deny(session: u32, generation: u64) -> (bool, String) {
    fn attempt(session: u32, generation: u64) -> Result<(bool, String)> {
        ensure!(current_secure(), "desktop-changed");
        let targets = consent_windows(session)?;
        if targets.is_empty() {
            return Ok((false, "no-consent-window".into()));
        }
        for w in &targets {
            ensure!(
                DESKTOP_SWITCHES.load(Ordering::Acquire) == generation,
                "desktop transition during deny"
            );
            ensure!(still_consent(*w, session), "consent identity changed");
            unsafe { PostMessageW(Some(*w), WM_CLOSE, WPARAM(0), LPARAM(0)) }?;
        }
        let wait = || -> Result<bool> {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if consent_windows(session)?.is_empty() {
                    return Ok(true);
                }
                ensure!(
                    DESKTOP_SWITCHES.load(Ordering::Acquire) == generation,
                    "desktop transition during deny"
                );
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        };
        if wait()? {
            return Ok((true, "closed".into()));
        }
        let remaining = consent_windows(session)?;
        for w in remaining {
            if targets.contains(&w) {
                ensure!(
                    DESKTOP_SWITCHES.load(Ordering::Acquire) == generation,
                    "desktop transition during deny"
                );
                ensure!(still_consent(w, session), "consent identity changed");
                unsafe { PostMessageW(Some(w), WM_COMMAND, WPARAM(2), LPARAM(0)) }?;
            }
        }
        Ok(if wait()? {
            (true, "cancelled".into())
        } else {
            (false, "consent-window-remains".into())
        })
    }
    attempt(session, generation).unwrap_or((false, "consent-verification-failed".into()))
}
fn capture(r: Region) -> Result<Vec<u8>> {
    r.validate()?;
    let len = protocol::frame_len(r.width, r.height, r.width * 4)?;
    let source = unsafe { GetDC(None) };
    ensure!(!source.is_invalid(), "GetDC");
    let memory = unsafe { CreateCompatibleDC(Some(source)) };
    if memory.is_invalid() {
        unsafe { ReleaseDC(None, source) };
        anyhow::bail!("CreateCompatibleDC");
    }
    let result = (|| {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: r.width as i32,
                biHeight: -(r.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: len as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = unsafe {
            CreateDIBSection(
                Some(memory),
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                None::<HANDLE>,
                0,
            )
        }?;
        let previous = unsafe { SelectObject(memory, bitmap.into()) };
        let result = (|| {
            ensure!(!bits.is_null() && !previous.is_invalid(), "DIB selection");
            unsafe {
                BitBlt(
                    memory,
                    0,
                    0,
                    r.width as i32,
                    r.height as i32,
                    Some(source),
                    r.x,
                    r.y,
                    SRCCOPY | CAPTUREBLT,
                )
            }?;
            composite_cursor(memory, r);
            ensure!(unsafe { GdiFlush() }.as_bool(), "GdiFlush");
            let mut p = vec![0; len];
            unsafe { std::ptr::copy_nonoverlapping(bits.cast::<u8>(), p.as_mut_ptr(), len) };
            Ok(p)
        })();
        if !previous.is_invalid() {
            unsafe { SelectObject(memory, previous) };
        }
        let _ = unsafe { DeleteObject(bitmap.into()) };
        result
    })();
    let _ = unsafe { DeleteDC(memory) };
    unsafe { ReleaseDC(None, source) };
    result
}
fn composite_cursor(dc: HDC, region: Region) {
    let mut cursor = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetCursorInfo(&mut cursor) }.is_err() || cursor.flags != CURSOR_SHOWING {
        return;
    }
    let mut icon = ICONINFO::default();
    if unsafe { GetIconInfo(HICON(cursor.hCursor.0), &mut icon) }.is_err() {
        return;
    }
    let x = i64::from(cursor.ptScreenPos.x) - i64::from(icon.xHotspot) - i64::from(region.x);
    let y = i64::from(cursor.ptScreenPos.y) - i64::from(icon.yHotspot) - i64::from(region.y);
    if let (Ok(x), Ok(y)) = (i32::try_from(x), i32::try_from(y)) {
        let _ = unsafe { DrawIconEx(dc, x, y, HICON(cursor.hCursor.0), 0, 0, 0, None, DI_NORMAL) };
    }
    if !icon.hbmColor.is_invalid() {
        let _ = unsafe { DeleteObject(icon.hbmColor.into()) };
    }
    if !icon.hbmMask.is_invalid() {
        let _ = unsafe { DeleteObject(icon.hbmMask.into()) };
    }
}
fn run(
    session: u32,
    sid: &str,
    region: Region,
    epoch: u64,
    commands: Receiver<Command>,
    events: SyncSender<Event>,
    stopping: Arc<AtomicBool>,
    receipts: SyncSender<protocol::InputReceipt>,
) -> Result<()> {
    let switch_generation = DESKTOP_SWITCHES.load(Ordering::Acquire);
    let desktop = InputDesktop::open()?;
    if desktop.kind() != DesktopKind::Secure {
        let _ = events.try_send(Event::State {
            secure: false,
            kind: Kind::Other,
            admin: false,
            available: false,
            epoch,
        });
        return Ok(());
    }
    let _bound = desktop.bind_thread()?;
    // Bind before any metric/GDI/cursor query and keep the scope through held
    // input release. USER/GDI objects are dropped before desktop restoration.
    let _physical_coordinates = PhysicalCoordinates::enter(epoch)?;
    let identity = _bound.security_context()?;
    tracing::info!(epoch,system=identity.token_is_local_system,session=identity.token_session_id,integrity=identity.integrity,desktop=%identity.thread_desktop,window_station=%identity.process_window_station,ui_access=identity.ui_access,desktop_access=identity.desktop_input_access,"secure input worker binding");
    ensure!(
        identity.token_is_local_system
            && identity.token_session_id == session
            && identity.thread_desktop.eq_ignore_ascii_case("winlogon"),
        "invalid secure input binding"
    );
    let mut held = Held::default();
    let mut admin = false;
    let mut last_hash = None;
    let mut sent = Instant::now() - Duration::from_secs(2);
    let mut capture_at = Instant::now();
    let mut available = false;
    let mut frames = RateLimiter::new(2000., 2000., Instant::now());
    let mut records = RateLimiter::new(8192., 8192., Instant::now());
    let mut last_state = None;
    let mut last_applied_input_id = 0;
    // Held is released before the bound-desktop guard is dropped on every exit.
    let result = (|| {
        loop {
            if stopping.load(Ordering::Acquire)
                || DESKTOP_SWITCHES.load(Ordering::Acquire) != switch_generation
                || trust::console() != session
                || !current_secure()
            {
                break;
            }
            let policy = admin_mode(sid);
            if policy != admin {
                held.release();
                admin = policy;
            }
            let mut stop = false;
            for _ in 0..32 {
                let c = match commands.try_recv() {
                    Ok(c) => c,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        stop = true;
                        break;
                    }
                };
                let policy = admin_mode(sid);
                if policy != admin {
                    held.release();
                    admin = policy;
                }
                if stopping.load(Ordering::Acquire)
                    || DESKTOP_SWITCHES.load(Ordering::Acquire) != switch_generation
                    || !current_secure()
                {
                    stop = true;
                    break;
                }
                match c {
                    Command::Input { id, records: r } => {
                        let now = Instant::now();
                        let received = r.len() as u16;
                        let reject = if !admin {
                            Some("policy-off")
                        } else if !available {
                            Some("capture-unavailable")
                        } else if r.len() > 1024 || r.iter().any(|r| r.validate().is_err()) {
                            Some("invalid-records")
                        } else if !frames.take(1., now) || !records.take(r.len() as f64, now) {
                            Some("input-rate-limit")
                        } else {
                            None
                        };
                        let receipt = if let Some(reason) = reject {
                            if admin && reason != "invalid-records" {
                                protocol::InputReceipt::rejected_after_policy(id, received, reason)
                            } else {
                                protocol::InputReceipt::rejected(id, received, reason)
                            }
                        } else {
                            let virtual_screen = Region {
                                x: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
                                y: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
                                width: unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) } as u32,
                                height: unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) } as u32,
                            };
                            match r
                                .iter()
                                .copied()
                                .map(|r| protocol::map_absolute(r, region, virtual_screen))
                                .collect::<std::io::Result<Vec<_>>>()
                            {
                                Ok(mapped) => {
                                    let (applied, error_code) = inject(&mapped);
                                    let applied = applied.min(mapped.len());
                                    if applied > 0 {
                                        last_applied_input_id = id;
                                        last_hash = None;
                                        capture_at = Instant::now();
                                    }
                                    for record in &mapped[..applied] {
                                        held.observe(record);
                                    }
                                    protocol::InputReceipt {
                                        id,
                                        received,
                                        accepted: received,
                                        applied: applied as u16,
                                        error_code,
                                        reason: if applied == mapped.len() {
                                            "ok"
                                        } else {
                                            "partial-send-input"
                                        }
                                        .into(),
                                    }
                                }
                                Err(_) => protocol::InputReceipt::rejected_after_policy(
                                    id,
                                    received,
                                    "invalid-geometry",
                                ),
                            }
                        };
                        receipts
                            .try_send(receipt)
                            .map_err(|_| anyhow::anyhow!("input receipt queue unavailable"))?;
                    }
                    Command::Deny(id) => {
                        tracing::info!(epoch, "secure consent deny requested");
                        let (ok, reason) = deny(session, switch_generation);
                        tracing::info!(epoch,ok,reason=%reason,"secure consent deny completed");
                        let _ = events.try_send(Event::Denied { id, ok, reason });
                        last_hash = None;
                    }
                }
            }
            if stop {
                break;
            }
            if Instant::now() >= capture_at {
                capture_at = Instant::now() + Duration::from_millis(100);
                if !trust::console_user().is_ok_and(|(_, user)| user == sid) {
                    break;
                }
                let kind = if consent_windows(session).is_ok_and(|windows| !windows.is_empty()) {
                    Kind::Uac
                } else if crate::desktop_session::session_locked() {
                    Kind::Lock
                } else {
                    Kind::Other
                };
                match capture(region) {
                    Ok(pixels) => {
                        if DESKTOP_SWITCHES.load(Ordering::Acquire) != switch_generation
                            || !current_secure()
                        {
                            break;
                        }
                        available = true;
                        if last_state != Some((kind, admin, available)) {
                            tracing::info!(
                                epoch,
                                kind = kind.name(),
                                admin_mode = admin,
                                available,
                                "secure desktop state changed"
                            );
                            last_state = Some((kind, admin, available));
                        }
                        let h = protocol::hash(&pixels);
                        let _ = events.try_send(Event::State {
                            secure: true,
                            kind,
                            admin,
                            available,
                            epoch,
                        });
                        if last_hash != Some(h) || sent.elapsed() >= Duration::from_secs(1) {
                            if events
                                .try_send(Event::Pixels {
                                    input_id: last_applied_input_id,
                                    region,
                                    pixels,
                                    hash: h,
                                    epoch,
                                })
                                .is_ok()
                            {
                                last_hash = Some(h);
                                sent = Instant::now();
                            }
                        }
                    }
                    Err(_) => {
                        available = false;
                        if last_state != Some((kind, admin, available)) {
                            tracing::info!(
                                epoch,
                                kind = kind.name(),
                                admin_mode = admin,
                                available,
                                "secure desktop state changed"
                            );
                            last_state = Some((kind, admin, available));
                        }
                        let _ = events.try_send(Event::State {
                            secure: true,
                            kind,
                            admin,
                            available,
                            epoch,
                        });
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    })();
    held.release();
    tracing::info!(epoch, "secure desktop worker left input desktop");
    let _ = events.try_send(Event::State {
        secure: false,
        kind: Kind::Other,
        admin: false,
        available: false,
        epoch,
    });
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dpi_unaware_previous_context_is_a_valid_success() {
        use windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_UNAWARE;
        assert!(dpi_context_succeeded(DPI_AWARENESS_CONTEXT_UNAWARE));
        assert!(dpi_context_succeeded(
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
        ));
        assert!(!dpi_context_succeeded(DPI_AWARENESS_CONTEXT::default()));
    }
    #[test]
    fn held_unicode_is_released() {
        let mut h = Held::default();
        h.observe(&Record::Key {
            vk: 0,
            scan: 0x20ac,
            flags: KEYEVENTF_UNICODE,
        });
        assert!(h.unicode.contains(&0x20ac));
        h.observe(&Record::Key {
            vk: 0,
            scan: 0x20ac,
            flags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
        });
        assert!(h.unicode.is_empty());
        h.observe(&Record::Key {
            vk: 0,
            scan: 0xd83d,
            flags: KEYEVENTF_UNICODE,
        });
        h.observe(&Record::Key {
            vk: 0x41,
            scan: 0,
            flags: 0,
        });
        let release = h.take_release();
        assert!(release.contains(&Record::Key {
            vk: 0,
            scan: 0xd83d,
            flags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
        }));
        assert!(release.contains(&Record::Key {
            vk: 0x41,
            scan: 0,
            flags: KEYEVENTF_KEYUP
        }));
        assert!(release.iter().all(|r| r.validate().is_ok()));
        assert!(h.take_release().is_empty());
    }
    #[test]
    fn unreadable_or_invalid_policy_is_off() {
        assert!(parse_admin_mode(br#"{"adminMode":true}"#));
        for b in [
            &b""[..],
            &b"{"[..],
            &br#"{}"#[..],
            &br#"{"adminMode":false}"#[..],
            &br#"{"adminMode":"true"}"#[..],
        ] {
            assert!(!parse_admin_mode(b));
        }
        assert!(!parse_admin_mode(&vec![b' '; 65537]));
    }
    #[test]
    fn full_queues_cannot_block_teardown() {
        let (s, r) = mpsc::sync_channel(1);
        s.try_send(Command::Deny(1)).unwrap();
        let (es, er) = mpsc::sync_channel(1);
        es.try_send(Event::Denied {
            id: 1,
            ok: false,
            reason: "fixture".into(),
        })
        .ok();
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let t = std::thread::spawn(move || {
            while !st.load(Ordering::Acquire) {
                let _ = es.try_send(Event::Denied {
                    id: 1,
                    ok: false,
                    reason: "full".into(),
                });
                std::thread::yield_now();
            }
            drop(r);
        });
        let w = Worker {
            send: s,
            recv: er,
            thread: Some(t),
            stop,
            receipts: mpsc::sync_channel(1).1,
        };
        let now = Instant::now();
        drop(w);
        assert!(now.elapsed() < Duration::from_secs(1));
    }
}
