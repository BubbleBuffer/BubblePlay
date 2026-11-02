//! Owner-run live secure-desktop probe. Unit tests never start a helper or UAC.
//! Holds the authenticated exclusive Host slot through policy restoration.
use anyhow::{bail, ensure, Context, Result};
use bubbleplay_host::{
    admin_agent::{
        protocol::{
            Record, KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
            MOUSEEVENTF_VIRTUALDESK,
        },
        trust,
    },
    admin_mode,
    desktop_session::{DesktopKind, InputDesktop},
    input::DesktopRegion,
    secure_helper::{
        client::{BgraFrame, InputReceipt, LinkError, SecureHelperLink, SecureState},
        protocol,
    },
};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND},
        System::{
            Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED},
            Registry::{
                RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
                HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WOW64_64KEY, REG_DWORD,
                REG_VALUE_TYPE,
            },
            RemoteDesktop::{
                WTSEnumerateProcessesW, WTSFreeMemory, WTSGetActiveConsoleSessionId,
                WTS_PROCESS_INFOW,
            },
        },
        UI::{
            HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
            Shell::{
                FOLDERID_System, SHGetKnownFolderPath, ShellExecuteExW, KF_FLAG_DEFAULT,
                SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
            },
            WindowsAndMessaging::{
                GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
                SM_YVIRTUALSCREEN, SW_HIDE,
            },
        },
    },
};
const TASK: &str = r"\BubblePlay\SecureDesktop";
const ESCAPE: u16 = 0x1b;
const RUNAS_CANCELLED: i32 = 0x800704c7u32 as i32;
const MOVE_FLAGS: u32 = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
const PAUSE: Duration = Duration::from_millis(50);
const UAC_TIMEOUT: Duration = Duration::from_secs(15);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

