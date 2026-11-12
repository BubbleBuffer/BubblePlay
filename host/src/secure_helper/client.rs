//! The streaming loop only touches bounded queues/latest snapshots; all pipe
//! I/O, connection verification and frame copies run on one background thread.
use super::{
    mapping::Mapping,
    pipe,
    protocol::{self, FrameInfo, Message, Role},
    trust,
};
use crate::{
    activity::Activity,
    admin_agent::protocol::{ClientMessage, Record},
    deadline_pipe::BoundedPipe,
    input::DesktopRegion,
};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use windows::Win32::System::Pipes::{GetNamedPipeServerProcessId, PeekNamedPipe};
/// Hello and Welcome share one deadline, so a silent helper cannot hold connect.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
/// Each background write or read has its own bound; the loop never waits longer.
const MESSAGE_TIMEOUT: Duration = Duration::from_millis(500);
/// Pause between peeks while the handshake waits for its reply.
const FRAME_POLL: Duration = Duration::from_millis(5);
const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
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
    activity: Option<Activity>,
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
impl Snapshot {
    fn notify(&self) {
        if let Some(activity) = &self.activity {
            activity.notify();
        }
    }

    fn publish_state(&mut self, state: SecureState) {
        if self.state.as_ref() != Some(&state) {
            self.frame = None;
            self.state = Some(state);
            self.notify();
        }
    }

    fn publish_error(&mut self, error: Option<LinkError>) {
        if self.error != error {
            self.error = error;
            self.notify();
        }
    }

