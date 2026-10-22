//! A separate process emits fixed synthetic arrival records into a page-backed
//! mapping. No heap allocation, Rust channel, or decoder-process mutex occurs
//! in its timed loop. Published records are immutable and consumed in order.
//! Ship arrival-producer.exe beside decode-latency.exe. The companion compiles
//! this module and timing.rs without decoder/D3D modules, keeping startup lean.
use crate::timing::{
    configure_process_timing, frequency, qpc, ArrivalPriority, Sleeper, TimerResolution,
};
use anyhow::{anyhow, bail, Context, Result};
use std::{
    cell::Cell,
    fs::{self, File},
    mem::{align_of, size_of},
    os::windows::{io::AsRawHandle, process::CommandExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    ptr,
    sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering},
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{
            CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE,
            WAIT_FAILED, WAIT_OBJECT_0,
        },
        System::{
            Memory::*,
            Threading::{OpenProcess, WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_SYNCHRONIZE},
        },
    },
};

const MAGIC: u64 = 0x4250_4152_5249_5633;
const VERSION: u32 = 3;
const MAX_COUNT: usize = 60 * 20;
const INIT: u32 = 0;
const READY: u32 = 1;
const RUNNING: u32 = 2;
const DONE: u32 = 3;
const CANCELLED: u32 = 4;
const FAILED: u32 = 5;
static NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
pub struct Arrival {
    pub index: usize,
    pub scheduled: i64,
    pub received: i64,
}

