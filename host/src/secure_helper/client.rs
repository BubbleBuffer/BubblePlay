//! The streaming loop only touches bounded queues/latest snapshots; all pipe
//! I/O, connection verification and frame copies run on one background thread.
use super::{
    mapping::Mapping,
    pipe,
    protocol::{self, FrameInfo, Message, Role},
    trust,
};
use crate::{
    admin_agent::protocol::{ClientMessage, Record},
    input::DesktopRegion,
};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    os::windows::io::FromRawHandle,
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use windows::{
    core::PCWSTR,
    Win32::{
        Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
        },
        System::Pipes::GetNamedPipeServerProcessId,
    },
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    Busy,
    Unavailable,
    Incompatible { helper_min: u16, helper_max: u16 },
    Untrusted,
    Protocol,
}
pub use super::protocol::InputReceipt;
impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "secure helper: {self:?}")
    }
}
impl std::error::Error for LinkError {}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureState {
    pub secure: bool,
    pub kind: &'static str,
    pub caps: Vec<&'static str>,
    pub available: bool,
    pub desktop_epoch: u64,
}
#[derive(Debug, Clone)]
pub struct BgraFrame {
    pub input_id: u64,
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixels: Vec<u8>,
    pub desktop_epoch: u64,
}
#[derive(Debug, Clone)]
pub struct DenyResult {
    pub ok: bool,
    pub reason: String,
}
#[derive(Default)]
struct Snapshot {
    state: Option<SecureState>,
    frame: Option<BgraFrame>,
    deny: Option<DenyResult>,
    deny_at: Option<Instant>,
    deny_id: u64,
    error: Option<LinkError>,
    active: bool,
    region: Option<protocol::Region>,
    input_id: u64,
    receipts: VecDeque<InputReceipt>,
    pending: BTreeMap<u64, u16>,
    counters: super::stats::Counters,
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
fn connect(role: Role) -> Result<File, LinkError> {
    let session = crate::admin_agent::trust::current_session_id();
    let w = trust::wide(protocol::pipe_name(session));
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            crate::admin_agent::client::PIPE_CLIENT_ACCESS,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0x110000),
            None,
        )
    }
    .map_err(|_| LinkError::Unavailable)?;
    let mut f = unsafe { File::from_raw_handle(h.0.cast()) };
    let mut pid = 0;
    unsafe { GetNamedPipeServerProcessId(pipe::handle(&f), &mut pid) }
        .map_err(|_| LinkError::Untrusted)?;
    let image = crate::admin_agent::trust::process_image_path(pid).ok_or(LinkError::Untrusted)?;
    if crate::admin_agent::trust::process_session_id(pid) != Some(session)
        || trust::pipe_owner(pipe::handle(&f)).ok().as_deref() != Some("S-1-5-18")
        || trust::protected_image(&image).is_err()
    {
        return Err(LinkError::Untrusted);
    }
    protocol::write(
        &mut f,
        &Message::Hello {
            min: protocol::PROTOCOL_MIN,
            max: protocol::PROTOCOL_MAX,
            role: role as u8,
        },
    )
    .map_err(|_| LinkError::Protocol)?;
    match pipe::read_deadline(&mut f, Instant::now() + Duration::from_secs(2))
        .map_err(|_| LinkError::Protocol)?
    {
        Message::Welcome {
            version,
            min,
            max,
            pid: claimed,
        } if claimed == pid
            && crate::admin_agent::protocol::negotiate(
                protocol::PROTOCOL_MIN,
                protocol::PROTOCOL_MAX,
                min,
                max,
            ) == Some(version) =>
        {
            Ok(f)
        }
        Message::Incompatible { min, max } => Err(LinkError::Incompatible {
            helper_min: min,
            helper_max: max,
        }),
        Message::Busy => Err(LinkError::Busy),
        _ => Err(LinkError::Protocol),
    }
}
pub fn probe() -> Result<(), LinkError> {
    connect(Role::Probe).map(|_| ())
}
enum Command {
    Input(Vec<Record>, u64, u64),
    Deny(u64, u64),
}
pub struct SecureHelperLink {
    snapshot: Arc<Mutex<Snapshot>>,
    send: SyncSender<Command>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}