    fn publish_deny(&mut self, result: DenyResult) {
        self.deny = Some(result);
        self.notify();
    }
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
fn connect(role: Role) -> Result<BoundedPipe, LinkError> {
    let session = crate::admin_agent::trust::current_session_id();
    let mut pipe = BoundedPipe::open(
        &protocol::pipe_name(session),
        crate::admin_agent::client::PIPE_CLIENT_ACCESS,
        SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
    )
    .map_err(|_| LinkError::Unavailable)?;
    let file = pipe.file().map_err(|_| LinkError::Untrusted)?;
    let mut pid = 0;
    unsafe { GetNamedPipeServerProcessId(pipe::handle(file), &mut pid) }
        .map_err(|_| LinkError::Untrusted)?;
    let image = crate::admin_agent::trust::process_image_path(pid).ok_or(LinkError::Untrusted)?;
    if crate::admin_agent::trust::process_session_id(pid) != Some(session)
        || trust::pipe_owner(pipe::handle(file)).ok().as_deref() != Some("S-1-5-18")
        || trust::protected_image(&image).is_err()
    {
        return Err(LinkError::Untrusted);
    }
    handshake(&mut pipe, role, pid, Instant::now() + HANDSHAKE_TIMEOUT)?;
    Ok(pipe)
}
/// The version handshake for a server whose process has already been verified.
fn handshake(
    pipe: &mut BoundedPipe,
    role: Role,
    pid: u32,
    deadline: Instant,
) -> Result<(), LinkError> {
    send(
        pipe,
        &Message::Hello {
            min: protocol::PROTOCOL_MIN,
            max: protocol::PROTOCOL_MAX,
            role: role as u8,
        },
        deadline,
    )
    .map_err(|_| LinkError::Protocol)?;
    match read_message_until(pipe, deadline).map_err(|_| LinkError::Protocol)? {
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
            Ok(())
        }
        Message::Incompatible { min, max } => Err(LinkError::Incompatible {
            helper_min: min,
            helper_max: max,
        }),
        Message::Busy => Err(LinkError::Busy),
        _ => Err(LinkError::Protocol),
    }
}
/// Encode first, so the only I/O is one bounded write.
fn send(pipe: &mut BoundedPipe, message: &Message, deadline: Instant) -> io::Result<()> {
    let mut frame = Vec::new();
    protocol::write(&mut frame, message)?;
    pipe.write_all_until(&frame, deadline)
}
fn send_background(pipe: &mut BoundedPipe, message: &Message) -> io::Result<()> {
    send(pipe, message, Instant::now() + MESSAGE_TIMEOUT)
}
/// True once a whole frame is queued. Only peeks, so a truncated frame stays
/// in the pipe and nothing blocks waiting for its tail.
fn frame_queued(pipe: &BoundedPipe) -> io::Result<bool> {
    let mut header = [0; protocol::HEADER_LEN];
    let mut read = 0;
    let mut available = 0;
    // SAFETY: the handle belongs to this pipe; the buffer is exactly the header.
    unsafe {
        PeekNamedPipe(
            pipe::handle(pipe.file()?),
            Some(header.as_mut_ptr().cast()),
            protocol::HEADER_LEN as u32,
            Some(&mut read),
            Some(&mut available),
            None,
        )
    }
    .map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xffff))?;
    if (read as usize) < protocol::HEADER_LEN {
        return Ok(false);
    }
    let len = protocol::payload_len(&header)?;
    Ok(available as usize >= protocol::HEADER_LEN + len)
}
/// Reads a frame already known to be queued. Header and payload share one
/// deadline, and the decode runs over the in-memory copy.
fn read_frame_until(pipe: &mut BoundedPipe, deadline: Instant) -> io::Result<Message> {
    let mut frame = vec![0; protocol::HEADER_LEN];
    pipe.read_exact_until(&mut frame, deadline)?;
    let len = protocol::payload_len(&frame)?;
    frame.resize(protocol::HEADER_LEN + len, 0);
    pipe.read_exact_until(&mut frame[protocol::HEADER_LEN..], deadline)?;
    protocol::read(&mut frame.as_slice())
}
fn read_message_until(pipe: &mut BoundedPipe, deadline: Instant) -> io::Result<Message> {
    loop {
        if frame_queued(pipe)? {
            return read_frame_until(pipe, deadline);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "helper reply timed out",
            ));
        }
        std::thread::sleep(FRAME_POLL);
    }
}
/// Non-blocking poll: a message is returned only when its whole frame is queued.
fn poll_message(pipe: &mut BoundedPipe) -> io::Result<Option<Message>> {
    if !frame_queued(pipe)? {
        return Ok(None);
    }
    read_frame_until(pipe, Instant::now() + MESSAGE_TIMEOUT).map(Some)
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
    /// Wake the streaming loop only when the background thread publishes work.
    /// One notification on attachment also exposes already-queued snapshots.
    pub fn set_activity(&self, activity: Activity) {
        let mut snapshot = lock(&self.snapshot);
        snapshot.activity = Some(activity);
        snapshot.notify();
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
    fn start(initial: Snapshot, file: Option<BoundedPipe>) -> Self {
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
        let mut changed = s.active != active;
        s.active = active;
        let r = protocol::Region {
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
        };
        if s.region != Some(r) {
            changed = true;
            s.frame = None;
            s.state = None;
        }
        s.region = Some(r);
        if !active {
            s.state = None;
            s.frame = None;
            if s.deny_at.take().is_some() {
                s.publish_deny(DenyResult {
                    ok: false,
                    reason: "viewer-disconnected".into(),
                });
            }
        }
        if changed {
            s.notify();
        }
    }
    pub fn state(&self) -> Option<SecureState> {
        lock(&self.snapshot).state.clone()
    }
    /// The helper verified that the normal-desktop foreground is above the
    /// admin agent's integrity and accepts input for it (protocol v3).
    pub fn elevated_input_ready(&self) -> bool {
        lock(&self.snapshot).state.as_ref().is_some_and(|state| {
            !state.secure
                && state.kind == protocol::Kind::ElevatedForeground.name()
                && state.available
                && state.caps.contains(&"input")
        })
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
        // Normal-desktop input is accepted only for the helper-verified
        // elevated foreground, and never for probes.
        let elevated = !probe && state.kind == protocol::Kind::ElevatedForeground.name();
        if !state.secure && !elevated {
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
    initial: Option<BoundedPipe>,
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
                s.publish_deny(DenyResult {
                    ok: false,
                    reason: "deny-timeout".into(),
                });
            }
            (s.active, s.region)
        };
        if !active {
            let disconnected = connection.is_some();
            connection = None;
            mapping = None;
            let mut s = lock(&shared);
            fail_pending(&mut s);
            if disconnected {
                s.notify();
            }
            drop(s);
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
                    lock(&shared).publish_error(None);
                }
                Err(e) => {
                    let mut s = lock(&shared);
                    // Unavailable is the normal state without the helper task;
                    // anything else means a helper is up but was refused.
                    if e != LinkError::Unavailable && s.error.as_ref() != Some(&e) {
                        tracing::warn!(error = ?e, "secure helper connect refused");
                    }
                    s.publish_error(Some(e));
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
                    send_background(file, &Message::Poll { region })?;
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
                                send_background(
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
                            send_background(file, &Message::DenyConsent { epoch, id })?
                        }
                    }
                }
                for _ in 0..16 {
                    let Some(m) = poll_message(file)? else {
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
                            s.publish_state(state);
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
                            send_background(file, &Message::Ack { slot, seq })?;
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
                                s.notify();
                            }
                        }
                        Message::Denied { id, ok, reason } => {
                            let mut s = lock(&shared);
                            if id == s.deny_id && s.deny_at.take().is_some() {
                                s.publish_deny(DenyResult { ok, reason });
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
                s.publish_error(Some(LinkError::Protocol));
                fail_pending(&mut s);
                if s.deny_at.take().is_some() {
                    s.publish_deny(DenyResult {
                        ok: false,
                        reason: "helper-disconnected".into(),
                    });
                }
                s.notify();
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
    s.notify();
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
    fn repeated_helper_reports_do_not_keep_the_producer_awake() {
        let activity = Activity::new().unwrap();
        let mut snapshot = Snapshot {
            activity: Some(activity.clone()),
            ..Default::default()
        };
        let state = SecureState {
            secure: true,
            kind: "uac",
            caps: vec!["view", "deny"],
            available: true,
            desktop_epoch: 1,
        };
        snapshot.publish_state(state.clone());
        snapshot.publish_error(Some(LinkError::Unavailable));
        activity.wait(Duration::ZERO, &[]).unwrap();
        for _ in 0..1_000 {
            snapshot.publish_state(state.clone());
            snapshot.publish_error(Some(LinkError::Unavailable));
            fail_pending(&mut snapshot);
        }
        let started = Instant::now();
        activity.wait(Duration::from_millis(30), &[]).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(20));
        snapshot.publish_error(None);
        let started = Instant::now();
        activity.wait(Duration::from_secs(2), &[]).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn background_receipts_and_deny_results_wake_the_idle_producer() {
        let activity = Activity::new().unwrap();
        let mut snapshot = Snapshot {
            activity: Some(activity.clone()),
            ..Default::default()
        };
        snapshot.pending.insert(7, 1);
        fail_pending(&mut snapshot);
        let started = Instant::now();
        activity.wait(Duration::from_secs(2), &[]).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(snapshot.receipts.len(), 1);
        snapshot.publish_deny(DenyResult {
            ok: false,
            reason: "deny-timeout".into(),
        });
        let started = Instant::now();
        activity.wait(Duration::from_secs(2), &[]).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

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

    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE},
            Storage::FileSystem::{WriteFile, PIPE_ACCESS_DUPLEX},
            System::Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_TYPE_BYTE, PIPE_WAIT,
            },
        },
    };

    /// Test-owned listening pipe. It never reaches the real helper: connect()'s
    /// trust checks are not run, and nothing here can inject input.
    struct Server(HANDLE);

    impl Drop for Server {
        fn drop(&mut self) {
            // SAFETY: owned handle, closed once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    fn private_pipe(suffix: &str) -> (Server, BoundedPipe) {
        let name = format!(
            r"\\.\pipe\BubblePlay.SecureHelper.client-{}-{suffix}",
            std::process::id()
        );
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: NUL-terminated name, default security, one private instance.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                4096,
                4096,
                0,
                None,
            )
        };
        assert!(!handle.is_invalid());
        let server = Server(handle);
        let pipe = BoundedPipe::open(
            &name,
            crate::admin_agent::client::PIPE_CLIENT_ACCESS,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
        )
        .unwrap();
        // SAFETY: owned listening instance; the client has already connected.
        let connected = unsafe { ConnectNamedPipe(server.0, None) };
        assert!(
            connected.is_ok() || connected.unwrap_err().code() == ERROR_PIPE_CONNECTED.to_hresult()
        );
        (server, pipe)
    }

    /// Writes raw bytes as the helper would, so a test can send a truncated frame.
    fn helper_write(server: &Server, bytes: &[u8]) {
        let mut written = 0;
        // SAFETY: owned server handle; buffer and count outlive the call.
        unsafe { WriteFile(server.0, Some(bytes), Some(&mut written), None) }.unwrap();
        assert_eq!(written as usize, bytes.len());
    }

    fn welcome(pid: u32) -> Message {
        Message::Welcome {
            version: protocol::PROTOCOL_MAX,
            min: protocol::PROTOCOL_MIN,
            max: protocol::PROTOCOL_MAX,
            pid,
        }
    }

    #[test]
    fn handshake_accepts_a_matching_welcome() {
        let (server, mut pipe) = private_pipe("welcome");
        helper_write(&server, &protocol::encode(&welcome(4321)).unwrap());
        handshake(
            &mut pipe,
            Role::Host,
            4321,
            Instant::now() + HANDSHAKE_TIMEOUT,
        )
        .unwrap();
    }

    #[test]
    fn silent_helper_fails_the_handshake_on_its_deadline() {
        let (_server, mut pipe) = private_pipe("silent");
        let started = Instant::now();
        let error = handshake(
            &mut pipe,
            Role::Host,
            4321,
            started + Duration::from_millis(150),
        )
        .unwrap_err();
        assert_eq!(error, LinkError::Protocol);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn truncated_welcome_fails_the_handshake_on_its_deadline() {
        let (server, mut pipe) = private_pipe("truncated-handshake");
        let frame = protocol::encode(&welcome(4321)).unwrap();
        helper_write(&server, &frame[..protocol::HEADER_LEN + 3]);
        let started = Instant::now();
        let error = handshake(
            &mut pipe,
            Role::Host,
            4321,
            started + Duration::from_millis(150),
        )
        .unwrap_err();
        assert_eq!(error, LinkError::Protocol);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn truncated_frame_stays_queued_until_its_tail_arrives() {
        let (server, mut pipe) = private_pipe("truncated-poll");
        let frame = protocol::encode(&welcome(4321)).unwrap();
        helper_write(&server, &frame[..protocol::HEADER_LEN + 3]);
        let started = Instant::now();
        assert_eq!(poll_message(&mut pipe).unwrap(), None);
        assert!(started.elapsed() < Duration::from_millis(100));
        helper_write(&server, &frame[protocol::HEADER_LEN + 3..]);
        assert_eq!(poll_message(&mut pipe).unwrap(), Some(welcome(4321)));
    }

    #[test]
    fn peer_that_stops_reading_times_out_writes() {
        let (_server, mut pipe) = private_pipe("write-block");
        let message = Message::Mapping {
            name: "x".repeat(protocol::MAX_PAYLOAD - 64),
        };
        let started = Instant::now();
        let mut error = None;
        for _ in 0..64 {
            if let Err(e) = send(
                &mut pipe,
                &message,
                Instant::now() + Duration::from_millis(100),
            ) {
                error = Some(e);
                break;
            }
        }
        let error = error.expect("writes to a peer that never reads must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        // A timed-out operation poisons the connection; the caller must reconnect.
        assert!(pipe.file().is_err());
    }
}
