//! Browser presence approval on the existing signaling connection.
//! The connection owns decisions; a single process-wide UI thread owns windows.
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender, TryRecvError},
    Arc, OnceLock,
};
use std::time::{SystemTime, UNIX_EPOCH};

pub const ID_MIN: usize = 22;
pub const ID_MAX: usize = 64;
pub const AGENT_MAX: usize = 80;
const MAX_WIRE_BYTES: usize = 1_024;
const MAX_UNIX_MS: u64 = 8_640_000_000_000_000; // JavaScript Date's finite range.

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalRequest {
    pub id: String,
    pub client: String,
    pub agent: String,
    pub expires_at: u64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
enum Incoming {
    ApprovalRequest {
        id: String,
        client: String,
        agent: String,
        #[serde(rename = "expiresAt")]
        expires_at: u64,
    },
    ApprovalCancel {
        id: String,
    },
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ApprovalDecision {
    #[serde(rename = "type")]
    kind: &'static str,
    pub id: String,
    pub approve: bool,
}

fn decision(id: String, approve: bool) -> ApprovalDecision {
    ApprovalDecision {
        kind: "approval-decision",
        id,
        approve,
    }
}

fn valid_id(id: &str) -> bool {
    (ID_MIN..=ID_MAX).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl ApprovalRequest {
    fn valid(&self) -> bool {
        if !valid_id(&self.id)
            || self.agent.is_empty()
            || self.agent.len() > AGENT_MAX
            || !self.agent.bytes().all(|b| (0x20..=0x7e).contains(&b))
            || self.client.is_empty()
            || self.client.len() > 80
            || !self.client.bytes().all(|b| (0x20..=0x7e).contains(&b))
            || self.expires_at == 0
            || self.expires_at > MAX_UNIX_MS
        {
            return false;
        }
        // Relays use different prefix summaries (/24, /56, truncated IPv6,
        // or "unknown"). Treat this bounded printable label as untrusted text.
        true
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// Presence approval belongs to the per-user host on WinSta0\Default. A
/// failed probe is ineligible; legacy service transport never advertises it.
fn capability_eligible(
    service_mode: bool,
    session_id: Option<u32>,
    window_station_name: Option<&str>,
    window_station_visible: bool,
    desktop_name: Option<&str>,
    desktop_can_create_window: bool,
) -> bool {
    !service_mode
        && session_id.is_some_and(|session| session != 0)
        && window_station_name.is_some_and(|name| name.eq_ignore_ascii_case("WinSta0"))
        && window_station_visible
        && desktop_name.is_some_and(|name| name.eq_ignore_ascii_case("Default"))
        && desktop_can_create_window
}

pub fn capability_available(service_mode: bool) -> bool {
    !service_mode && interactive_desktop_available()
}

#[cfg(not(windows))]
fn interactive_desktop_available() -> bool {
    false
}

#[cfg(windows)]
fn interactive_desktop_available() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW,
        OpenDesktopW, DESKTOP_CONTROL_FLAGS, DESKTOP_CREATEWINDOW, DESKTOP_READOBJECTS,
        DESKTOP_WRITEOBJECTS, UOI_FLAGS, UOI_NAME, USEROBJECTFLAGS,
    };
    use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::WSF_VISIBLE;

    fn object_name(handle: HANDLE) -> Option<String> {
        // Object names beyond this bounded buffer fail closed.
        let mut buffer = [0u16; 256];
        unsafe {
            GetUserObjectInformationW(
                handle,
                UOI_NAME,
                Some(buffer.as_mut_ptr().cast()),
                std::mem::size_of_val(&buffer) as u32,
                None,
            )
        }
        .ok()?;
        let end = buffer.iter().position(|unit| *unit == 0)?;
        String::from_utf16(&buffer[..end]).ok()
    }

    // Query only inherited USER objects; do not create a window, switch a
    // desktop, or require Default to be the current input desktop. Lock/UAC
    // transitions must not permanently suppress a normal per-user host.
    (|| -> Option<bool> {
        let mut session_id = 0;
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) }.ok()?;
        if session_id == 0 {
            return Some(false);
        }
        let station = unsafe { GetProcessWindowStation() }.ok()?;
        let station_name = object_name(HANDLE(station.0))?;
        let mut flags = USEROBJECTFLAGS::default();
        unsafe {
            GetUserObjectInformationW(
                HANDLE(station.0),
                UOI_FLAGS,
                Some((&mut flags as *mut USEROBJECTFLAGS).cast()),
                std::mem::size_of::<USEROBJECTFLAGS>() as u32,
                None,
            )
        }
        .ok()?;
        let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.ok()?;
        let desktop_name = object_name(HANDLE(desktop.0))?;
        if !capability_eligible(
            false,
            Some(session_id),
            Some(&station_name),
            flags.dwFlags & WSF_VISIBLE as u32 != 0,
            Some(&desktop_name),
            true,
        ) {
            return Some(false);
        }
        let access = DESKTOP_CREATEWINDOW.0 | DESKTOP_READOBJECTS.0 | DESKTOP_WRITEOBJECTS.0;
        let opened =
            unsafe { OpenDesktopW(w!("Default"), DESKTOP_CONTROL_FLAGS(0), false, access) }.ok()?;
        // GetProcessWindowStation/GetThreadDesktop are borrowed. Only close
        // the separate handle opened for this rights check.
        let can_create = unsafe { CloseDesktop(opened) }.is_ok();
        Some(capability_eligible(
            false,
            Some(session_id),
            Some(&station_name),
            flags.dwFlags & WSF_VISIBLE as u32 != 0,
            Some(&desktop_name),
            can_create,
        ))
    })()
    .unwrap_or(false)
}

/// A UI must return only after its window has closed. Cancellation is polled
/// by the UI thread; the signaling loop never waits for user input.
pub trait ApprovalUi {
    fn start(
        &mut self,
        request: ApprovalRequest,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Receiver<bool>, ()>;
}

struct Active {
    request: ApprovalRequest,
    cancelled: Arc<AtomicBool>,
    result: Receiver<bool>,
    suppress_decision: bool,
}

pub struct BrowserApproval<U: ApprovalUi = NativeApprovalUi> {
    ui: U,
    active: Option<Active>,
}

impl Default for BrowserApproval {
    fn default() -> Self {
        Self::new(NativeApprovalUi)
    }
}

impl<U: ApprovalUi> BrowserApproval<U> {
    pub fn new(ui: U) -> Self {
        Self { ui, active: None }
    }

    /// `Some` means this is an approval control, including malformed controls.
    /// No malformed or unknown fields ever reach the native dialog.
    pub fn handle_wire(&mut self, text: &str, now: u64) -> Option<Vec<ApprovalDecision>> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        if !matches!(
            value.get("type").and_then(|v| v.as_str()),
            Some("approval-request" | "approval-cancel")
        ) {
            return None;
        }
        let mut decisions = Vec::new();
        if text.len() > MAX_WIRE_BYTES {
            return Some(decisions);
        }
        let Ok(incoming) = serde_json::from_value::<Incoming>(value) else {
            return Some(decisions);
        };
        match incoming {
            Incoming::ApprovalRequest {
                id,
                client,
                agent,
                expires_at,
            } => {
                let request = ApprovalRequest {
                    id,
                    client,
                    agent,
                    expires_at,
                };
                if !request.valid() {
                    return Some(decisions);
                }
                if let Some(active) = self.active.as_mut() {
                    // A repeated ID must not subsequently produce an Allow.
                    if active.request.id == request.id {
                        active.suppress_decision = true;
                        active.cancelled.store(true, Ordering::Release);
                    }
                    decisions.push(decision(request.id, false));
                } else if request.expires_at <= now {
                    decisions.push(decision(request.id, false));
                } else {
                    let cancelled = Arc::new(AtomicBool::new(false));
                    match self.ui.start(request.clone(), cancelled.clone()) {
                        Ok(result) => {
                            self.active = Some(Active {
                                request,
                                cancelled,
                                result,
                                suppress_decision: false,
                            })
                        }
                        Err(()) => decisions.push(decision(request.id, false)),
                    }
                }
            }
            Incoming::ApprovalCancel { id } if valid_id(&id) => {
                if let Some(active) = self
                    .active
                    .as_mut()
                    .filter(|active| active.request.id == id)
                {
                    active.suppress_decision = true;
                    active.cancelled.store(true, Ordering::Release);
                }
            }
            _ => {}
        }
        Some(decisions)
    }

    pub fn poll(&mut self, now: u64) -> Vec<ApprovalDecision> {
        let Some(active) = self.active.as_mut() else {
            return Vec::new();
        };
        let mut decisions = Vec::new();
        // Expiry wins even if an Allow was queued but not yet delivered.
        if !active.suppress_decision && now >= active.request.expires_at {
            active.suppress_decision = true;
            active.cancelled.store(true, Ordering::Release);
            decisions.push(decision(active.request.id.clone(), false));
        }
        match active.result.try_recv() {
            Ok(approve) => {
                if !active.suppress_decision {
                    decisions.push(decision(active.request.id.clone(), approve));
                }
                self.active = None;
            }
            Err(TryRecvError::Disconnected) => {
                if !active.suppress_decision {
                    decisions.push(decision(active.request.id.clone(), false));
                }
                self.active = None;
            }
            Err(TryRecvError::Empty) => {}
        }
        decisions
    }

    pub fn disconnect(&mut self) {
        if let Some(active) = self.active.as_mut() {
            active.suppress_decision = true;
            active.cancelled.store(true, Ordering::Release);
        }
    }
}

impl<U: ApprovalUi> Drop for BrowserApproval<U> {
    fn drop(&mut self) {
        self.disconnect();
    }
}

pub struct NativeApprovalUi;
struct UiJob {
    request: ApprovalRequest,
    cancelled: Arc<AtomicBool>,
    result: Sender<bool>,
}
struct UiWorker {
    jobs: Sender<UiJob>,
    busy: Arc<AtomicBool>,
}

impl ApprovalUi for NativeApprovalUi {
    fn start(
        &mut self,
        request: ApprovalRequest,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Receiver<bool>, ()> {
        static WORKER: OnceLock<Option<UiWorker>> = OnceLock::new();
        let worker = WORKER
            .get_or_init(|| {
                let (jobs, rx) = mpsc::channel::<UiJob>();
                let busy = Arc::new(AtomicBool::new(false));
                let worker_busy = busy.clone();
                std::thread::Builder::new()
                    .name("browser-approval".into())
                    .spawn(move || {
                        while let Ok(job) = rx.recv() {
                            let approve = native_dialog(&job.request, &job.cancelled);
                            // Clear before publishing completion: a new request can only
                            // start after all HWNDs from the old dialog are destroyed.
                            worker_busy.store(false, Ordering::Release);
                            let _ = job.result.send(approve);
                        }
                    })
                    .ok()
                    .map(|_| UiWorker { jobs, busy })
            })
            .as_ref()
            .ok_or(())?;
        if worker
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(());
        }
        let (result, rx) = mpsc::channel();
        if worker
            .jobs
            .send(UiJob {
                request,
                cancelled,
                result,
            })
            .is_err()
        {
            worker.busy.store(false, Ordering::Release);
            return Err(());
        }
        Ok(rx)
    }
}

#[cfg(not(windows))]
fn native_dialog(_: &ApprovalRequest, _: &AtomicBool) -> bool {
    false
}

#[cfg(windows)]
fn native_dialog(request: &ApprovalRequest, cancelled: &AtomicBool) -> bool {
    native::show(request, cancelled).unwrap_or_else(|error| {
        tracing::warn!(%error, "browser approval dialog failed; denying request");
        false
    })
}

#[cfg(windows)]
mod native {
    use super::*;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::COLOR_WINDOW;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::SystemServices::SS_NOPREFIX;
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows::Win32::UI::WindowsAndMessaging::*;

    const ALLOW: usize = 100;
    const DENY: usize = 101;
    const CONTENT: i32 = 102;
    struct State<'a> {
        request: &'a ApprovalRequest,
        cancelled: &'a AtomicBool,
        approve: std::cell::Cell<bool>,
    }
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }
    fn content(request: &ApprovalRequest) -> Vec<u16> {
        wide(&format!("Allow a new browser to sign in?\n\nClient prefix (untrusted): {}\nBrowser / OS (untrusted): {}\n\nExpires in {} seconds", request.client, request.agent,
            request.expires_at.saturating_sub(now_ms()).div_ceil(1000)))
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wp: WPARAM,
        lp: LPARAM,
    ) -> LRESULT {
        if message == WM_NCCREATE {
            let create = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State<'_>;
        if !ptr.is_null() {
            let state = &*ptr;
            match message {
                WM_COMMAND
                    if [ALLOW, DENY, IDOK.0 as usize, IDCANCEL.0 as usize]
                        .contains(&(wp.0 & 0xffff)) =>
                {
                    state.approve.set(
                        wp.0 & 0xffff == ALLOW
                            && !state.cancelled.load(Ordering::Acquire)
                            && now_ms() < state.request.expires_at,
                    );
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                WM_CLOSE => {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                WM_TIMER => {
                    if state.cancelled.load(Ordering::Acquire)
                        || now_ms() >= state.request.expires_at
                    {
                        let _ = DestroyWindow(hwnd);
                    } else if let Ok(label) = GetDlgItem(Some(hwnd), CONTENT) {
                        let text = content(state.request);
                        let _ = SetWindowTextW(label, PCWSTR(text.as_ptr()));
                    }
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

    pub fn show(request: &ApprovalRequest, cancelled: &AtomicBool) -> windows::core::Result<bool> {
        if cancelled.load(Ordering::Acquire) || now_ms() >= request.expires_at {
            return Ok(false);
        }
        // All handles and callback state live exclusively on this dedicated
        // thread. A hidden window in this process owns the topmost dialog.
        unsafe {
            let instance = GetModuleHandleW(PCWSTR::null())?;
            let class = w!("BubblePlayBrowserApproval");
            static CLASS: OnceLock<bool> = OnceLock::new();
            if !*CLASS.get_or_init(|| {
                RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(window_proc),
                    hInstance: instance.into(),
                    lpszClassName: class,
                    hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                    hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
                        (COLOR_WINDOW.0 + 1) as *mut _,
                    ),
                    ..Default::default()
                }) != 0
            }) {
                return Err(windows::core::Error::from_thread());
            }
            let owner = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("BubblePlay"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            let mut state = State {
                request,
                cancelled,
                approve: std::cell::Cell::new(false),
            };
            let dialog = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_DLGMODALFRAME,
                class,
                w!("BubblePlay"),
                WS_CAPTION | WS_SYSMENU,
                (GetSystemMetrics(SM_CXSCREEN) - 540) / 2,
                (GetSystemMetrics(SM_CYSCREEN) - 285) / 2,
                540,
                285,
                Some(owner),
                None,
                Some(instance.into()),
                Some(&mut state as *mut _ as *const _),
            );
            let result = (|| {
                let hwnd = dialog.clone()?;
                let text = content(request);
                // SS_NOPREFIX keeps attacker-controlled ampersands literal.
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    PCWSTR(text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SS_NOPREFIX.0),
                    20,
                    15,
                    495,
                    170,
                    Some(hwnd),
                    Some(HMENU(CONTENT as *mut _)),
                    Some(instance.into()),
                    None,
                )?;
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("BUTTON"),
                    w!("Allow"),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                    305,
                    195,
                    95,
                    30,
                    Some(hwnd),
                    Some(HMENU(ALLOW as *mut _)),
                    Some(instance.into()),
                    None,
                )?;
                let deny = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("BUTTON"),
                    w!("Deny"),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
                    410,
                    195,
                    95,
                    30,
                    Some(hwnd),
                    Some(HMENU(DENY as *mut _)),
                    Some(instance.into()),
                    None,
                )?;
                if SetTimer(Some(hwnd), 1, 100, None) == 0 {
                    return Err(windows::core::Error::from_thread());
                }
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetFocus(Some(deny));
                let mut message = MSG::default();
                loop {
                    let status = GetMessageW(&mut message, None, 0, 0).0;
                    if status == -1 {
                        return Err(windows::core::Error::from_thread());
                    }
                    if status == 0 {
                        break;
                    }
                    if !IsDialogMessageW(hwnd, &message).as_bool() {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                Ok(state.approve.get()
                    && !cancelled.load(Ordering::Acquire)
                    && now_ms() < request.expires_at)
            })();
            // Also clean up after partial creation or a message-loop error.
            if let Ok(hwnd) = dialog {
                let _ = KillTimer(Some(hwnd), 1);
                if IsWindow(Some(hwnd)).as_bool() {
                    let _ = DestroyWindow(hwnd);
                }
            }
            let _ = DestroyWindow(owner);
            // Errors during creation can post WM_QUIT during cleanup; remove it
            // before the persistent worker accepts another request.
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, WM_QUIT, WM_QUIT, PM_REMOVE).as_bool() {}
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct FakeUi {
        senders: Vec<Sender<bool>>,
        cancellations: Vec<Arc<AtomicBool>>,
        fail: bool,
    }
    impl ApprovalUi for FakeUi {
        fn start(
            &mut self,
            _: ApprovalRequest,
            cancelled: Arc<AtomicBool>,
        ) -> Result<Receiver<bool>, ()> {
            if self.fail {
                return Err(());
            }
            let (tx, rx) = mpsc::channel();
            self.senders.push(tx);
            self.cancellations.push(cancelled);
            Ok(rx)
        }
    }
    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../fixtures/browser-approval-contract.json"
        ))
        .unwrap()
    }
    fn wire(value: &serde_json::Value) -> String {
        serde_json::to_string(value).unwrap()
    }
    fn request() -> serde_json::Value {
        fixture()["request"].clone()
    }
    fn controller() -> BrowserApproval<FakeUi> {
        BrowserApproval::new(FakeUi::default())
    }

    #[test]
    fn capability_policy_requires_every_interactive_host_condition() {
        assert!(capability_eligible(
            false,
            Some(1),
            Some("WinSta0"),
            true,
            Some("Default"),
            true
        ));
        assert!(capability_eligible(
            false,
            Some(7),
            Some("winsta0"),
            true,
            Some("default"),
            true
        ));
        for (service, session, station, visible, desktop, create) in [
            (true, Some(1), Some("WinSta0"), true, Some("Default"), true),
            (false, None, Some("WinSta0"), true, Some("Default"), true),
            (false, Some(0), Some("WinSta0"), true, Some("Default"), true),
            (false, Some(1), None, true, Some("Default"), true),
            (
                false,
                Some(1),
                Some("Service-0x0-3e7$"),
                true,
                Some("Default"),
                true,
            ),
            (
                false,
                Some(1),
                Some("WinSta0"),
                false,
                Some("Default"),
                true,
            ),
            (false, Some(1), Some("WinSta0"), true, None, true),
            (
                false,
                Some(1),
                Some("WinSta0"),
                true,
                Some("Winlogon"),
                true,
            ),
            (false, Some(1), Some("WinSta0"), true, Some("Other"), true),
            (
                false,
                Some(1),
                Some("WinSta0"),
                true,
                Some("Default"),
                false,
            ),
        ] {
            assert!(!capability_eligible(
                service, session, station, visible, desktop, create
            ));
        }
    }

    #[test]
    fn contract_request_cancel_decision_and_limits() {
        let f = fixture();
        assert_eq!(f["limits"]["idMin"], ID_MIN);
        assert_eq!(f["limits"]["idMax"], ID_MAX);
        assert_eq!(f["limits"]["agentMax"], AGENT_MAX);
        let mut approval = controller();
        assert!(approval
            .handle_wire(&wire(&f["request"]), 1)
            .unwrap()
            .is_empty());
        approval.ui.senders[0].send(true).unwrap();
        assert_eq!(
            serde_json::to_value(&approval.poll(2)[0]).unwrap(),
            f["decision"]
        );
        approval.handle_wire(&wire(&f["request"]), 1);
        approval.handle_wire(&wire(&f["cancel"]), 2);
        assert!(approval.ui.cancellations[1].load(Ordering::Acquire));
        approval.ui.senders[1].send(true).unwrap();
        assert!(approval.poll(3).is_empty());
        for invalid in f["invalid"].as_array().unwrap() {
            approval.handle_wire(&wire(invalid), 1);
        }
        assert_eq!(approval.ui.senders.len(), 2);
    }

    #[test]
    fn validation_rejects_untrusted_fields_and_accepts_ipv6() {
        for (field, value) in [
            ("id", serde_json::json!("short")),
            ("id", serde_json::json!("!".repeat(22))),
            ("agent", serde_json::json!("x".repeat(81))),
            ("agent", serde_json::json!("x\nAllow")),
            ("client", serde_json::json!("x".repeat(81))),
            ("client", serde_json::json!("evil\ntext")),
            ("expiresAt", serde_json::json!(-1)),
            ("expiresAt", serde_json::json!(u64::MAX)),
            ("expiresAt", serde_json::json!("tomorrow")),
            ("extra", serde_json::json!(true)),
        ] {
            let mut req = request();
            req[field] = value;
            let mut approval = controller();
            approval.handle_wire(&wire(&req), 1);
            assert!(approval.ui.senders.is_empty(), "accepted {field}: {req}");
        }
        for client in [
            "2001:db8:abcd:1200::/56",
            "2001:db8:abcd:1234::/64",
            "unknown",
            "2001:db8:abcd",
        ] {
            let mut req = request();
            req["client"] = serde_json::json!(client);
            let mut approval = controller();
            approval.handle_wire(&wire(&req), 1);
            assert_eq!(approval.ui.senders.len(), 1);
        }
    }

    #[test]
    fn busy_requests_deny_and_duplicate_id_cancels_original() {
        let mut approval = controller();
        let req = request();
        approval.handle_wire(&wire(&req), 1);
        let mut other = req.clone();
        other["id"] = serde_json::json!("x".repeat(22));
        assert!(!approval.handle_wire(&wire(&other), 2).unwrap()[0].approve);
        assert!(!approval.ui.cancellations[0].load(Ordering::Acquire));
        assert!(!approval.handle_wire(&wire(&req), 2).unwrap()[0].approve);
        assert!(approval.ui.cancellations[0].load(Ordering::Acquire));
        approval.ui.senders[0].send(true).unwrap();
        assert!(approval.poll(3).is_empty());
        assert_eq!(approval.ui.senders.len(), 1);
    }

    #[test]
    fn expiry_denies_once_and_stays_busy_until_window_closes() {
        let mut approval = controller();
        let mut req = request();
        req["expiresAt"] = serde_json::json!(10);
        approval.handle_wire(&wire(&req), 1);
        approval.ui.senders[0].send(true).unwrap();
        assert!(!approval.poll(10)[0].approve);
        assert!(approval.poll(11).is_empty());
        approval.handle_wire(&wire(&req), 10);
        assert_eq!(approval.ui.senders.len(), 1);
        // No completion: cancellation still holds the slot until UI returns.
        let mut approval = controller();
        approval.handle_wire(&wire(&req), 1);
        assert_eq!(approval.poll(10).len(), 1);
        approval.handle_wire(&wire(&request()), 11);
        assert_eq!(approval.ui.senders.len(), 1);
    }

    #[test]
    fn cancel_disconnect_and_drop_suppress_queued_allow() {
        for disconnect in [false, true] {
            let mut approval = controller();
            let req = request();
            approval.handle_wire(&wire(&req), 1);
            approval.ui.senders[0].send(true).unwrap();
            if disconnect {
                approval.disconnect();
            } else {
                approval.handle_wire(&wire(&fixture()["cancel"]), 2);
            }
            assert!(approval.poll(3).is_empty());
        }
        let mut approval = controller();
        approval.handle_wire(&wire(&request()), 1);
        let token = approval.ui.cancellations[0].clone();
        drop(approval);
        assert!(token.load(Ordering::Acquire));
    }

    #[test]
    fn ui_failure_denies_and_unknown_cancel_preserves_request() {
        let mut approval = BrowserApproval::new(FakeUi {
            fail: true,
            ..Default::default()
        });
        assert!(!approval.handle_wire(&wire(&request()), 1).unwrap()[0].approve);
        let mut approval = controller();
        approval.handle_wire(&wire(&request()), 1);
        approval.handle_wire(
            &wire(&serde_json::json!({"type":"approval-cancel","id":"x".repeat(22)})),
            2,
        );
        assert!(!approval.ui.cancellations[0].load(Ordering::Acquire));
        approval.ui.senders.clear();
        assert!(!approval.poll(3)[0].approve);
    }
}