impl Default for SecureHelperLink {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for SecureHelperLink {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
    }
}
impl SecureHelperLink {
    pub fn new() -> Self {
        Self::start(Snapshot::default(), None)
    }
    /// Acquire the real exclusive Host role before an owner-driven live probe
    /// changes policy or opens a prompt. Busy does not disturb the current Host.
    pub fn connect_host(region: DesktopRegion) -> Result<Self, LinkError> {
        let r = protocol::Region {
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
        };
        r.validate().map_err(|_| LinkError::Protocol)?;
        let file = connect(Role::Host)?;
        let snapshot = Snapshot {
            active: true,
            region: Some(r),
            ..Default::default()
        };
        Ok(Self::start(snapshot, Some(file)))
    }
    fn start(initial: Snapshot, file: Option<File>) -> Self {
        let snapshot = Arc::new(Mutex::new(initial));
        let (s, r) = mpsc::sync_channel(32);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ss = snapshot.clone();
        let st = stop.clone();
        std::thread::spawn(move || background(ss, r, st, file));
        Self {
            snapshot,
            send: s,
            stop,
        }
    }
    pub fn tick(&mut self, active: bool, region: DesktopRegion, _now: Instant) {
        let mut s = lock(&self.snapshot);
        s.active = active;
        let r = protocol::Region {
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
        };
        if s.region != Some(r) {
            s.frame = None;
            s.state = None;
        }
        s.region = Some(r);
        if !active {
            s.state = None;
            s.frame = None;
            if s.deny_at.take().is_some() {
                s.deny = Some(DenyResult {
                    ok: false,
                    reason: "viewer-disconnected".into(),
                });
            }
        }
    }
    pub fn state(&self) -> Option<SecureState> {
        lock(&self.snapshot).state.clone()
    }
    pub fn take_frame(&mut self) -> Option<BgraFrame> {
        lock(&self.snapshot).frame.take()
    }
    pub fn send_input(&mut self, records: &[Record]) -> bool {
        if records.is_empty()
            || records.len() > 1024
            || records.iter().any(|r| r.validate().is_err())
        {
            lock(&self.snapshot)
                .counters
                .reject(records.len() as u64, "client-invalid");
            return false;
        }
        self.enqueue_input(records, false).is_ok()
    }
    pub fn send_probe_input(&mut self, records: &[Record]) -> Result<u64, LinkError> {
        if !safe_probe_records(records) {
            lock(&self.snapshot)
                .counters
                .reject(records.len() as u64, "client-invalid");
            return Err(LinkError::Protocol);
        }
        self.enqueue_input(records, true)
    }
    fn enqueue_input(&mut self, records: &[Record], probe: bool) -> Result<u64, LinkError> {
        let mut s = lock(&self.snapshot);
        let count = records.len() as u64;
        let Some(state) = s.state.as_ref() else {
            s.counters.reject(count, "not_ready");
            return Err(LinkError::Unavailable);
        };
        if !state.secure {
            s.counters.reject(count, "not_secure");
            return Err(LinkError::Unavailable);
        }
        if !probe && (!state.available || !state.caps.contains(&"input")) {
            s.counters.reject(count, "no_input_capability");
            return Err(LinkError::Unavailable);
        }
        let epoch = state.desktop_epoch;
        let chunks = records.len().div_ceil(256) as u64;
        let id = s.input_id.checked_add(1).ok_or(LinkError::Protocol)?;
        let last = s.input_id.checked_add(chunks).ok_or(LinkError::Protocol)?;
        if s.pending.len() + chunks as usize > 128
            || self
                .send
                .try_send(Command::Input(records.to_vec(), epoch, id))
                .is_err()
        {
            s.counters.reject(count, "queue-full");
            return Err(LinkError::Protocol);
        }
        s.input_id = last;
        s.counters.queued += count;
        for (i, chunk) in records.chunks(256).enumerate() {
            s.pending.insert(id + i as u64, chunk.len() as u16);
        }
        Ok(id)
    }
    pub fn take_input_receipt(&mut self) -> Option<InputReceipt> {
        lock(&self.snapshot).receipts.pop_front()
    }
    pub fn note_input_drop(&mut self, records: u64, reason: &'static str) {
        let reason = match reason {
            "no_input_capability" => "no_input_capability",
            "not_secure" => "not_secure",
            _ => "not_ready",
        };
        lock(&self.snapshot).counters.reject(records, reason);
    }
    pub fn record_drop_no_cap(&mut self, count: usize) {
        self.note_input_drop(count as u64, "no_input_capability");
    }
    pub fn deny_consent(&mut self) -> bool {
        let mut s = lock(&self.snapshot);
        if s.deny_at.is_some() {
            return false;
        }
        let Some(state) = s
            .state
            .as_ref()
            .filter(|s| s.secure && s.caps.contains(&"deny"))
        else {
            return false;
        };
        let Some(id) = s.deny_id.checked_add(1) else {
            return false;
        };
        if self
            .send
            .try_send(Command::Deny(state.desktop_epoch, id))
            .is_err()
        {
            return false;
        }
        s.deny = None;
        s.deny_id = id;
        s.deny_at = Some(Instant::now());
        true
    }
    pub fn take_deny_result(&mut self) -> Option<DenyResult> {
        lock(&self.snapshot).deny.take()
    }
    pub fn last_error(&self) -> Option<LinkError> {
        lock(&self.snapshot).error.clone()
    }
}
fn background(
    shared: Arc<Mutex<Snapshot>>,
    commands: Receiver<Command>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    initial: Option<File>,
) {
    let mut connection = initial;
    let mut mapping = None;
    let mut retry = Instant::now();
    let mut poll_at = Instant::now();
    let mut last = 0;
    let mut last_reply = Instant::now();
    while !stop.load(std::sync::atomic::Ordering::Acquire) {
        let (active, region) = {
            let mut s = lock(&shared);
            let epoch = s.state.as_ref().map_or(0, |s| s.desktop_epoch);
            let secure = s.state.as_ref().is_some_and(|s| s.secure);
            let admin = s.state.as_ref().is_some_and(|s| s.caps.contains(&"input"));
            s.counters.log("host-link", epoch, secure, admin);
            if s.deny_at
                .is_some_and(|at| at.elapsed() > Duration::from_secs(5))
            {
                s.deny_at = None;
                s.deny = Some(DenyResult {
                    ok: false,
                    reason: "deny-timeout".into(),
                });
            }
            (s.active, s.region)
        };
        if !active {
            connection = None;
            mapping = None;
            fail_pending(&mut lock(&shared));
            while commands.try_recv().is_ok() {}
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        if connection.is_none() && Instant::now() >= retry {
            match connect(Role::Host) {
                Ok(f) => {
                    connection = Some(f);
                    last = 0;
                    mapping = None;
                    poll_at = Instant::now();
                    last_reply = Instant::now();
                    lock(&shared).error = None;
                }
                Err(e) => {
                    let mut s = lock(&shared);
                    // Unavailable is the normal state without the helper task;
                    // anything else means a helper is up but was refused.
                    if e != LinkError::Unavailable && s.error.as_ref() != Some(&e) {
                        tracing::warn!(error = ?e, "secure helper connect refused");
                    }
                    s.error = Some(e);
                    drop(s);
                    retry = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        if let Some(file) = connection.as_mut() {
            let result = (|| -> anyhow::Result<()> {
                if Instant::now() >= poll_at {
                    let region = region.ok_or_else(|| anyhow::anyhow!("no region"))?;
                    region.validate()?;
                    protocol::write(file, &Message::Poll { region })?;
                    poll_at = Instant::now() + Duration::from_millis(100);
                }
                for _ in 0..8 {
                    let Ok(c) = commands.try_recv() else {
                        break;
                    };
                    match c {
                        Command::Input(r, epoch, first_id) => {
                            for (i, chunk) in r.chunks(256).enumerate() {
                                let (_, records) = ClientMessage::Inject(chunk.to_vec()).encode();
                                protocol::write(
                                    file,
                                    &Message::Input {
                                        id: first_id + i as u64,
                                        records,
                                        epoch,
                                    },
                                )?;
                                lock(&shared).counters.forwarded += chunk.len() as u64;
                            }
                        }
                        Command::Deny(epoch, id) => {
                            protocol::write(file, &Message::DenyConsent { epoch, id })?
                        }
                    }
                }
                for _ in 0..16 {
                    let Some(m) = pipe::poll(file)? else {
                        break;
                    };
                    last_reply = Instant::now();
                    match m {
                        Message::InputReceipt(receipt) => {
                            receipt.validate()?;
                            let mut s = lock(&shared);
                            let expected = s
                                .pending
                                .remove(&receipt.id)
                                .ok_or_else(|| anyhow::anyhow!("unexpected input receipt"))?;
                            anyhow::ensure!(
                                expected == receipt.received,
                                "input receipt count mismatch"
                            );
                            s.counters.receipt(&receipt);
                            push_receipt(&mut s, receipt);
                        }
                        Message::Mapping { name } => {
                            anyhow::ensure!(mapping.is_none(), "duplicate mapping");
                            mapping = Some(Mapping::open(
                                &name,
                                crate::admin_agent::trust::current_session_id(),
                            )?);
                        }
                        Message::State {
                            secure,
                            kind,
                            admin,
                            available,
                            epoch,
                        } => {
                            let state = SecureState {
                                secure,
                                kind: kind.name(),
                                caps: protocol::caps(secure, kind, admin, available),
                                available,
                                desktop_epoch: epoch,
                            };
                            let mut s = lock(&shared);
                            if s.state.as_ref() != Some(&state) {
                                s.frame = None;
                            }
                            s.state = Some(state);
                        }
                        Message::Frame {
                            input_id,
                            slot,
                            seq,
                            width,
                            height,
                            stride,
                            hash,
                            epoch,
                        } => {
                            let info = FrameInfo {
                                slot,
                                seq,
                                width,
                                height,
                                stride,
                                hash,
                                epoch,
                            };
                            let pixels = mapping
                                .as_ref()
                                .ok_or_else(|| anyhow::anyhow!("missing mapping"))?
                                .copy(info, last)?;
                            last = seq;
                            protocol::write(file, &Message::Ack { slot, seq })?;
                            let mut s = lock(&shared);
                            if s.state.as_ref().is_some_and(|s| {
                                s.secure && s.available && s.desktop_epoch == epoch
                            }) {
                                s.frame = Some(BgraFrame {
                                    input_id,
                                    seq,
                                    width,
                                    height,
                                    stride,
                                    pixels,
                                    desktop_epoch: epoch,
                                });
                            }
                        }
                        Message::Denied { id, ok, reason } => {
                            let mut s = lock(&shared);
                            if id == s.deny_id && s.deny_at.take().is_some() {
                                s.deny = Some(DenyResult { ok, reason });
                            }
                        }
                        Message::Refused => {}
                        _ => anyhow::bail!("unexpected helper message"),
                    }
                }
                anyhow::ensure!(
                    last_reply.elapsed() < Duration::from_secs(6),
                    "helper stopped replying"
                );
                Ok(())
            })();
            if result.is_err() {
                connection = None;
                mapping = None;
                let mut s = lock(&shared);
                s.state = None;
                s.frame = None;
                s.error = Some(LinkError::Protocol);
                fail_pending(&mut s);
                if s.deny_at.take().is_some() {
                    s.deny = Some(DenyResult {
                        ok: false,
                        reason: "helper-disconnected".into(),
                    });
                }
                retry = Instant::now() + Duration::from_secs(2);
                while commands.try_recv().is_ok() {}
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn push_receipt(s: &mut Snapshot, r: InputReceipt) {
    if s.receipts.len() >= 128 {
        s.receipts.pop_front();
    }
    s.receipts.push_back(r);
}
fn fail_pending(s: &mut Snapshot) {
    for (id, received) in std::mem::take(&mut s.pending) {
        s.counters.reject(u64::from(received), "link-lost");
        push_receipt(s, InputReceipt::rejected(id, received, "link-lost"));
    }
}
fn safe_probe_records(records: &[Record]) -> bool {
    use crate::admin_agent::protocol::{
        KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK,
    };
    if records
        == [
            Record::Key {
                vk: 0x1b,
                scan: 0,
                flags: 0,
            },
            Record::Key {
                vk: 0x1b,
                scan: 0,
                flags: KEYEVENTF_KEYUP,
            },
        ]
    {
        return true;
    }
    !records.is_empty()&&records.len()<=256&&records.iter().all(|r|matches!(*r,Record::Mouse{data:0,flags,..}if flags==MOUSEEVENTF_MOVE|MOUSEEVENTF_ABSOLUTE||flags==MOUSEEVENTF_MOVE|MOUSEEVENTF_ABSOLUTE|MOUSEEVENTF_VIRTUALDESK)&&r.validate().is_ok())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn probe_bypass_is_bounded_to_safe_input() {
        use crate::admin_agent::protocol::*;
        assert!(safe_probe_records(&[
            Record::Key {
                vk: 0x1b,
                scan: 0,
                flags: 0
            },
            Record::Key {
                vk: 0x1b,
                scan: 0,
                flags: KEYEVENTF_KEYUP
            }
        ]));
        assert!(!safe_probe_records(&[Record::Key {
            vk: 0x1b,
            scan: 0,
            flags: 0
        }]));
        let motion = Record::Mouse {
            dx: 32768,
            dy: 32768,
            data: 0,
            flags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        };
        assert!(safe_probe_records(&[motion]));
        assert!(!safe_probe_records(&vec![motion; 257]));
        assert!(!safe_probe_records(&[Record::Mouse {
            dx: 0,
            dy: 0,
            data: 0,
            flags: MOUSEEVENTF_LEFTDOWN
        }]));
        assert!(!safe_probe_records(&[Record::Key {
            vk: 0,
            scan: 0x41,
            flags: KEYEVENTF_UNICODE
        }]));
        assert!(!safe_probe_records(&[]));
    }
}