// Both executables compile this same protocol module for the same architecture.
// Only atomic controls mutate
// after initialization; records are written once before Release publication.
#[repr(C, align(64))]
struct Header {
    magic: u64,
    version: u32,
    capacity: u32,
    freq: i64,
    proposed_epoch: i64,
    actual_epoch: AtomicI64,
    published: AtomicU32,
    state: AtomicU32,
    start: AtomicU32,
    cancel: AtomicU32,
    error_stage: AtomicU32,
    parent_pid: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Record {
    index: u32,
    reserved: u32,
    scheduled: i64,
    received: i64,
}

fn mapping_size(count: usize) -> Result<usize> {
    if count == 0 || count > MAX_COUNT {
        bail!("Arrival count must be 1..={MAX_COUNT} (at most 20 seconds)");
    }
    size_of::<Header>()
        .checked_add(
            count
                .checked_mul(size_of::<Record>())
                .context("record size overflow")?,
        )
        .context("mapping size overflow")
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

struct Mapping {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    bytes: usize,
}
impl Mapping {
    unsafe fn map(handle: HANDLE, bytes: usize) -> Result<Self> {
        let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, bytes);
        if view.Value.is_null() {
            let error = windows::core::Error::from_win32();
            let _ = CloseHandle(handle);
            return Err(error).context("MapViewOfFile arrival metadata");
        }
        if view.Value as usize % align_of::<Header>() != 0 {
            let _ = UnmapViewOfFile(view);
            let _ = CloseHandle(handle);
            bail!("Arrival mapping is not header-aligned");
        }
        Ok(Self {
            handle,
            view,
            bytes,
        })
    }
    unsafe fn create(name: &str, bytes: usize) -> Result<Self> {
        let name = wide(name);
        let handle = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            bytes.try_into()?,
            PCWSTR(name.as_ptr()),
        )?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            bail!("Unique arrival mapping name unexpectedly already exists");
        }
        Self::map(handle, bytes)
    }
    unsafe fn open(name: &str, bytes: usize) -> Result<Self> {
        let name = wide(name);
        Self::map(
            OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(name.as_ptr()))?,
            bytes,
        )
    }
    fn header(&self) -> &Header {
        unsafe { &*self.view.Value.cast::<Header>() }
    }
    unsafe fn record(&self, index: usize) -> *mut Record {
        self.view
            .Value
            .cast::<u8>()
            .add(size_of::<Header>())
            .cast::<Record>()
            .add(index)
    }
    unsafe fn prefault(&self) {
        // Resolve this process's page mappings before READY. The parent already
        // initialized these few pages; no artificial load is introduced.
        for offset in (0..self.bytes).step_by(4096) {
            std::hint::black_box(ptr::read_volatile(self.view.Value.cast::<u8>().add(offset)));
        }
        std::hint::black_box(ptr::read_volatile(
            self.view.Value.cast::<u8>().add(self.bytes - 1),
        ));
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}
struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub struct PacedInput {
    mapping: Mapping,
    child: Option<Child>,
    cursor: Cell<u32>,
    epoch: i64,
    error_log: PathBuf,
    finished: Option<usize>,
}
impl PacedInput {
    pub fn new(count: usize, proposed_epoch: i64, freq: i64) -> Result<Self> {
        let bytes = mapping_size(count)?;
        if freq <= 0 || freq != frequency() {
            bail!("Arrival QPC frequency mismatch/invalid");
        }
        let producer_exe = std::env::current_exe()
            .context("current spike executable")?
            .with_file_name("arrival-producer.exe");
        if !producer_exe.is_file() {
            bail!(
                "Arrival companion executable missing: {}",
                producer_exe.display()
            );
        }
        let identity = format!(
            "{}-{}-{}",
            std::process::id(),
            qpc(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        );
        let name = format!("Local\\BubblePlayDecodeArrivalV3-{identity}");
        let mapping = unsafe {
            let mapping = Mapping::create(&name, bytes)?;
            ptr::write_bytes(mapping.view.Value.cast::<u8>(), 0, bytes);
            ptr::write(
                mapping.view.Value.cast::<Header>(),
                Header {
                    magic: MAGIC,
                    version: VERSION,
                    capacity: count as u32,
                    freq,
                    proposed_epoch,
                    actual_epoch: AtomicI64::new(0),
                    published: AtomicU32::new(0),
                    state: AtomicU32::new(INIT),
                    start: AtomicU32::new(0),
                    cancel: AtomicU32::new(0),
                    error_stage: AtomicU32::new(0),
                    parent_pid: std::process::id(),
                },
            );
            mapping
        };
        let directory = PathBuf::from("F:/build/spike-results");
        fs::create_dir_all(&directory)?;
        let error_log = directory.join(format!("arrival-producer-{identity}.log"));
        let stderr = File::create(&error_log)?;
        let child = Command::new(&producer_exe)
            .args([
                "--arrival-producer",
                "--arrival-map",
                &name,
                "--arrival-bytes",
                &bytes.to_string(),
            ])
            .creation_flags(CREATE_NO_WINDOW.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            // No job breakaway, detached launch, or global environment changes.
            .spawn()
            .context("spawn independent arrival producer")?;
        let mut input = Self {
            mapping,
            child: Some(child),
            cursor: Cell::new(0),
            epoch: 0,
            error_log,
            finished: None,
        };
        let setup_deadline = qpc()
            .checked_add(freq.checked_mul(15).context("setup QPC overflow")?)
            .context("setup QPC overflow")?;
        let sleeper = Sleeper::new()?;
        loop {
            let state = input.mapping.header().state.load(Ordering::Acquire);
            if state == READY {
                break;
            }
            input.check_child()?;
            if state == FAILED {
                return Err(input.failure("producer initialization failed"));
            }
            if qpc() >= setup_deadline {
                bail!(
                    "Arrival producer READY timed out after 15 seconds; stderr={}",
                    input.error_log.display()
                );
            }
            sleeper.sleep_us(1000)?;
        }
        // Establish a real future epoch only after DLL/QoS/MMCSS/timer setup.
        // Received timestamps are never rewritten or shifted.
        input.epoch =
            proposed_epoch.max(qpc().checked_add(freq / 10).context("start QPC overflow")?);
        input
            .mapping
            .header()
            .actual_epoch
            .store(input.epoch, Ordering::Release);
        input.mapping.header().start.store(1, Ordering::Release);
        Ok(input)
    }
    pub fn epoch(&self) -> i64 {
        self.epoch
    }
    fn failure(&self, reason: &str) -> anyhow::Error {
        anyhow!(
            "{reason}; stage={}; published={}/{}; stderr={}",
            self.mapping.header().error_stage.load(Ordering::Acquire),
            self.mapping.header().published.load(Ordering::Acquire),
            self.mapping.header().capacity,
            self.error_log.display()
        )
    }
    fn check_child(&self) -> Result<()> {
        if let Some(child) = &self.child {
            let wait = unsafe { WaitForSingleObject(HANDLE(child.as_raw_handle()), 0) };
            if wait == WAIT_OBJECT_0 {
                let state = self.mapping.header().state.load(Ordering::Acquire);
                if state != DONE && state != CANCELLED {
                    return Err(
                        self.failure("Arrival child exited without successful terminal state")
                    );
                }
            } else if wait == WAIT_FAILED {
                bail!(
                    "Arrival child wait failed: {}",
                    windows::core::Error::from_win32()
                );
            }
        }
        Ok(())
    }
    pub fn next(&self) -> Result<Option<Arrival>> {
        let header = self.mapping.header();
        // State first: if DONE is acquired, the later published load includes
        // its final count. This avoids a stale-count/premature-EOF race.
        let state = header.state.load(Ordering::Acquire);
        let count = header.published.load(Ordering::Acquire);
        if count > header.capacity {
            return Err(self.failure("Arrival published count exceeds capacity"));
        }
        let index = self.cursor.get();
        if index < count {
            let record = unsafe { ptr::read(self.mapping.record(index as usize)) };
            let expected = self
                .epoch
                .checked_add(index as i64 * header.freq / 60)
                .context("arrival schedule overflow")?;
            if record.index != index
                || record.reserved != 0
                || record.scheduled != expected
                || record.received < expected
            {
                bail!("Malformed arrival record at index {index}");
            }
            self.cursor.set(index + 1);
            return Ok(Some(Arrival {
                index: index as usize,
                scheduled: record.scheduled,
                received: record.received,
            }));
        }
        if state == FAILED {
            return Err(self.failure("Arrival producer failed"));
        }
        self.check_child()?;
        if index < header.capacity && (state == DONE || state == CANCELLED) {
            return Err(self.failure("Arrival producer ended before all AUs were available"));
        }
        Ok(None)
    }
    pub fn finish(&mut self) -> Result<usize> {
        if let Some(count) = self.finished {
            if self.mapping.header().state.load(Ordering::Acquire) == FAILED {
                return Err(self.failure("Arrival child previously failed"));
            }
            return Ok(count);
        }
        self.mapping.header().cancel.store(1, Ordering::Release);
        if let Some(child) = self.child.as_mut() {
            let handle = HANDLE(child.as_raw_handle());
            // Cleanup is outside timing. Allow bounded OS/MMCSS teardown under
            // memory pressure; a forced termination still fails the run.
            if unsafe { WaitForSingleObject(handle, 20000) } != WAIT_OBJECT_0 {
                // Target only the Child created by this object, never a name.
                child
                    .kill()
                    .context("terminate own unresponsive arrival child")?;
                if unsafe { WaitForSingleObject(handle, 3000) } != WAIT_OBJECT_0 {
                    bail!("Arrival child failed to terminate within cleanup deadline");
                }
                let _ = child.wait();
                self.child.take();
                // The child has exited. Preserve cleanup failure across repeat
                // finish calls rather than turning its cached count into success.
                self.mapping
                    .header()
                    .error_stage
                    .store(99, Ordering::Release);
                self.mapping.header().state.store(FAILED, Ordering::Release);
                self.finished =
                    Some(self.mapping.header().published.load(Ordering::Acquire) as usize);
                bail!(
                    "Arrival child required forced termination; stderr={}",
                    self.error_log.display()
                );
            }
            let status = child.wait().context("reap arrival child")?;
            self.child.take();
            let count = self.mapping.header().published.load(Ordering::Acquire) as usize;
            self.finished = Some(count);
            let terminal = self.mapping.header().state.load(Ordering::Acquire);
            if !status.success() || terminal == FAILED {
                if terminal != FAILED {
                    self.mapping
                        .header()
                        .error_stage
                        .store(98, Ordering::Release);
                    self.mapping.header().state.store(FAILED, Ordering::Release);
                }
                return Err(self.failure("Arrival child failed during run"));
            }
            if terminal != DONE && terminal != CANCELLED {
                self.mapping
                    .header()
                    .error_stage
                    .store(98, Ordering::Release);
                self.mapping.header().state.store(FAILED, Ordering::Release);
                return Err(self.failure("Arrival child exited with unexpected terminal state"));
            }
            if count > self.mapping.header().capacity as usize {
                self.mapping
                    .header()
                    .error_stage
                    .store(97, Ordering::Release);
                self.mapping.header().state.store(FAILED, Ordering::Release);
                return Err(self.failure("Invalid final published count"));
            }
            return Ok(count);
        }
        Ok(self.mapping.header().published.load(Ordering::Acquire) as usize)
    }
}
impl Drop for PacedInput {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn argument<'a>(args: &'a [String], name: &str) -> Result<&'a str> {
    args.windows(2)
        .find(|a| a[0] == name)
        .map(|a| a[1].as_str())
        .with_context(|| format!("missing arrival child argument {name}"))
}
/// Dispatch before COM/MF, D3D/window initialization or normal argument parsing.
pub fn maybe_run_producer(args: &[String]) -> Result<bool> {
    if !args.iter().any(|a| a == "--arrival-producer") {
        return Ok(false);
    }
    let name = argument(args, "--arrival-map")?;
    if !name.starts_with("Local\\BubblePlayDecodeArrivalV3-") {
        bail!("Invalid arrival mapping namespace");
    }
    let bytes: usize = argument(args, "--arrival-bytes")?.parse()?;
    if bytes < size_of::<Header>() || bytes > mapping_size(MAX_COUNT)? {
        bail!("Invalid arrival mapping size");
    }
    let mapping = unsafe { Mapping::open(name, bytes)? };
    let header = mapping.header();
    if header.magic != MAGIC
        || header.version != VERSION
        || mapping_size(header.capacity as usize)? != bytes
        || header.freq <= 0
        || header.freq != frequency()
    {
        bail!("Arrival mapping header/version/frequency mismatch");
    }
    let mut stage = 1;
    let result = (|| -> Result<()> {
        configure_process_timing().context("arrival child process timing policy")?;
        stage = 2;
        let _resolution = TimerResolution::new().context("arrival child 1ms timer period")?;
        let _priority = ArrivalPriority::new().context("arrival child MMCSS Pro Audio CRITICAL")?;
        stage = 3;
        let sleeper = Sleeper::new()?;
        let parent =
            OwnedHandle(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, header.parent_pid)? });
        unsafe {
            mapping.prefault();
        }
        header.state.store(READY, Ordering::Release);
        let setup_deadline = qpc()
            .checked_add(
                header
                    .freq
                    .checked_mul(5)
                    .context("setup deadline overflow")?,
            )
            .context("setup deadline overflow")?;
        while header.start.load(Ordering::Acquire) == 0 {
            if header.cancel.load(Ordering::Acquire) != 0
                || unsafe { WaitForSingleObject(parent.0, 0) } == WAIT_OBJECT_0
            {
                header.state.store(CANCELLED, Ordering::Release);
                return Ok(());
            }
            if qpc() >= setup_deadline {
                bail!("Parent did not publish START within five seconds");
            }
            sleeper.sleep_us(1000)?;
        }
        let epoch = header.actual_epoch.load(Ordering::Acquire);
        let deadline = epoch
            .checked_add(
                header
                    .freq
                    .checked_mul(20)
                    .context("producer deadline overflow")?,
            )
            .context("producer deadline overflow")?;
        if epoch <= 0 || epoch < header.proposed_epoch {
            bail!("Invalid actual arrival epoch");
        }
        header.state.store(RUNNING, Ordering::Release);
        stage = 4;
        for index in 0..header.capacity {
            let scheduled = epoch
                .checked_add(index as i64 * header.freq / 60)
                .context("producer schedule overflow")?;
            loop {
                if header.cancel.load(Ordering::Acquire) != 0
                    || unsafe { WaitForSingleObject(parent.0, 0) } == WAIT_OBJECT_0
                {
                    header.state.store(CANCELLED, Ordering::Release);
                    return Ok(());
                }
                let now = qpc();
                if now >= deadline {
                    bail!("Arrival producer exceeded its 20-second stream deadline");
                }
                let remaining = scheduled - now;
                if remaining <= 0 {
                    break;
                }
                // High-resolution kernel waits only, with cancellation checks
                // at most 50ms apart. No hot spin or heap/channel operation.
                sleeper.sleep_us(
                    (remaining as u64 * 1_000_000 / header.freq as u64).clamp(1, 50_000),
                )?;
            }
            let received = qpc();
            unsafe {
                ptr::write(
                    mapping.record(index as usize),
                    Record {
                        index,
                        reserved: 0,
                        scheduled,
                        received,
                    },
                );
            }
            header.published.store(index + 1, Ordering::Release);
        }
        header.state.store(DONE, Ordering::Release);
        Ok(())
    })();
    if let Err(error) = result {
        header.error_stage.store(stage, Ordering::Release);
        header.state.store(FAILED, Ordering::Release);
        return Err(error);
    }
    Ok(true)
}