struct Args {
    out: PathBuf,
    settings: PathBuf,
    host_log: PathBuf,
    run_id: String,
}
fn args() -> Result<Args> {
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA missing")?)
        .join("BubblePlay");
    let mut result = Args {
        out: PathBuf::new(),
        settings: local.join("tray.json"),
        host_log: latest_host_log(&local.join("logs")),
        run_id: format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ),
    };
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().context("every option requires a path")?;
        match arg.to_str() {
            Some("--out-dir") => result.out = PathBuf::from(value),
            Some("--tray-settings") => result.settings = PathBuf::from(value),
            Some("--host-log") => result.host_log = PathBuf::from(value),
            Some("--run-id") => result.run_id = value.into_string().map_err(|_| anyhow::anyhow!("run-id must be UTF-8"))?,
            _ => bail!("usage: probe_secure_helper --out-dir <new directory> [--tray-settings <tray.json>] [--host-log <host-YYYY-MM-DD.jsonl>]"),
        }
    }
    ensure!(!result.out.as_os_str().is_empty(), "--out-dir is required");
    Ok(result)
}
fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.as_ref().encode_wide().chain(Some(0)).collect()
}
fn system_directory() -> Result<PathBuf> {
    let text = unsafe { SHGetKnownFolderPath(&FOLDERID_System, KF_FLAG_DEFAULT, None) }
        .context("resolve Windows System32")?;
    let path = unsafe { text.to_string() };
    unsafe {
        CoTaskMemFree(Some(text.0.cast()));
    }
    Ok(PathBuf::from(path?))
}
/// The newest `host-YYYY-MM-DD[.N].jsonl` day file in the host's log directory.
fn latest_host_log(logs: &Path) -> PathBuf {
    let newest = fs::read_dir(logs)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("host-") && name.ends_with(".jsonl"))
        })
        .max();
    newest.unwrap_or_else(|| logs.join("host.jsonl"))
}
fn no_viewer_in_log(text: &str) -> bool {
    let start = text.rfind("viewer session started");
    let end = text.rfind("viewer session ended");
    start.is_none() || end.is_some_and(|end| end > start.unwrap())
}
fn check_no_viewer(path: &Path) -> Result<()> {
    let bytes = fs::read(path)
        .with_context(|| format!("cannot establish viewer state from {}", path.display()))?;
    ensure!(
        no_viewer_in_log(&String::from_utf8_lossy(&bytes)),
        "host log shows a live viewer; disconnect all viewers before this probe"
    );
    Ok(())
}
fn virtual_region() -> Result<DesktopRegion> {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        .context("set per-monitor V2 DPI awareness before reading physical display bounds")?;
    let r = DesktopRegion::new(
        unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
        unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
        unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.try_into()?,
        unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.try_into()?,
    );
    protocol::Region {
        x: r.x,
        y: r.y,
        width: r.width,
        height: r.height,
    }
    .validate()
    .context("virtual display exceeds helper frame limits")?;
    Ok(r)
}
fn consent_processes() -> Result<Vec<u32>> {
    let mut processes: *mut WTS_PROCESS_INFOW = std::ptr::null_mut();
    let mut count = 0;
    unsafe { WTSEnumerateProcessesW(None, 0, 1, &mut processes, &mut count) }
        .context("enumerate consent processes")?;
    let result = (|| {
        ensure!(
            count == 0 || !processes.is_null(),
            "invalid WTS process list"
        );
        let list = if count == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(processes, count as usize) }
        };
        let mut pids = Vec::new();
        for process in list {
            if process.SessionId == trust::current_session_id() && !process.pProcessName.is_null() {
                let name =
                    unsafe { process.pProcessName.to_string() }.context("read WTS process name")?;
                if name.eq_ignore_ascii_case("consent.exe") {
                    pids.push(process.ProcessId);
                }
            }
        }
        pids.sort_unstable();
        Ok(pids)
    })();
    unsafe {
        WTSFreeMemory(processes.cast());
    }
    result
}
fn escape() -> Vec<Record> {
    vec![
        Record::Key {
            vk: ESCAPE,
            scan: 0,
            flags: 0,
        },
        Record::Key {
            vk: ESCAPE,
            scan: 0,
            flags: KEYEVENTF_KEYUP,
        },
    ]
}
fn movement(x: i32, y: i32) -> Vec<Record> {
    vec![Record::Mouse {
        dx: x,
        dy: y,
        data: 0,
        flags: MOVE_FLAGS,
    }]
}
/// Defence in depth: the probe has no approval input primitive.
fn safe_records(records: &[Record]) -> bool {
    records == escape().as_slice() || (!records.is_empty() && records.len() <= 256 && records.iter().all(|record| matches!(record,
        Record::Mouse { dx, dy, data:0, flags } if *flags == MOVE_FLAGS && (0..=65535).contains(dx) && (0..=65535).contains(dy))))
}
fn receipt_passes(receipt: &InputReceipt, id: u64, count: usize, admin: bool) -> bool {
    receipt.id == id
        && receipt.received as usize == count
        && receipt.error_code == 0
        && if admin {
            receipt.accepted as usize == count
                && receipt.applied as usize == count
                && receipt.reason == "ok"
        } else {
            receipt.accepted == 0 && receipt.applied == 0 && receipt.reason == "policy-off"
        }
}
fn fresh_frame(frame: &BgraFrame, epoch: u64, after_seq: u64, input_id: u64) -> bool {
    frame.desktop_epoch == epoch && frame.seq > after_seq && frame.input_id >= input_id
}
fn default_closed(state: &SecureState, local_default: bool, no_consent: bool) -> bool {
    !state.secure && local_default && no_consent
}
fn closure_in_deadline(closed: bool, observed: Instant, deadline: Instant) -> bool {
    closed && observed <= deadline
}
fn cleanup_complete(trigger_finished: bool, default: bool, no_consent: bool) -> bool {
    trigger_finished && default && no_consent
}
fn policy_acknowledged(value: &Value, run_id: &str, id: u32) -> bool {
    value["run_id"] == run_id && value["id"] == id && value["success"] == true
}
fn check_wrapper(args: &Args) -> Result<()> {
    let value: Value =
        serde_json::from_slice(&fs::read(args.out.join("policy-wrapper.json")).context(
            "protected policy requires host/scripts/test-secure-desktop.ps1; use that wrapper",
        )?)?;
    let pid = value["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok());
    ensure!(
        value["run_id"] == args.run_id
            && pid.is_some_and(|pid| pid != std::process::id()
                && trust::process_image_path(pid).is_some()
                && trust::process_session_id(pid) == Some(trust::current_session_id())),
        "missing matching live wrapper; use host/scripts/test-secure-desktop.ps1"
    );
    Ok(())
}
fn receipt_json(receipt: &InputReceipt) -> Value {
    json!({"id":receipt.id,"received":receipt.received,"accepted":receipt.accepted,"applied":receipt.applied,"error_code":receipt.error_code,"reason":receipt.reason})
}
fn frame_json(frame: &BgraFrame, name: &str) -> Value {
    json!({"path":name,"seq":frame.seq,"epoch":frame.desktop_epoch,"input_id":frame.input_id,"width":frame.width,"height":frame.height})
}
fn rgba(frame: &BgraFrame) -> Result<Vec<u8>> {
    let length = protocol::frame_len(frame.width, frame.height, frame.stride)?;
    ensure!(frame.pixels.len() == length, "BGRA frame length mismatch");
    let mut pixels = frame.pixels.clone();
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    Ok(pixels)
}
fn save_png(path: &Path, frame: &BgraFrame) -> Result<()> {
    let pixels = rgba(frame)?;
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&pixels)?;
    Ok(())
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("probe-{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
struct ConfigBackup {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
    value: Value,
    changed: bool,
}
impl ConfigBackup {
    fn read(path: &Path) -> Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let value = if let Some(bytes) = &bytes {
            ensure!(
                bytes.len() <= 65536,
                "tray.json exceeds helper config limit"
            );
            serde_json::from_slice(bytes)
                .context("tray.json is not valid JSON; refusing to replace it")?
        } else {
            json!({})
        };
        ensure!(value.is_object(), "tray.json must be a JSON object");
        Ok(Self {
            path: path.to_path_buf(),
            bytes,
            value,
            changed: false,
        })
    }
    fn set_mode(&mut self, enabled: bool) -> Result<()> {
        let mut value = self.value.clone();
        value["adminMode"] = json!(enabled);
        // Mark before writing so every partial failure reaches restoration.
        self.changed = true;
        atomic_write(&self.path, &serde_json::to_vec_pretty(&value)?)
    }
    fn restore(&mut self) -> Result<()> {
        if !self.changed {
            return Ok(());
        }
        if let Some(bytes) = &self.bytes {
            atomic_write(&self.path, bytes)?;
            ensure!(
                fs::read(&self.path)? == *bytes,
                "tray.json restore verification failed"
            );
        } else {
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            };
            ensure!(
                !self.path.exists(),
                "new tray.json still exists after restore"
            );
        }
        self.changed = false;
        Ok(())
    }
}
impl Drop for ConfigBackup {
    fn drop(&mut self) {
        if self.changed {
            let _ = self.restore();
        }
    }
}

#[derive(Debug)]
struct TriggerResult {
    shell_ok: bool,
    error_code: i32,
}
struct UacAttempt {
    receive: mpsc::Receiver<TriggerResult>,
    thread: Option<thread::JoinHandle<()>>,
    result: Option<TriggerResult>,
    consent_pid: Option<u32>,
}
impl UacAttempt {
    fn start() -> Result<Self> {
        let (send, receive) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("probe-owned-uac".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
                        .ok()
                        .context("initialize runas COM thread")?;
                    let verb = wide("runas");
                    let exe = wide(system_directory()?.join("cmd.exe"));
                    let parameters = wide("/c exit 0");
                    let mut info = SHELLEXECUTEINFOW {
                        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
                        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
                        lpVerb: PCWSTR(verb.as_ptr()),
                        lpFile: PCWSTR(exe.as_ptr()),
                        lpParameters: PCWSTR(parameters.as_ptr()),
                        nShow: SW_HIDE.0,
                        ..Default::default()
                    };
                    let result = unsafe { ShellExecuteExW(&mut info) };
                    if !info.hProcess.is_invalid() {
                        let _ = unsafe { CloseHandle(info.hProcess) };
                    }
                    unsafe {
                        CoUninitialize();
                    }
                    result.context("owned runas request")
                })();
                let error_code = result
                    .as_ref()
                    .err()
                    .and_then(|error| error.downcast_ref::<windows::core::Error>())
                    .map_or(0, |error| error.code().0);
                let _ = send.send(TriggerResult {
                    shell_ok: result.is_ok(),
                    error_code,
                });
            })?;
        Ok(Self {
            receive,
            thread: Some(thread),
            result: None,
            consent_pid: None,
        })
    }
    fn poll(&mut self) {
        if self.result.is_none() {
            if let Ok(result) = self.receive.try_recv() {
                self.result = Some(result);
            }
        }
    }
    fn finished(&mut self) -> bool {
        self.poll();
        self.result.is_some()
            && self
                .thread
                .as_ref()
                .is_none_or(thread::JoinHandle::is_finished)
    }
    fn join_if_finished(&mut self) {
        if self.finished() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
    fn identify_owned(&mut self, state: &SecureState) -> Result<bool> {
        if !state.secure || state.kind != "uac" {
            return Ok(false);
        }
        let pids = consent_processes()?;
        ensure!(
            pids.len() == 1,
            "cannot identify a single owned consent process; refusing to target another prompt"
        );
        if let Some(pid) = self.consent_pid {
            ensure!(
                pids[0] == pid,
                "consent process changed; refusing to target another prompt"
            );
        } else {
            self.consent_pid = Some(pids[0]);
        }
        Ok(true)
    }
}
struct InputProof {
    id: u64,
    frame_watermark: u64,
}
struct Harness {
    link: SecureHelperLink,
    region: DesktopRegion,
    config: ConfigBackup,
    out: PathBuf,
    host_log: PathBuf,
    frame: Option<BgraFrame>,
    attempt: Option<UacAttempt>,
    run_id: String,
    mutation_marked: bool,
    policy_request: u32,
}
impl Harness {
    /// The owner-run wrapper performs UAC preparation/restoration while we
    /// retain the exclusive Host and keep the helper alive. Never inject into
    /// these owner-approved policy prompts.
    fn policy_step(&mut self, action: &str) -> Result<()> {
        self.policy_request += 1;
        let id = self.policy_request;
        let request = self.out.join(format!("policy-request-{id}.json"));
        let response = self.out.join(format!("policy-response-{id}.json"));
        atomic_write(
            &request,
            &serde_json::to_vec(&json!({"run_id":self.run_id,"id":id,"action":action}))?,
        )?;
        // Approval is local and may take time. The wrapper is responsible for
        // waiting for the elevated process before acknowledging this request.
        // Do not time out and release Host while an owner approval is still
        // pending. Only the wrapper's post-process acknowledgement ends this
        // wait, so an unacknowledged prompt cannot outlive the probe.
        loop {
            self.link.tick(true, self.region, Instant::now());
            if response.exists() {
                let value: Value = serde_json::from_slice(&fs::read(&response)?)?;
                ensure!(
                    policy_acknowledged(&value, &self.run_id, id),
                    "owner policy step {action} failed or acknowledgement identity mismatched: {}",
                    value["error"]
                );
                ensure!(
                    InputDesktop::open()?.kind() == DesktopKind::Default
                        && consent_processes()?.is_empty(),
                    "policy step left a consent window or secure desktop open"
                );
                return Ok(());
            }
            thread::sleep(PAUSE);
        }
    }
    fn mark_mutation(&mut self) -> Result<()> {
        if !self.mutation_marked {
            let marker = json!({"run_id":self.run_id,"settings_path":self.config.path,"exclusive_host_claimed":true,"pid":std::process::id()});
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.out.join("mutation-authorized.json"))?;
            file.write_all(&serde_json::to_vec(&marker)?)?;
            file.sync_all()?;
            self.mutation_marked = true;
        }
        Ok(())
    }
    fn set_policy(&mut self, enabled: bool) -> Result<()> {
        if enabled {
            self.policy_step("on")?;
        } else {
            // Wrapper runs the protected task as the normal user, then waits
            // for task completion so a queued OFF cannot race the next ON.
            self.policy_step("off")?;
        }
        let deadline = Instant::now() + CLOSE_TIMEOUT;
        while admin_mode::effective_for_console() != enabled {
            self.link.tick(true, self.region, Instant::now());
            ensure!(
                Instant::now() < deadline,
                "HKLM policy did not become {enabled}"
            );
            thread::sleep(PAUSE);
        }
        Ok(())
    }
    fn state(&mut self) -> Result<SecureState> {
        self.link.tick(true, self.region, Instant::now());
        if let Some(frame) = self.link.take_frame() {
            self.frame = Some(frame);
        }
        if let Some(error) = self.link.last_error() {
            bail!("helper connection failed: {error}");
        }
        self.link
            .state()
            .context("helper has not supplied desktop state")
    }
    fn baseline(&mut self) -> Result<()> {
        let deadline = Instant::now() + CLOSE_TIMEOUT;
        let mut default_since = None;
        loop {
            if let Ok(state) = self.state() {
                ensure!(
                    !state.secure,
                    "pre-existing secure desktop; no prompt was created or cancelled"
                );
                ensure!(
                    InputDesktop::open()?.kind() == DesktopKind::Default,
                    "input desktop is not Default"
                );
                ensure!(
                    consent_processes()?.is_empty(),
                    "pre-existing consent process; refusing to start or cancel UAC"
                );
                let since = default_since.get_or_insert(Instant::now());
                if since.elapsed() >= Duration::from_secs(1) {
                    check_no_viewer(&self.host_log)?;
                    return Ok(());
                }
            }
            if let Some(error) = self.link.last_error() {
                bail!("baseline helper error: {error}");
            }
            ensure!(Instant::now() < deadline, "Default baseline timeout");
            thread::sleep(PAUSE);
        }
    }
    fn require_owned(&mut self) -> Result<SecureState> {
        let state = self.state()?;
        let attempt = self.attempt.as_mut().context("no owned runas request")?;
        ensure!(
            !attempt.finished(),
            "runas request already finished; refusing to act on another prompt"
        );
        ensure!(
            attempt.identify_owned(&state)?,
            "owned UAC prompt is no longer present"
        );
        ensure!(
            !attempt.finished(),
            "runas finished while verifying consent; refusing further input"
        );
        ensure!(state.available, "owned UAC capture unavailable");
        Ok(state)
    }
    fn wait_prompt(&mut self) -> Result<SecureState> {
        let deadline = Instant::now() + UAC_TIMEOUT;
        loop {
            let attempt = self.attempt.as_mut().context("no runas request")?;
            ensure!(
                !attempt.finished(),
                "runas finished without an observed UAC prompt: {:?}",
                attempt.result
            );
            if let Ok(state) = self.state() {
                if self.attempt.as_mut().unwrap().identify_owned(&state)? {
                    ensure!(state.available, "secure frame source unavailable");
                    return Ok(state);
                }
            }
            let attempt = self.attempt.as_mut().unwrap();
            attempt.poll();
            if attempt.finished() {
                bail!(
                    "runas finished without an observed UAC prompt: {:?}",
                    attempt.result
                );
            }
            ensure!(
                Instant::now() < deadline,
                "UAC detection timed out; bounded late-prompt cleanup will run"
            );
            thread::sleep(PAUSE);
        }
    }
    fn wait_frame(&mut self, epoch: u64, after_seq: u64, input_id: u64) -> Result<BgraFrame> {
        let deadline = Instant::now() + CLOSE_TIMEOUT;
        loop {
            let state = self.require_owned()?;
            ensure!(state.desktop_epoch == epoch, "secure desktop epoch changed");
            if let Some(frame) = &self.frame {
                if fresh_frame(frame, epoch, after_seq, input_id) {
                    return Ok(frame.clone());
                }
            }
            ensure!(
                Instant::now() < deadline,
                "no fresh secure frame after sequence {after_seq}"
            );
            thread::sleep(PAUSE);
        }
    }
    fn send(
        &mut self,
        records: &[Record],
        admin: bool,
        report: &mut Value,
        action_started: Instant,
    ) -> Result<InputProof> {
        ensure!(safe_records(records), "unsafe probe input rejected locally");
        self.require_owned()?;
        let id = self
            .link
            .send_probe_input(records)
            .context("queue safe probe input")?;
        let deadline = action_started + CLOSE_TIMEOUT;
        loop {
            if let Some(receipt) = self.link.take_input_receipt() {
                // A stale/unrelated receipt is never treated as this request's proof.
                ensure!(
                    receipt.id == id,
                    "receipt ID mismatch: wanted {id}, got {}",
                    receipt.id
                );
                report["receipts"]
                    .as_array_mut()
                    .unwrap()
                    .push(receipt_json(&receipt));
                ensure!(
                    receipt_passes(&receipt, id, records.len(), admin),
                    "helper receipt did not prove expected policy/SendInput result"
                );
                // Consume any snapshot already queued when the receipt arrived.
                // Screenshots must then come from a sequence beyond this watermark.
                self.state()?;
                return Ok(InputProof {
                    id,
                    frame_watermark: self.frame.as_ref().map_or(0, |frame| frame.seq),
                });
            }
            if let Some(error) = self.link.last_error() {
                bail!("input receipt connection error: {error}");
            }
            ensure!(
                Instant::now() < deadline,
                "input receipt timeout for request {id}"
            );
            thread::sleep(PAUSE);
        }
    }
    fn wait_default(&mut self, deadline: Instant) -> Result<()> {
        loop {
            if let Ok(state) = self.state() {
                if closure_in_deadline(
                    default_closed(
                        &state,
                        InputDesktop::open()
                            .is_ok_and(|desktop| desktop.kind() == DesktopKind::Default),
                        consent_processes()?.is_empty(),
                    ),
                    Instant::now(),
                    deadline,
                ) {
                    return Ok(());
                }
            }
            ensure!(
                Instant::now() < deadline,
                "prompt did not close and return to Default within 5 seconds"
            );
            thread::sleep(PAUSE);
        }
    }
    fn deny(&mut self, action_started: Instant) -> Result<Value> {
        self.require_owned()?;
        ensure!(self.link.deny_consent(), "DenyConsent was not queued");
        let deadline = action_started + CLOSE_TIMEOUT;
        loop {
            if let Some(result) = self.link.take_deny_result() {
                ensure!(result.ok, "DenyConsent failed: {}", result.reason);
                return Ok(json!({"ok":result.ok,"reason":result.reason}));
            }
            ensure!(Instant::now() < deadline, "DenyConsent receipt timeout");
            thread::sleep(PAUSE);
        }
    }
    fn settle_attempt(&mut self) -> Result<Value> {
        let deadline = Instant::now() + CLOSE_TIMEOUT;
        let attempt = self.attempt.as_mut().context("no runas request")?;
        while !attempt.finished() {
            ensure!(
                Instant::now() < deadline,
                "ShellExecuteEx thread did not settle after prompt closed"
            );
            thread::sleep(PAUSE);
        }
        attempt.join_if_finished();
        let result = attempt.result.as_ref().unwrap();
        let report =
            json!({"finished":true,"shell_ok":result.shell_ok,"error_code":result.error_code});
        ensure!(
            !result.shell_ok && result.error_code == RUNAS_CANCELLED,
            "runas did not finish with ERROR_CANCELLED: {:?}; no approval input was sent",
            result
        );
        self.attempt = None;
        Ok(report)
    }
    fn scenario(
        &mut self,
        admin: bool,
        legacy: bool,
        name: &str,
        report: &mut Value,
    ) -> Result<()> {
        self.baseline()?;
        self.mark_mutation()?;
        self.set_policy(admin)?;
        self.config.set_mode(legacy)?;
        // Policy is read by the helper on each secure-desktop request. We wait
        // for advertised capabilities below; a local write is not policy proof.
        check_no_viewer(&self.host_log)?;
        self.frame = None;
        self.attempt = Some(UacAttempt::start()?);
        let state = self.wait_prompt()?;
        ensure!(
            state.caps.contains(&"input") == admin && state.caps.contains(&"deny"),
            "helper policy capabilities do not match requested Admin mode"
        );
        report["epoch"] = json!(state.desktop_epoch);
        let first = self.wait_frame(state.desktop_epoch, 0, 0)?;
        let first_name = format!("{name}-first-secure.png");
        save_png(&self.out.join(&first_name), &first)?;
        report["first_frame"] = frame_json(&first, &first_name);
        if !admin {
            self.send(&escape(), false, report, Instant::now())?;
            self.send(&movement(32768, 32768), false, report, Instant::now())?;
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until {
                let current = self.require_owned()?;
                ensure!(
                    current.desktop_epoch == state.desktop_epoch,
                    "OFF prompt changed after rejected input"
                );
                thread::sleep(PAUSE);
            }
            let current = self.require_owned()?;
            ensure!(
                current.desktop_epoch == state.desktop_epoch,
                "OFF prompt changed after the two-second rejection observation"
            );
            let frame = self.frame.as_ref().context("OFF secure frame missing")?;
            ensure!(
                frame.desktop_epoch == state.desktop_epoch,
                "OFF frame is from another prompt"
            );
            // The first frame can predate the consent dialog's fade-in; this
            // one shows whether the dialog itself is captured.
            let before_deny = format!("{name}-before-deny.png");
            save_png(&self.out.join(&before_deny), frame)?;
            report["before_deny_frame"] = frame_json(frame, &before_deny);
            report["uac_remained_after_2s"] = json!(true);
            let close_started = Instant::now();
            report["deny"] = self.deny(close_started)?;
            self.wait_default(close_started + CLOSE_TIMEOUT)?;
            report["closure_elapsed_ms"] = json!(close_started.elapsed().as_millis());
            report["default_within_5s"] = json!(true);
        } else {
            for (position, x, y) in [("center", 32768, 32768), ("corner", 1000, 1000)] {
                let proof = self.send(&movement(x, y), true, report, Instant::now())?;
                let frame =
                    self.wait_frame(state.desktop_epoch, proof.frame_watermark, proof.id)?;
                let name = format!("on-{position}.png");
                save_png(&self.out.join(&name), &frame)?;
                report[format!("{position}_frame")] = frame_json(&frame, &name);
            }
            let close_started = Instant::now();
            self.send(&escape(), true, report, close_started)?;
            self.wait_default(close_started + CLOSE_TIMEOUT)?;
            report["closure_elapsed_ms"] = json!(close_started.elapsed().as_millis());
            report["escape_closed_within_5s"] = json!(true);
        }
        report["trigger"] = self.settle_attempt()?;
        report["success"] = json!(true);
        Ok(())
    }
    /// Keep the Host pipe alive while waiting for a delayed runas request. A
    /// pending trigger is not clean merely because Default is visible now.
    fn cleanup_owned(&mut self) -> Result<Value> {
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        let mut deny_attempts = 0;
        let mut next_deny = Instant::now();
        let mut last_error = None;
        loop {
            let state = self.state();
            if let Some(attempt) = self.attempt.as_mut() {
                attempt.poll();
                if !attempt.finished() {
                    if let Ok(state) = &state {
                        match attempt.identify_owned(state) {
                            Ok(true)
                                if Instant::now() >= next_deny
                                    && deny_attempts < 3
                                    && !attempt.finished() =>
                            {
                                if self.link.deny_consent() {
                                    deny_attempts += 1;
                                    next_deny = Instant::now() + CLOSE_TIMEOUT;
                                }
                            }
                            Err(error) => last_error = Some(format!("{error:#}")),
                            _ => {}
                        }
                    }
                }
                // Drain cleanup receipts so a failed deny can be retried.
                if let Some(result) = self.link.take_deny_result() {
                    if !result.ok {
                        last_error = Some(format!("cleanup DenyConsent: {}", result.reason));
                    }
                }
                if attempt.finished() {
                    attempt.join_if_finished();
                }
            }
            let finished = self.attempt.as_mut().is_none_or(UacAttempt::finished);
            let default = state.as_ref().is_ok_and(|state| !state.secure)
                && InputDesktop::open().is_ok_and(|desktop| desktop.kind() == DesktopKind::Default);
            let no_consent = consent_processes().is_ok_and(|pids| pids.is_empty());
            if cleanup_complete(finished, default, no_consent) {
                if let Some(attempt) = &mut self.attempt {
                    attempt.join_if_finished();
                }
                self.attempt = None;
                return Ok(
                    json!({"success":true,"default":true,"no_consent":true,"trigger_finished":true,"deny_attempts":deny_attempts}),
                );
            }
            ensure!(Instant::now() < deadline, "cleanup failed: default={default}, no_consent={no_consent}, trigger_finished={finished}, last_error={last_error:?}; an outstanding/late prompt cannot be ruled out");
            thread::sleep(PAUSE);
        }
    }
}
fn start_task() -> Result<()> {
    run_task(TASK)
}
fn run_task(task: &str) -> Result<()> {
    let output = std::process::Command::new(system_directory()?.join("schtasks.exe"))
        .args(["/run", "/tn", task])
        .creation_flags(0x08000000)
        .output()?;
    ensure!(
        output.status.success(),
        "installed task {task} did not start: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}
/// Prove that the normal desktop process cannot obtain registry write access.
/// If an unexpectedly writable key is found, attempt to write its existing
/// bytes so a failed assertion cannot enable Admin mode or corrupt the value.
fn non_elevated_write_denied() -> Result<Value> {
    ensure!(
        !trust::is_process_elevated(),
        "registry denial check must be unelevated"
    );
    let path = wide(admin_mode::REGISTRY_KEY);
    let name = wide("AdminMode");
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path.as_ptr()),
            None,
            KEY_SET_VALUE | KEY_WOW64_64KEY,
            &mut key,
        )
    };
    if opened == ERROR_ACCESS_DENIED {
        return Ok(
            json!({"success":true,"elevated":false,"error_code":opened.0,"stage":"open-for-write"}),
        );
    }
    opened
        .ok()
        .context("non-elevated registry write preflight")?;
    let result = (|| {
        let mut read_key = HKEY::default();
        unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path.as_ptr()),
                None,
                KEY_QUERY_VALUE | KEY_WOW64_64KEY,
                &mut read_key,
            )
        }
        .ok()?;
        let mut kind = REG_VALUE_TYPE::default();
        let mut data = vec![0u8; 65536];
        let mut len = data.len() as u32;
        let query = unsafe {
            RegQueryValueExW(
                read_key,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr()),
                Some(&mut len),
            )
        };
        let _ = unsafe { RegCloseKey(read_key) };
        if query == ERROR_FILE_NOT_FOUND {
            kind = REG_DWORD;
            data = 0u32.to_le_bytes().to_vec();
        } else {
            query.ok()?;
            data.truncate(len as usize);
        }
        let written =
            unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, kind, Some(&data)) };
        ensure!(
            written == ERROR_ACCESS_DENIED,
            "non-elevated AdminMode write must fail with access denied, got {}",
            written.0
        );
        Ok(json!({"success":true,"elevated":false,"error_code":written.0,"stage":"set-value"}))
    })();
    let _ = unsafe { RegCloseKey(key) };
    result
}
fn acquire(region: DesktopRegion) -> Result<SecureHelperLink> {
    match SecureHelperLink::connect_host(region) {
        Ok(link) => return Ok(link),
        Err(LinkError::Busy) => {
            bail!("live helper Host slot is Busy; disconnect all viewers before this probe")
        }
        Err(LinkError::Unavailable) => start_task()?,
        Err(error) => {
            return Err(error).context(
                "helper identity/version preflight failed; no configuration or UAC changed",
            )
        }
    }
    let deadline = Instant::now() + UAC_TIMEOUT;
    loop {
        match SecureHelperLink::connect_host(region) {
            Ok(link) => return Ok(link),
            Err(LinkError::Unavailable) if Instant::now() < deadline => thread::sleep(PAUSE),
            Err(LinkError::Busy) => {
                bail!("helper Host slot became Busy; no configuration or UAC changed")
            }
            Err(error) => return Err(error).context("helper acquisition failed"),
        }
    }
}
fn execute(args: &Args, report: &mut Value) -> Result<()> {
    ensure!(
        !trust::is_process_elevated(),
        "run this probe as the normal unelevated desktop user"
    );
    ensure!(
        trust::current_session_id() == unsafe { WTSGetActiveConsoleSessionId() },
        "probe must run in the active console session"
    );
    check_wrapper(args)?;
    check_no_viewer(&args.host_log)?;
    let config = ConfigBackup::read(&args.settings)?;
    report["config"] = json!({"path":args.settings,"original_exists":config.bytes.is_some(),"read_valid":true,"restored":true});
    let region = virtual_region()?;
    report["region"] =
        json!({"x":region.x,"y":region.y,"width":region.width,"height":region.height});
    let link = acquire(region)?;
    let mut harness = Harness {
        link,
        region,
        config,
        out: args.out.clone(),
        host_log: args.host_log.clone(),
        frame: None,
        attempt: None,
        run_id: args.run_id.clone(),
        mutation_marked: false,
        policy_request: 0,
    };
    harness.baseline()?;
    report["ownership"] = json!({"basis":"baseline-no-consent/single-pending-consent","assumption":"no concurrent local UAC launches or tray settings changes during this probe"});
    report["baseline"] = json!({"exclusive_authenticated_host_slot":true,"default":true,"no_consent":true,"no_viewer_in_log":true});
    let result: Result<()> = (|| {
        harness.mark_mutation()?;
        report["non_elevated_registry_write"] = non_elevated_write_denied()?;
        for (admin, legacy, name) in [
            (false, false, "off"),
            (false, true, "legacy-json-on-hklm-off"),
            (true, false, "on"),
        ] {
            let mut scenario = json!({"name":name,"admin_mode":admin,"tray_json_admin_mode":legacy,"success":false,"receipts":[]});
            let result = harness.scenario(admin, legacy, name, &mut scenario);
            if let Err(error) = &result {
                scenario["error"] = json!(format!("{error:#}"));
            }
            report["scenarios"].as_array_mut().unwrap().push(scenario);
            result?;
        }
        Ok(())
    })();
    // Explicit finally ordering: settle/cancel our prompt, restore config, then
    // release the Host slot. Rust Drop is only a backup for unwinding.
    let cleanup = harness.cleanup_owned();
    report["cleanup"] = match &cleanup {
        Ok(value) => value.clone(),
        Err(error) => json!({"success":false,"error":format!("{error:#}")}),
    };
    // Settle our synthetic UAC request before requesting owner elevation for
    // exact policy restoration; the Host connection is still held here.
    let policy_restored = if harness.mutation_marked && cleanup.is_ok() {
        harness.policy_step("restore")
    } else if !harness.mutation_marked {
        Ok(())
    } else {
        Err(anyhow::anyhow!("policy restoration deferred: consent cleanup failed; wrapper must restore after local cleanup"))
    };
    report["registry"] = json!({"restored":policy_restored.is_ok()});
    if let Err(error) = &policy_restored {
        report["registry"]["restore_error"] = json!(format!("{error:#}"));
    }
    report["mutation_marker"] = json!({"run_id":args.run_id,"written":harness.mutation_marked});
    let restored = harness.config.restore();
    report["config"]["restored"] = json!(restored.is_ok());
    if let Err(error) = &restored {
        report["config"]["restore_error"] = json!(format!("{error:#}"));
    }
    harness.link.tick(false, region, Instant::now());
    drop(harness);
    result?;
    cleanup?;
    policy_restored?;
    restored?;
    Ok(())
}
fn main() {
    let args = match args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error:#}");
            std::process::exit(2);
        }
    };
    if let Err(error) = fs::create_dir_all(&args.out) {
        eprintln!("output directory: {error}");
        std::process::exit(2);
    }
    let report_path = args.out.join("summary.json");
    if report_path.exists() {
        eprintln!("refusing to replace existing summary.json; choose a new -OutDir");
        std::process::exit(2);
    }
    let mut report = json!({"schema":"bubbleplay.secure-desktop-probe.v1","started_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),"required_protocol":{"min":protocol::PROTOCOL_MIN,"max":protocol::PROTOCOL_MAX},"success":false,"scenarios":[],"cleanup":{"success":true,"not_started":true}});
    let result = execute(&args, &mut report);
    report["success"] = json!(result.is_ok());
    if let Err(error) = &result {
        report["error"] = json!(format!("{error:#}"));
        if let Some(LinkError::Incompatible {
            helper_min,
            helper_max,
        }) = error.downcast_ref::<LinkError>()
        {
            report["helper_protocol"] = json!({"min":helper_min,"max":helper_max});
        }
        eprintln!("{error:#}");
    }
    let write = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&report_path)?;
        file.write_all(&serde_json::to_vec_pretty(&report)?)?;
        file.write_all(b"\n")?;
        Ok(())
    })();
    if let Err(error) = write {
        eprintln!("summary write failed: {error:#}");
        std::process::exit(1);
    }
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_ack_requires_exact_run_request_and_success() {
        let mut ack = json!({"run_id":"owner-run", "id":2, "success":true});
        assert!(policy_acknowledged(&ack, "owner-run", 2));
        assert!(!policy_acknowledged(&ack, "another-run", 2));
        assert!(!policy_acknowledged(&ack, "owner-run", 1));
        ack["success"] = json!(false);
        assert!(!policy_acknowledged(&ack, "owner-run", 2));
        ack["success"] = json!("true");
        assert!(!policy_acknowledged(&ack, "owner-run", 2));
        assert!(!policy_acknowledged(&json!({}), "owner-run", 2));
    }
    #[test]
    fn allowlist_has_only_escape_pair_and_absolute_move() {
        assert!(safe_records(&escape()));
        assert!(safe_records(&movement(32768, 32768)));
        assert!(safe_records(&movement(1000, 1000)));
        assert!(!safe_records(&[]));
        assert!(!safe_records(&escape()[..1]));
        for vk in [0x0d, 0x20, 0x09, 0x59, 0x12] {
            assert!(!safe_records(&[
                Record::Key {
                    vk,
                    scan: 0,
                    flags: 0
                },
                Record::Key {
                    vk,
                    scan: 0,
                    flags: KEYEVENTF_KEYUP
                }
            ]));
        }
        for flags in [
            MOUSEEVENTF_MOVE,
            MOVE_FLAGS | 2,
            MOVE_FLAGS | 4,
            MOVE_FLAGS | 0x800,
        ] {
            assert!(!safe_records(&[Record::Mouse {
                dx: 0,
                dy: 0,
                data: 0,
                flags
            }]));
        }
        assert!(!safe_records(&movement(-1, 10)));
        assert!(!safe_records(&movement(0, 65536)));
        assert!(!safe_records(&[escape(), movement(0, 0)].concat()));
    }
    #[test]
    fn receipts_require_identity_complete_counts_and_policy_reason() {
        let mut r = InputReceipt {
            id: 7,
            received: 2,
            accepted: 0,
            applied: 0,
            error_code: 0,
            reason: "policy-off".into(),
        };
        assert!(receipt_passes(&r, 7, 2, false));
        assert!(!receipt_passes(&r, 8, 2, false));
        assert!(!receipt_passes(&r, 7, 2, true));
        r.reason = "ok".into();
        r.accepted = 2;
        r.applied = 2;
        assert!(receipt_passes(&r, 7, 2, true));
        r.applied = 1;
        assert!(!receipt_passes(&r, 7, 2, true));
        r.applied = 2;
        r.error_code = 5;
        assert!(!receipt_passes(&r, 7, 2, true));
    }
    #[test]
    fn frame_proof_requires_new_sequence_and_same_secure_epoch() {
        let f = BgraFrame {
            seq: 10,
            width: 1,
            height: 1,
            stride: 4,
            pixels: vec![0; 4],
            desktop_epoch: 8,
            input_id: 4,
        };
        assert!(fresh_frame(&f, 8, 9, 4));
        assert!(!fresh_frame(&f, 8, 10, 4));
        assert!(!fresh_frame(&f, 7, 9, 4));
        assert!(!fresh_frame(&f, 8, 9, 5));
    }
    #[test]
    fn closure_and_cleanup_cannot_pass_on_an_unsettled_trigger() {
        let mut state = SecureState {
            secure: false,
            kind: "other",
            caps: vec![],
            available: false,
            desktop_epoch: 0,
        };
        assert!(default_closed(&state, true, true));
        assert!(!default_closed(&state, false, true));
        assert!(!default_closed(&state, true, false));
        state.secure = true;
        assert!(!default_closed(&state, true, true));
        assert!(cleanup_complete(true, true, true));
        assert!(!cleanup_complete(false, true, true));
        assert!(!cleanup_complete(true, false, true));
        assert!(!cleanup_complete(true, true, false));
    }
    #[test]
    fn closure_deadline_includes_receipt_time() {
        let start = Instant::now();
        let deadline = start + CLOSE_TIMEOUT;
        assert!(closure_in_deadline(
            true,
            start + Duration::from_secs(5),
            deadline
        ));
        assert!(!closure_in_deadline(
            true,
            start + Duration::from_millis(5001),
            deadline
        ));
        assert!(!closure_in_deadline(
            false,
            start + Duration::from_secs(1),
            deadline
        ));
    }
    fn fixture_dir() -> PathBuf {
        let name = format!(
            "bubbleplay-secure-probe-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(name);
        fs::create_dir(&path).unwrap();
        path
    }
    fn remove_fixture(path: &Path) {
        let resolved = path.canonicalize().unwrap();
        assert!(resolved.starts_with(std::env::temp_dir().canonicalize().unwrap()));
        assert!(resolved
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("bubbleplay-secure-probe-test-"));
        fs::remove_dir_all(resolved).unwrap();
    }
    #[test]
    fn config_restores_original_bytes_after_scenario_error() {
        let directory = fixture_dir();
        let path = directory.join("tray.json");
        let bytes = b"{ \"adminMode\" : false, \"startAtLogin\":true }\r\n";
        fs::write(&path, bytes).unwrap();
        {
            let mut backup = ConfigBackup::read(&path).unwrap();
            let failed: Result<()> = (|| {
                backup.set_mode(true)?;
                bail!("synthetic receipt/PNG failure")
            })();
            assert!(failed.is_err());
            backup.restore().unwrap();
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        remove_fixture(&directory);
    }
    #[test]
    fn config_drop_restores_initially_missing_file_after_error() {
        let directory = fixture_dir();
        let path = directory.join("tray.json");
        {
            let mut backup = ConfigBackup::read(&path).unwrap();
            assert!(backup.bytes.is_none());
            backup.set_mode(true).unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists());
        remove_fixture(&directory);
    }
    #[test]
    fn viewer_log_preflight_is_conservative() {
        assert!(no_viewer_in_log("host started"));
        assert!(!no_viewer_in_log("viewer session started"));
        assert!(no_viewer_in_log(
            "viewer session started\nviewer session ended"
        ));
        assert!(!no_viewer_in_log(
            "viewer session ended\nviewer session started"
        ));
    }
    #[test]
    fn png_conversion_checks_frame_length_and_bgra_channels() {
        let mut f = BgraFrame {
            seq: 1,
            width: 1,
            height: 1,
            stride: 4,
            pixels: vec![3, 2, 1, 0],
            desktop_epoch: 1,
            input_id: 0,
        };
        assert_eq!(rgba(&f).unwrap(), vec![1, 2, 3, 255]);
        f.pixels.pop();
        assert!(rgba(&f).is_err());
    }
}
