//! Copy-only OLE virtual files. The file socket owns writes; this STA owns COM.
//! Descriptors are available immediately, but streams never expose unverified
//! bytes. A private bounded cache lives until the last data object/stream dies.
use crate::file_transfer::{self as ft, DropPoint, OfferMeta};
use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::mem::{size_of, ManuallyDrop};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows::core::{implement, Ref, BOOL, HRESULT};
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::System::Com::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::*;
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_MOVE_NOCOALESCE, MOUSEINPUT, VK_LBUTTON,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

mod input_monitor;
mod preview;
pub use input_monitor::{NativeDropInputGuard, NativeDropInputStatus};
pub use preview::NativeDropPreview;

/// Aggregate batches at this size are fully verified before the native drag.
pub const PREPARE_NATIVE_DROP_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TargetWindow {
    handle: usize,
    process: u32,
    thread: u32,
    bounds: [i32; 4],
}
impl TargetWindow {
    unsafe fn capture(window: HWND) -> Result<Self> {
        if window.0.is_null()
            || !IsWindow(Some(window)).as_bool()
            || !IsWindowVisible(window).as_bool()
        {
            bail!("native target window unavailable");
        }
        let mut process = 0;
        let thread = GetWindowThreadProcessId(window, Some(&mut process));
        let mut rect = RECT::default();
        GetWindowRect(window, &mut rect).context("read native target bounds")?;
        if process == 0 || thread == 0 {
            bail!("native target identity unavailable");
        }
        Ok(Self {
            handle: window.0 as usize,
            process,
            thread,
            bounds: [rect.left, rect.top, rect.right, rect.bottom],
        })
    }
}
/// Native identity at release. This cannot identify a browser DOM node; main
/// also invalidates pending delivery after meaningful intervening user input.
#[derive(Clone, Copy, Debug)]
pub struct NativeDropTarget {
    point: POINT,
    leaf: TargetWindow,
    root: TargetWindow,
    foreground: TargetWindow,
}
impl NativeDropTarget {
    pub fn capture(point: POINT) -> Result<Self> {
        if !crate::clipboard::input_desktop_is_default() {
            bail!("input desktop unavailable");
        }
        unsafe {
            let leaf = WindowFromPoint(point);
            Ok(Self {
                point,
                leaf: TargetWindow::capture(leaf)?,
                root: TargetWindow::capture(GetAncestor(leaf, GA_ROOT))?,
                foreground: TargetWindow::capture(GetForegroundWindow())?,
            })
        }
    }
    pub fn matches_current(&self) -> bool {
        Self::capture(self.point).is_ok_and(|now| self.same_binding(&now))
    }
    fn same_binding(&self, now: &Self) -> bool {
        self.leaf == now.leaf && self.root == now.root && self.foreground == now.foreground
    }
    pub fn point(&self) -> POINT {
        self.point
    }
}

const WAIT_LIMIT: Duration = Duration::from_secs(120);
const OVERALL_LIMIT: Duration = Duration::from_secs(60 * 60);
const RETAIN_LIMIT: Duration = Duration::from_secs(5 * 60);
const POST_DROP_NEGOTIATION_LIMIT: Duration = Duration::from_secs(10);
const MAX_RETAINED_BATCHES: usize = 16;
static NEXT_CACHE: AtomicUsize = AtomicUsize::new(1);
static DRAG_ACTIVE: AtomicBool = AtomicBool::new(false);
static CACHE_BUDGET: CacheBudget = CacheBudget::new();

struct CacheBudget {
    bytes: AtomicU64,
    batches: AtomicUsize,
}
impl CacheBudget {
    const fn new() -> Self {
        Self {
            bytes: AtomicU64::new(0),
            batches: AtomicUsize::new(0),
        }
    }
    fn reserve(&self, bytes: u64) -> Result<()> {
        // A zero-byte batch still owns a cache and may retain its COM STA.
        self.batches
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < MAX_RETAINED_BATCHES).then_some(current + 1)
            })
            .map_err(|_| anyhow::anyhow!("retained native drop batch limit reached"))?;
        if self
            .bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(bytes)
                    .filter(|total| *total <= ft::MAX_FILE_BYTES)
            })
            .is_err()
        {
            self.batches.fetch_sub(1, Ordering::AcqRel);
            bail!("retained native drop cache is full");
        }
        Ok(())
    }
    fn release(&self, bytes: u64) {
        self.bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.batches.fetch_sub(1, Ordering::AcqRel);
    }
}

struct CacheReservation {
    bytes: u64,
    released: AtomicBool,
}
impl CacheReservation {
    fn new(bytes: u64) -> Result<Self> {
        CACHE_BUDGET.reserve(bytes)?;
        Ok(Self {
            bytes,
            released: AtomicBool::new(false),
        })
    }
    fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            CACHE_BUDGET.release(self.bytes);
        }
    }
}
impl Drop for CacheReservation {
    fn drop(&mut self) {
        self.release();
    }
}

/// True during cursor placement and OLE negotiation, until target commitment. The main loop uses
/// this to keep queued remote pointer events from changing the destination.
pub fn drag_active() -> bool {
    DRAG_ACTIVE.load(Ordering::Acquire)
}
struct DragGuard {
    owned: AtomicBool,
}
fn release_owned_gate(owned: &AtomicBool, gate: &AtomicBool) {
    if owned.swap(false, Ordering::AcqRel) {
        gate.store(false, Ordering::Release);
    }
}
impl DragGuard {
    fn acquire() -> Result<Self> {
        if DRAG_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            bail!("native drag already active");
        }
        Ok(Self {
            owned: AtomicBool::new(true),
        })
    }
    fn release(&self) {
        release_owned_gate(&self.owned, &DRAG_ACTIVE);
    }
}
impl Drop for DragGuard {
    fn drop(&mut self) {
        self.release();
    }
}

struct InputQueueGuard {
    native: u32,
    foreground: u32,
    attached: Mutex<bool>,
}
impl InputQueueGuard {
    unsafe fn attach() -> Result<Self> {
        // AttachThreadInput resets queue key state. Do not attach while any
        // physical or remotely injected key/button is held; preserve that work.
        if (1..=255).any(|key| GetAsyncKeyState(key) < 0) {
            bail!("held input prevents native queue attachment");
        }
        let native = windows::Win32::System::Threading::GetCurrentThreadId();
        let window = GetForegroundWindow();
        let foreground = GetWindowThreadProcessId(window, None);
        if window.0.is_null() || foreground == 0 {
            bail!("foreground input queue unavailable");
        }
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        let attached = native != foreground;
        if attached
            && !windows::Win32::System::Threading::AttachThreadInput(native, foreground, true)
                .as_bool()
        {
            return Err(windows::core::Error::from_thread())
                .context("attach native OLE foreground input queue");
        }
        let guard = Self {
            native,
            foreground,
            attached: Mutex::new(attached),
        };
        if (1..=255).any(|key| GetAsyncKeyState(key) < 0) {
            bail!("input changed during native queue attachment");
        }
        tracing::info!(target = ?window, native, foreground, attached, "native OLE foreground input queue attached");
        Ok(guard)
    }
    fn detach(&self) -> bool {
        let mut attached = self.attached.lock().unwrap();
        if *attached {
            let detached = unsafe {
                windows::Win32::System::Threading::AttachThreadInput(
                    self.native,
                    self.foreground,
                    false,
                )
            }
            .as_bool();
            if !detached {
                tracing::warn!("native OLE foreground input queue detach failed");
                return false;
            }
            *attached = false;
            tracing::info!("native OLE foreground input queue detached");
        }
        true
    }
}
impl Drop for InputQueueGuard {
    fn drop(&mut self) {
        self.detach();
    }
}
struct DropRouting {
    queue: InputQueueGuard,
    drag: DragGuard,
}
impl DropRouting {
    fn release(&self) -> bool {
        if !self.queue.detach() {
            return false;
        }
        self.drag.release();
        true
    }
}

#[derive(Default)]
struct State {
    verified: Vec<bool>,
    error: Option<String>,
    disposition: Option<bool>,
    operation: bool,
    async_mode: bool,
    started: bool,
    last_progress: Option<Instant>,
    content_requested: bool,
}

fn progress_remaining(created: Instant, last_progress: Instant, now: Instant) -> Duration {
    WAIT_LIMIT
        .saturating_sub(now.saturating_duration_since(last_progress))
        .min(OVERALL_LIMIT.saturating_sub(now.saturating_duration_since(created)))
}
struct Inner {
    metas: Vec<OfferMeta>,
    paths: Vec<PathBuf>,
    cache: PathBuf,
    state: Mutex<State>,
    changed: Condvar,
    live_objects: AtomicUsize,
    created: Instant,
    reservation: CacheReservation,
}
impl Drop for Inner {
    fn drop(&mut self) {
        // Every path was created inside this unique cache; never a client path.
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_dir(&self.cache);
    }
}

#[derive(Clone)]
pub(crate) struct DropBatch(Arc<Inner>);
impl DropBatch {
    pub(crate) fn new(metas: Vec<OfferMeta>) -> Result<Self> {
        if metas.is_empty() || metas.len() > ft::MAX_BATCH_FILES as usize {
            bail!("invalid drop count");
        }
        let total: u64 = metas.iter().map(|m| m.size).sum();
        if total > ft::MAX_FILE_BYTES {
            bail!("drop cache exceeds 2 GiB");
        }
        let reservation = CacheReservation::new(total)?;
        let cache = std::env::temp_dir().join(format!(
            "bubbleplay-drop-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            NEXT_CACHE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&cache).context("create private drop cache")?;
        let paths = (0..metas.len())
            .map(|i| cache.join(format!("{i}.bpcache")))
            .collect();
        Ok(Self(Arc::new(Inner {
            state: Mutex::new(State {
                verified: vec![false; metas.len()],
                async_mode: true,
                ..State::default()
            }),
            metas,
            paths,
            cache,
            changed: Condvar::new(),
            live_objects: AtomicUsize::new(0),
            created: Instant::now(),
            reservation,
        })))
    }
    pub(crate) fn path(&self, index: usize) -> PathBuf {
        self.0.paths[index].clone()
    }
    pub(crate) fn verified(&self, index: usize) {
        let mut state = self.0.state.lock().unwrap();
        state.verified[index] = true;
        state.last_progress = Some(Instant::now());
        drop(state);
        self.0.changed.notify_all();
    }
    pub(crate) fn progress(&self) {
        self.0.state.lock().unwrap().last_progress = Some(Instant::now());
        self.0.changed.notify_all();
    }
    fn remaining(&self, state: &State) -> Duration {
        progress_remaining(
            self.0.created,
            state.last_progress.unwrap_or(self.0.created),
            Instant::now(),
        )
    }
    pub(crate) fn cancel(&self, reason: &str) {
        let mut s = self.0.state.lock().unwrap();
        if s.error.is_none() {
            s.error = Some(reason.to_owned());
        }
        self.0.changed.notify_all();
    }
    pub(crate) fn result(&self) -> Option<std::result::Result<(), String>> {
        let s = self.0.state.lock().unwrap();
        if let Some(error) = &s.error {
            return Some(Err(error.clone()));
        }
        if self.remaining(&s).is_zero() {
            return Some(Err("drop-timeout".into()));
        }
        match s.disposition {
            Some(false) => Some(Err("drop-rejected".into())),
            Some(true) if !s.operation && s.verified.iter().all(|v| *v) => Some(Ok(())),
            _ => None,
        }
    }
    #[cfg(test)]
    pub(crate) fn accepted_for_test(&self) {
        self.0.state.lock().unwrap().disposition = Some(true);
    }
    fn wait_verified(&self, index: usize) -> windows::core::Result<()> {
        let mut state = self.0.state.lock().unwrap();
        loop {
            if state.error.is_some() {
                return Err(E_ABORT.into());
            }
            if state.verified[index] {
                return Ok(());
            }
            let remaining = self.remaining(&state);
            if remaining.is_zero() {
                return Err(E_ABORT.into());
            }
            state = self.0.changed.wait_timeout(state, remaining).unwrap().0;
        }
    }
}

/// Main-loop authorization: map `point` against the active captured desktop,
/// check the interactive session, then start. Never run COM on the main loop.
pub struct NativeDropRequest {
    pub point: DropPoint,
    pub epoch: String,
    batch: DropBatch,
}
impl NativeDropRequest {
    pub(crate) fn new(batch: DropBatch, point: DropPoint, epoch: String) -> Self {
        Self {
            point,
            epoch,
            batch,
        }
    }
    pub fn reject(self, reason: &str) {
        self.batch.cancel(reason);
    }
    pub fn total_bytes(&self) -> u64 {
        self.batch.0.metas.iter().map(|meta| meta.size).sum()
    }
    pub fn preparation_required(&self) -> bool {
        self.total_bytes() >= PREPARE_NATIVE_DROP_BYTES
    }
    /// Nonblocking readiness: errors/idle limits are preserved, and every large
    /// batch slot must pass size/hash/flush verification before native startup.
    pub fn preparation_status(&self) -> std::result::Result<bool, String> {
        let state = self.batch.0.state.lock().unwrap();
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        if self.batch.remaining(&state).is_zero() {
            return Err("drop-timeout".into());
        }
        Ok(!self.preparation_required() || state.verified.iter().all(|verified| *verified))
    }
    pub fn start(self, point: POINT) -> Result<()> {
        self.start_with_target(point, None)
    }
    pub fn start_prepared(self, target: NativeDropTarget) -> Result<()> {
        if self.preparation_status() != Ok(true) {
            self.batch.cancel("drop-not-ready");
            bail!("native contents not prepared");
        }
        self.start_with_target(target.point(), Some(target))
    }
    fn start_with_target(self, point: POINT, target: Option<NativeDropTarget>) -> Result<()> {
        {
            let mut state = self.batch.0.state.lock().unwrap();
            if state.started || state.error.is_some() {
                bail!("drop already started or cancelled");
            }
            state.started = true;
        }
        let batch = self.batch.clone();
        match std::thread::Builder::new()
            .name("bubbleplay-native-drop".into())
            .spawn(move || {
                if let Err(error) = run_drop(&batch, point, target) {
                    tracing::warn!(error = %error, "native file drop failed");
                    batch.cancel("drop-failed");
                }
            }) {
            Ok(_) => Ok(()),
            Err(error) => {
                self.batch.cancel("drop-unavailable");
                Err(error.into())
            }
        }
    }
}

/// Diagnostic entry point for explicitly owned test fixtures. This follows the
/// same cache, virtual descriptors, COM lifetime, and OLE path as a web drop.
/// Calling it starts a real desktop drop at `point`.
pub fn diagnostic_drop(paths: &[PathBuf], point: POINT) -> Result<NativeDropProbe> {
    let count = paths.len() as u32;
    let mut metas = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() || metadata.len() > ft::MAX_FILE_BYTES {
            bail!("invalid diagnostic file");
        }
        let meta = OfferMeta {
            name: path
                .file_name()
                .context("file name")?
                .to_string_lossy()
                .into_owned(),
            size: metadata.len(),
            mime: "application/octet-stream".into(),
            mtime_ms: metadata
                .modified()
                .ok()
                .and_then(|v| v.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |v| v.as_millis() as u64),
            purpose: ft::Purpose::Drop,
            batch_id: 1,
            batch_index: index as u32,
            batch_count: count,
            drop: Some(DropPoint {
                x: 0,
                y: 0,
                generation: 1,
                viewport_revision: 0,
            }),
        };
        if !ft::valid_offer_meta(&meta) {
            bail!("invalid diagnostic metadata");
        }
        metas.push(meta);
    }
    let batch = DropBatch::new(metas)?;
    for (index, path) in paths.iter().enumerate() {
        fs::copy(path, batch.path(index))?;
        crate::file_service::write_mark_of_the_web(&batch.path(index))?;
        // The probe verifies its private copy independently before marking ready.
        let hash = |path: &PathBuf| -> Result<[u8; 32]> {
            let mut hasher = crate::file_hash::Sha256::new()?;
            let mut file = File::open(path)?;
            let mut buffer = [0; 64 * 1024];
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
            }
            Ok(hasher.finish())
        };
        if hash(path)? != hash(&batch.path(index))? {
            bail!("diagnostic cache hash mismatch");
        }
        batch.verified(index);
    }
    NativeDropRequest::new(
        batch.clone(),
        DropPoint {
            x: 0,
            y: 0,
            generation: 1,
            viewport_revision: 0,
        },
        "diagnostic".into(),
    )
    .start(point)?;
    Ok(NativeDropProbe(batch))
}
pub struct NativeDropProbe(DropBatch);
impl NativeDropProbe {
    pub fn result(&self) -> Option<std::result::Result<(), String>> {
        self.0.result()
    }
    pub fn cancel(&self) {
        self.0.cancel("drop-cancelled");
    }
}

#[derive(Clone, Copy)]
struct Formats {
    descriptors: u16,
    contents: u16,
    preferred: u16,
}
impl Formats {
    fn register() -> Self {
        unsafe {
            tracing::info!("native drop formats registration starting");
            Self {
                descriptors: windows::Win32::System::DataExchange::RegisterClipboardFormatW(
                    CFSTR_FILEDESCRIPTORW,
                ) as u16,
                contents: windows::Win32::System::DataExchange::RegisterClipboardFormatW(
                    CFSTR_FILECONTENTS,
                ) as u16,
                preferred: windows::Win32::System::DataExchange::RegisterClipboardFormatW(
                    CFSTR_PREFERREDDROPEFFECT,
                ) as u16,
            }
        }
    }
    fn list(self) -> Vec<FORMATETC> {
        vec![
            fmt(self.descriptors, -1, TYMED_HGLOBAL),
            fmt(self.contents, -1, TYMED_ISTREAM),
            fmt(self.preferred, -1, TYMED_HGLOBAL),
        ]
    }
}
fn fmt(cf: u16, index: i32, medium: TYMED) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: index,
        tymed: medium.0 as u32,
    }
}
fn hglobal(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let data = GlobalLock(handle);
        if data.is_null() {
            let _ = GlobalFree(Some(handle));
            return Err(E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast(), bytes.len());
        let _ = GlobalUnlock(handle);
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: handle },
            ..STGMEDIUM::default()
        })
    }
}
fn descriptor_bytes(metas: &[OfferMeta]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + metas.len() * size_of::<FILEDESCRIPTORW>());
    bytes.extend_from_slice(&(metas.len() as u32).to_le_bytes());
    let mut names = Vec::new();
    for meta in metas {
        // Existing sanitizer bounds the base to 200 UTF-16 units and removes
        // trailing dots/spaces after extension-preserving truncation. Reserve
        // ample MAX_PATH room for a suffix; test every candidate against literal
        // names as well as generated collision names.
        let safe = ft::sanitize_file_name(&meta.name);
        let mut name = safe.clone();
        let mut suffix = 1;
        while names.iter().any(|v: &String| v.eq_ignore_ascii_case(&name)) {
            name = ft::collision_name(&safe, suffix);
            suffix += 1;
        }
        names.push(name.clone());
        let mut fd = FILEDESCRIPTORW {
            dwFlags: (FD_FILESIZE.0
                | FD_ATTRIBUTES.0
                | FD_WRITESTIME.0
                | FD_UNICODE.0
                | FD_PROGRESSUI.0) as u32,
            dwFileAttributes: FILE_ATTRIBUTE_NORMAL.0,
            nFileSizeHigh: (meta.size >> 32) as u32,
            nFileSizeLow: meta.size as u32,
            ..FILEDESCRIPTORW::default()
        };
        let ticks = meta
            .mtime_ms
            .saturating_mul(10_000)
            .saturating_add(116444736000000000);
        fd.ftLastWriteTime = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        for (i, unit) in name.encode_utf16().take(259).enumerate() {
            fd.cFileName[i] = unit;
        }
        unsafe {
            bytes.extend_from_slice(std::slice::from_raw_parts(
                (&fd as *const FILEDESCRIPTORW).cast::<u8>(),
                size_of::<FILEDESCRIPTORW>(),
            ));
        }
    }
    bytes
}

#[implement(IDataObject, IDataObjectAsyncCapability)]
struct VirtualFiles {
    batch: DropBatch,
    formats: Formats,
}
impl Drop for VirtualFiles {
    fn drop(&mut self) {
        self.batch.0.live_objects.fetch_sub(1, Ordering::AcqRel);
    }
}
impl VirtualFiles {
    fn new(batch: DropBatch, formats: Formats) -> Self {
        batch.0.live_objects.fetch_add(1, Ordering::AcqRel);
        Self { batch, formats }
    }
    fn validate(&self, ptr: *const FORMATETC) -> HRESULT {
        let Some(f) = (unsafe { ptr.as_ref() }) else {
            return E_POINTER;
        };
        if !f.ptd.is_null() {
            return DV_E_DVTARGETDEVICE;
        }
        if f.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_DVASPECT;
        }
        let medium = if f.cfFormat == self.formats.contents {
            if f.lindex < 0 || f.lindex as usize >= self.batch.0.metas.len() {
                return DV_E_LINDEX;
            }
            TYMED_ISTREAM
        } else if [self.formats.descriptors, self.formats.preferred].contains(&f.cfFormat) {
            if f.lindex != -1 {
                return DV_E_LINDEX;
            }
            TYMED_HGLOBAL
        } else {
            return DV_E_FORMATETC;
        };
        if f.tymed & medium.0 as u32 == 0 {
            return DV_E_TYMED;
        }
        S_OK
    }
}
impl IDataObject_Impl for VirtualFiles_Impl {
    fn GetData(&self, ptr: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let format = unsafe { ptr.as_ref() }.map(|f| f.cfFormat).unwrap_or(0);
        let result = (|| {
            self.validate(ptr).ok()?;
            if self.batch.0.state.lock().unwrap().error.is_some() {
                return Err(E_ABORT.into());
            }
            let f = unsafe { &*ptr };
            tracing::info!(
                format = f.cfFormat,
                index = f.lindex,
                "native drop data requested"
            );
            if f.cfFormat == self.formats.descriptors {
                return hglobal(&descriptor_bytes(&self.batch.0.metas));
            }
            if f.cfFormat == self.formats.preferred {
                return hglobal(&DROPEFFECT_COPY.0.to_le_bytes());
            }
            // Cache files retain ZoneId=3 ADS. The IE-specific URL-action policy
            // format is not native-file provenance; arbitrary stream consumers
            // need not propagate the cache's ADS.
            self.batch.0.state.lock().unwrap().content_requested = true;
            let stream: IStream =
                VerifiedStream::new(self.batch.clone(), f.lindex as usize, 0).into();
            Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream)),
                },
                ..STGMEDIUM::default()
            })
        })();
        tracing::info!(
            format,
            hresult = result
                .as_ref()
                .err()
                .map(|e: &windows::core::Error| e.code().0)
                .unwrap_or(S_OK.0),
            "native drop data request completed"
        );
        result
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn QueryGetData(&self, ptr: *const FORMATETC) -> HRESULT {
        self.validate(ptr)
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if !out.is_null() {
            unsafe {
                (*out).ptd = std::ptr::null_mut();
            }
        }
        DATA_S_SAMEFORMATETC
    }
    fn SetData(
        &self,
        _: *const FORMATETC,
        medium: *const STGMEDIUM,
        release: BOOL,
    ) -> windows::core::Result<()> {
        // Shell sends performed effect/private drag-image formats. We never
        // delete a source or interpret move effects. Honor medium ownership.
        if medium.is_null() {
            return Err(E_POINTER.into());
        }
        if release.as_bool() {
            unsafe {
                let mut owned = (*medium).clone();
                ReleaseStgMedium(&mut owned);
            }
        }
        Ok(())
    }
    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        unsafe { SHCreateStdEnumFmtEtc(&self.formats.list()) }
    }
    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}
impl IDataObjectAsyncCapability_Impl for VirtualFiles_Impl {
    fn SetAsyncMode(&self, value: BOOL) -> windows::core::Result<()> {
        self.batch.0.state.lock().unwrap().async_mode = value.as_bool();
        Ok(())
    }
    fn GetAsyncMode(&self) -> windows::core::Result<BOOL> {
        Ok(self.batch.0.state.lock().unwrap().async_mode.into())
    }
    fn StartOperation(&self, _: Ref<IBindCtx>) -> windows::core::Result<()> {
        self.batch.0.state.lock().unwrap().operation = true;
        Ok(())
    }
    fn InOperation(&self) -> windows::core::Result<BOOL> {
        Ok(self.batch.0.state.lock().unwrap().operation.into())
    }
    fn EndOperation(
        &self,
        result: HRESULT,
        _: Ref<IBindCtx>,
        effects: u32,
    ) -> windows::core::Result<()> {
        let mut state = self.batch.0.state.lock().unwrap();
        state.operation = false;
        if (result.is_err() || effects & DROPEFFECT_COPY.0 == 0) && state.error.is_none() {
            state.error = Some("drop-failed".into());
        }
        drop(state);
        self.batch.0.changed.notify_all();
        Ok(())
    }
}

#[implement(IStream)]
struct VerifiedStream {
    batch: DropBatch,
    index: usize,
    position: Mutex<u64>,
}
impl VerifiedStream {
    fn new(batch: DropBatch, index: usize, position: u64) -> Self {
        batch.0.live_objects.fetch_add(1, Ordering::AcqRel);
        Self {
            batch,
            index,
            position: Mutex::new(position),
        }
    }
    fn read(&self, bytes: &mut [u8]) -> windows::core::Result<usize> {
        self.batch.wait_verified(self.index)?;
        let mut position = self.position.lock().unwrap();
        let mut file = File::open(&self.batch.0.paths[self.index])
            .map_err(|_| windows::core::Error::from(E_FAIL))?;
        file.seek(SeekFrom::Start(*position))
            .map_err(|_| windows::core::Error::from(E_FAIL))?;
        let n = file
            .read(bytes)
            .map_err(|_| windows::core::Error::from(E_FAIL))?;
        *position += n as u64;
        if n != 0 {
            self.batch.progress();
        }
        Ok(n)
    }
}
impl Drop for VerifiedStream {
    fn drop(&mut self) {
        self.batch.0.live_objects.fetch_sub(1, Ordering::AcqRel);
    }
}
impl ISequentialStream_Impl for VerifiedStream_Impl {
    fn Read(&self, ptr: *mut std::ffi::c_void, count: u32, read: *mut u32) -> HRESULT {
        if !read.is_null() {
            unsafe {
                *read = 0;
            }
        }
        if count == 0 {
            return S_OK;
        }
        if ptr.is_null() {
            return E_POINTER;
        }
        match self.read(unsafe { std::slice::from_raw_parts_mut(ptr.cast(), count as usize) }) {
            Ok(n) => {
                if !read.is_null() {
                    unsafe {
                        *read = n as u32;
                    }
                }
                if n == count as usize {
                    S_OK
                } else {
                    S_FALSE
                }
            }
            Err(error) => error.code(),
        }
    }
    fn Write(&self, _: *const std::ffi::c_void, _: u32, written: *mut u32) -> HRESULT {
        if !written.is_null() {
            unsafe {
                *written = 0;
            }
        }
        STG_E_ACCESSDENIED
    }
}
impl IStream_Impl for VerifiedStream_Impl {
    fn Seek(&self, offset: i64, origin: STREAM_SEEK, out: *mut u64) -> windows::core::Result<()> {
        let mut p = self.position.lock().unwrap();
        let base = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => *p,
            STREAM_SEEK_END => self.batch.0.metas[self.index].size,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let next = (base as i128) + offset as i128;
        if next < 0 || next > u64::MAX as i128 {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        *p = next as u64;
        if !out.is_null() {
            unsafe {
                *out = *p;
            }
        }
        Ok(())
    }
    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }
    fn CopyTo(
        &self,
        target: Ref<IStream>,
        count: u64,
        read: *mut u64,
        written: *mut u64,
    ) -> windows::core::Result<()> {
        if !read.is_null() {
            unsafe {
                *read = 0;
            }
        }
        if !written.is_null() {
            unsafe {
                *written = 0;
            }
        }
        let target = target
            .as_ref()
            .ok_or_else(|| windows::core::Error::from(E_POINTER))?;
        let mut remaining = count;
        let mut buffer = vec![0u8; ft::MAX_DATA_BYTES];
        while remaining > 0 {
            let take = remaining.min(buffer.len() as u64) as usize;
            let n = self.read(&mut buffer[..take])?;
            if n == 0 {
                break;
            }
            let mut out = 0;
            unsafe {
                target
                    .Write(buffer.as_ptr().cast(), n as u32, Some(&mut out))
                    .ok()?;
            }
            if !read.is_null() {
                unsafe {
                    *read += n as u64;
                }
            }
            if !written.is_null() {
                unsafe {
                    *written += out as u64;
                }
            }
            if out != n as u32 {
                return Err(STG_E_MEDIUMFULL.into());
            }
            remaining -= n as u64;
        }
        Ok(())
    }
    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Ok(())
    }
    fn Revert(&self) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn Stat(&self, out: *mut STATSTG, _: &STATFLAG) -> windows::core::Result<()> {
        if out.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *out = STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: self.batch.0.metas[self.index].size,
                grfMode: STGM_READ,
                ..STATSTG::default()
            };
        }
        Ok(())
    }
    fn Clone(&self) -> windows::core::Result<IStream> {
        Ok(VerifiedStream::new(
            self.batch.clone(),
            self.index,
            *self.position.lock().unwrap(),
        )
        .into())
    }
}

#[implement(IDropSource, IDropSourceNotify)]
struct DropSource {
    batch: DropBatch,
    feedback: AtomicUsize,
    started: Instant,
    queries: AtomicUsize,
    entered: AtomicBool,
    wake: Arc<DragWake>,
}
struct DragWake {
    batch: DropBatch,
    target: POINT,
    prepared_target: Option<NativeDropTarget>,
    routing: Arc<DropRouting>,
    committed_at: Mutex<Option<Instant>>,
    feedback_effect: AtomicUsize,
    release_intent: AtomicBool,
    released: AtomicBool,
    virtual_release_started: AtomicBool,
    warned: AtomicBool,
    started: Instant,
    returned: AtomicBool,
    down_sent: AtomicBool,
    down_owned: AtomicBool,
}
struct WakeGuard {
    wake: Arc<DragWake>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for WakeGuard {
    fn drop(&mut self) {
        self.wake.returned.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if self.queries.fetch_add(1, Ordering::Relaxed) == 0 {
            tracing::info!("native drop initial continuation query");
        }
        if escape.as_bool()
            || self.batch.0.state.lock().unwrap().error.is_some()
            || (self.started.elapsed() > Duration::from_secs(3)
                && !self.wake.virtual_release_started.load(Ordering::Acquire))
        {
            tracing::info!("native drop continuation cancelled");
            return DRAGDROP_S_CANCEL;
        }
        // The renderer may initially report NONE while negotiating. Only our
        // COPY-accepted paired release can commit the drop, and only after OLE
        // observes the actual button-up event. Intent precedes SendInput so an
        // immediate callback cannot race its completion bookkeeping.
        if virtual_drop_ready(self.wake.release_intent.load(Ordering::Acquire), keys) {
            if self
                .wake
                .prepared_target
                .as_ref()
                .is_some_and(|target| !target.matches_current())
            {
                self.batch.cancel("drop-target-changed");
                return DRAGDROP_S_CANCEL;
            }
            // The paired UP committed this target. Detach before allowing new
            // remote input so prompts and synchronous extraction stay usable.
            if !self.wake.routing.release() {
                self.batch.cancel("drop-unavailable");
                return DRAGDROP_S_CANCEL;
            }
            *self.wake.committed_at.lock().unwrap() = Some(Instant::now());
            self.wake
                .virtual_release_started
                .store(true, Ordering::Release);
            tracing::info!("native drop virtual release");
            return DRAGDROP_S_DROP;
        }
        S_OK
    }
    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        let previous = self
            .wake
            .feedback_effect
            .swap(effect.0 as usize, Ordering::AcqRel);
        if self.feedback.fetch_add(1, Ordering::Relaxed) == 0 || previous != effect.0 as usize {
            tracing::info!(effect = effect.0, "native drop feedback changed");
        }
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}
impl IDropSourceNotify_Impl for DropSource_Impl {
    fn DragEnterTarget(&self, hwnd: HWND) -> windows::core::Result<()> {
        self.entered.store(true, Ordering::Release);
        tracing::info!(target = ?hwnd, "native drop entered target");
        Ok(())
    }
    fn DragLeaveTarget(&self) -> windows::core::Result<()> {
        self.entered.store(false, Ordering::Release);
        Ok(())
    }
}
fn accepted_copy(effect: usize) -> bool {
    effect & DROPEFFECT_COPY.0 as usize != 0
}
fn virtual_drop_ready(release_intent: bool, keys: MODIFIERKEYS_FLAGS) -> bool {
    release_intent && keys.0 & MK_LBUTTON.0 == 0
}
fn post_drop_expired(
    content_requested: bool,
    committed: Instant,
    now: Instant,
    remaining: Duration,
) -> bool {
    remaining.is_zero()
        || (!content_requested
            && now.saturating_duration_since(committed) >= POST_DROP_NEGOTIATION_LIMIT)
}
unsafe fn wake_drag(wake: &DragWake, native_thread: u32) {
    // Once QueryContinueDrag committed the release, Drop can synchronously
    // extract a large stream while pumping messages. Entry-time wakes/timeouts
    // must never interrupt that healthy extraction; its idle policy owns it.
    if wake.virtual_release_started.load(Ordering::Acquire) {
        return;
    }
    let mut gui = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..GUITHREADINFO::default()
    };
    let _ = GetGUIThreadInfo(native_thread, &mut gui);
    let capture = gui.hwndCapture;
    let mut process = 0;
    let thread = GetWindowThreadProcessId(capture, Some(&mut process));
    let owned = !capture.0.is_null() && thread == native_thread && process == std::process::id();
    if !owned {
        if !wake.warned.swap(true, Ordering::AcqRel) {
            tracing::warn!(target = ?capture, thread, process, "native OLE continuation has no owned capture");
        }
    }
    if owned && !wake.down_sent.load(Ordering::Acquire) {
        let mut rect = RECT::default();
        let _ = GetWindowRect(capture, &mut rect);
        let mut class = [0u16; 128];
        let count = GetClassNameW(capture, &mut class);
        tracing::info!(target = ?capture, thread, process, visible = IsWindowVisible(capture).as_bool(),
            class = %String::from_utf16_lossy(&class[..count.max(0) as usize]), rect = ?rect,
            foreground = ?GetForegroundWindow(), "native OLE capture routing verified");
    }
    let mut cancelled = wake.started.elapsed() > Duration::from_secs(3);
    if cancelled {
        wake.batch.cancel("drop-timeout");
    }
    cancelled |= wake.batch.0.state.lock().unwrap().error.is_some();
    if owned && !wake.down_sent.load(Ordering::Acquire) && !cancelled {
        if GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 {
            wake.batch.cancel("drop-input-busy");
            let _ = PostThreadMessageW(native_thread, WM_QUIT, WPARAM(0), LPARAM(0));
            return;
        }
        // Bootstrap only inside this operation's captured OLE window, after
        // it exists. The following UP is always paired, including on abort.
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dwFlags: MOUSEEVENTF_LEFTDOWN,
                    ..MOUSEINPUT::default()
                },
            },
        };
        let sent = SendInput(&[input], size_of::<INPUT>() as i32);
        if sent == 1 {
            wake.down_sent.store(true, Ordering::Release);
            wake.down_owned.store(true, Ordering::Release);
        }
        tracing::info!(target = ?capture, sent, "native OLE captured drag bootstrap sent");
        return;
    }
    if owned
        && wake.down_owned.load(Ordering::Acquire)
        && !cancelled
        && accepted_copy(wake.feedback_effect.load(Ordering::Acquire))
    {
        if wake
            .prepared_target
            .as_ref()
            .is_some_and(|target| !target.matches_current())
        {
            wake.batch.cancel("drop-target-changed");
            cancelled = true;
        }
        let mut cursor = POINT::default();
        if GetCursorPos(&mut cursor).is_err()
            || cursor.x != wake.target.x
            || cursor.y != wake.target.y
        {
            // Respect local input: do not relocate the pointer or commit a drop
            // into the app it moved over after this request was accepted.
            tracing::info!(actual = ?cursor, expected = ?wake.target, "native drop destination moved; cancelling");
            wake.batch.cancel("drop-target-moved");
            cancelled = true;
        }
    }
    let release = owned
        && wake.down_owned.load(Ordering::Acquire)
        && (cancelled || accepted_copy(wake.feedback_effect.load(Ordering::Acquire)));
    if wake.released.load(Ordering::Acquire) {
        return;
    }
    if wake.down_sent.load(Ordering::Acquire) && !owned {
        return;
    }
    // Keep DragOver progressing while a renderer asynchronously negotiates its
    // effect. OLE's capture consumes the eventual paired release; neither event
    // changes the fixed destination coordinates.
    if release && !cancelled {
        wake.release_intent.store(true, Ordering::Release);
    }
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: if release {
                    MOUSEEVENTF_LEFTUP
                } else {
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE
                },
                ..MOUSEINPUT::default()
            },
        },
    };
    let sent = SendInput(&[input], size_of::<INPUT>() as i32);
    if release && sent == 1 {
        wake.released.store(true, Ordering::Release);
        wake.down_owned.store(false, Ordering::Release);
    }
    if release && sent != 1 {
        wake.release_intent.store(false, Ordering::Release);
    }
    if release {
        tracing::info!(target = ?capture, cancelled, sent, "native OLE captured release sent");
    }
    if sent != 1 {
        tracing::warn!("native drop continuation wake failed");
    }
}
fn start_wake(wake: Arc<DragWake>, native_thread: u32) -> Result<WakeGuard> {
    let thread_wake = wake.clone();
    let thread = std::thread::Builder::new()
        .name("bubbleplay-native-drop-wake".into())
        .spawn(move || {
            // Independent of the STA modal loop: an STA timer cannot wake a loop
            // that has not begun dispatching that thread's timer messages.
            while !thread_wake.returned.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(30));
                if thread_wake.returned.load(Ordering::Acquire) {
                    break;
                }
                if thread_wake.virtual_release_started.load(Ordering::Acquire) {
                    let committed = thread_wake.committed_at.lock().unwrap().unwrap();
                    let state = thread_wake.batch.0.state.lock().unwrap();
                    let expired = post_drop_expired(
                        state.content_requested,
                        committed,
                        Instant::now(),
                        thread_wake.batch.remaining(&state),
                    );
                    let cancelled = state.error.is_some();
                    drop(state);
                    if expired || cancelled {
                        thread_wake.batch.cancel("drop-timeout");
                        tracing::warn!("native OLE post-drop watchdog cancelled stalled target");
                        break;
                    }
                    continue;
                }
                if thread_wake.started.elapsed() >= Duration::from_secs(4) {
                    break;
                }
                unsafe {
                    wake_drag(&thread_wake, native_thread);
                }
            }
            if thread_wake.down_owned.swap(false, Ordering::AcqRel) {
                // Pair our captured DOWN even if OLE lost capture or was
                // cancelled. No application received the initiating DOWN.
                unsafe {
                    let input = INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dwFlags: MOUSEEVENTF_LEFTUP,
                                ..MOUSEINPUT::default()
                            },
                        },
                    };
                    let sent = SendInput(&[input], size_of::<INPUT>() as i32);
                    tracing::info!(sent, "native OLE owned button cleanup release sent");
                }
            }
            if !thread_wake.returned.load(Ordering::Acquire) {
                thread_wake.batch.cancel("drop-timeout");
                // Quit only this operation's private STA modal loop, never an app
                // window. A target that never negotiates must not leave input gated.
                unsafe {
                    let _ = PostThreadMessageW(native_thread, WM_QUIT, WPARAM(0), LPARAM(0));
                }
                thread_wake.routing.release();
                tracing::warn!("native OLE deadline reached; private STA quit requested");
            }
        })
        .context("spawn native OLE continuation helper")?;
    Ok(WakeGuard {
        wake,
        thread: Some(thread),
    })
}
fn pump_messages() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
fn run_drop(batch: &DropBatch, point: POINT, target: Option<NativeDropTarget>) -> Result<()> {
    unsafe {
        OleInitialize(None).context("initialize OLE STA")?;
        struct OleGuard;
        impl Drop for OleGuard {
            fn drop(&mut self) {
                unsafe {
                    OleUninitialize();
                }
            }
        }
        let _ole = OleGuard;
        if !crate::clipboard::input_desktop_is_default() {
            batch.cancel("drop-unavailable");
            return Ok(());
        }
        if batch.0.state.lock().unwrap().error.is_some() {
            return Ok(());
        }
        if target
            .as_ref()
            .is_some_and(|target| !target.matches_current())
        {
            batch.cancel("drop-target-changed");
            tracing::info!(
                reason = "drop-target-changed",
                "prepared native file drop cancelled"
            );
            return Ok(());
        }
        let drag_guard = DragGuard::acquire()?;
        if GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 {
            batch.cancel("drop-input-busy");
            return Ok(());
        }
        let queue_guard = match InputQueueGuard::attach() {
            Ok(guard) => guard,
            Err(error) => {
                tracing::warn!(%error, "native drop input queue unavailable");
                batch.cancel("drop-input-busy");
                return Ok(());
            }
        };
        SetCursorPos(point.x, point.y).context("position native drop")?;
        tracing::info!(x = point.x, y = point.y, "native OLE drop starting");
        let formats = Formats::register();
        tracing::info!(
            descriptors = formats.descriptors,
            contents = formats.contents,
            preferred = formats.preferred,
            "native drop formats registered"
        );
        if formats.list().iter().any(|f| f.cfFormat == 0) {
            bail!("register native drop formats");
        }
        let data: IDataObject = VirtualFiles::new(batch.clone(), formats).into();
        tracing::info!("native drop data object constructed");
        let wake = Arc::new(DragWake {
            batch: batch.clone(),
            target: point,
            prepared_target: target,
            routing: Arc::new(DropRouting {
                queue: queue_guard,
                drag: drag_guard,
            }),
            committed_at: Mutex::new(None),
            feedback_effect: AtomicUsize::new(0),
            release_intent: AtomicBool::new(false),
            released: AtomicBool::new(false),
            virtual_release_started: AtomicBool::new(false),
            warned: AtomicBool::new(false),
            started: Instant::now(),
            returned: AtomicBool::new(false),
            down_sent: AtomicBool::new(false),
            down_owned: AtomicBool::new(false),
        });
        let source: IDropSource = DropSource {
            batch: batch.clone(),
            feedback: AtomicUsize::new(0),
            started: Instant::now(),
            queries: AtomicUsize::new(0),
            entered: AtomicBool::new(false),
            wake: wake.clone(),
        }
        .into();
        tracing::info!("native drop source constructed");
        let wake_guard = start_wake(
            wake,
            windows::Win32::System::Threading::GetCurrentThreadId(),
        )?;
        tracing::info!("native drop continuation helper started");
        let mut effect = DROPEFFECT_NONE;
        if target
            .as_ref()
            .is_some_and(|target| !target.matches_current())
        {
            batch.cancel("drop-target-changed");
            tracing::info!(
                reason = "drop-target-changed",
                "prepared native file drop cancelled"
            );
            return Ok(());
        }
        tracing::info!("native drop entering DoDragDrop");
        let hr = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
        tracing::info!(
            hresult = hr.0,
            effect = effect.0,
            "native OLE drop returned"
        );
        wake_guard.wake.routing.release();
        drop(wake_guard);
        batch.0.state.lock().unwrap().disposition =
            Some(hr == DRAGDROP_S_DROP && effect == DROPEFFECT_COPY);
        drop(source);
        // Hold the object through EndOperation, pumping COM calls from Explorer's
        // extraction thread. Then release our ref and keep the STA alive while a
        // target still owns an object or stream (bounded retention).
        while {
            let state = batch.0.state.lock().unwrap();
            state.operation && state.error.is_none() && !batch.remaining(&state).is_zero()
        } {
            pump_messages();
            std::thread::sleep(Duration::from_millis(5));
        }
        if batch.0.state.lock().unwrap().operation {
            batch.cancel("drop-timeout");
        }
        drop(data);
        let retained_since = Instant::now();
        while batch.0.live_objects.load(Ordering::Acquire) != 0
            && retained_since.elapsed() < RETAIN_LIMIT
            && batch.0.created.elapsed() < OVERALL_LIMIT
        {
            pump_messages();
            std::thread::sleep(Duration::from_millis(10));
        }
        if batch.0.live_objects.load(Ordering::Acquire) != 0 {
            // The apartment is about to close, making remote COM proxies unusable.
            // Bound cache retention even if a target leaked a reference indefinitely.
            batch.cancel("drop-timeout");
            for path in &batch.0.paths {
                let _ = fs::remove_file(path);
            }
            let _ = fs::remove_dir(&batch.0.cache);
            batch.0.reservation.release();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_mixed_batches_wait_for_all_verified_slots_and_preserve_failures() {
        let request = |sizes: &[u64]| {
            let metas = sizes
                .iter()
                .enumerate()
                .map(|(index, size)| {
                    let mut meta = meta();
                    meta.size = *size;
                    meta.batch_count = sizes.len() as u32;
                    meta.batch_index = index as u32;
                    meta
                })
                .collect();
            NativeDropRequest::new(
                DropBatch::new(metas).unwrap(),
                meta().drop.unwrap(),
                "test".into(),
            )
        };
        let small = request(&[
            PREPARE_NATIVE_DROP_BYTES / 2,
            PREPARE_NATIVE_DROP_BYTES / 2 - 1,
        ]);
        assert!(!small.preparation_required());
        assert_eq!(small.preparation_status(), Ok(true));
        let large = request(&[PREPARE_NATIVE_DROP_BYTES / 2, PREPARE_NATIVE_DROP_BYTES / 2]);
        assert!(large.preparation_required());
        assert_eq!(large.preparation_status(), Ok(false));
        large.batch.verified(0);
        assert_eq!(large.preparation_status(), Ok(false));
        large.batch.verified(1);
        assert_eq!(large.preparation_status(), Ok(true));
        large.batch.cancel("hash-mismatch");
        assert_eq!(large.preparation_status(), Err("hash-mismatch".into()));
        let cancelled = request(&[PREPARE_NATIVE_DROP_BYTES]);
        cancelled.batch.cancel("drop-cancelled");
        assert_eq!(cancelled.preparation_status(), Err("drop-cancelled".into()));
    }
    #[test]
    fn prepared_target_rejects_changed_geometry_or_window_identity() {
        let window = TargetWindow {
            handle: 7,
            process: 8,
            thread: 9,
            bounds: [0, 0, 100, 100],
        };
        let target = NativeDropTarget {
            point: POINT { x: 5, y: 5 },
            leaf: window,
            root: window,
            foreground: window,
        };
        assert!(target.same_binding(&target));
        let mut changed = target;
        changed.leaf.bounds[2] = 101;
        assert!(!target.same_binding(&changed));
        changed = target;
        changed.root.handle += 1;
        assert!(!target.same_binding(&changed));
        changed = target;
        changed.leaf.process += 1;
        assert!(!target.same_binding(&changed));
        changed = target;
        changed.leaf.thread += 1;
        assert!(!target.same_binding(&changed));
        changed = target;
        changed.foreground.handle += 1;
        assert!(!target.same_binding(&changed));
    }
    #[test]
    fn native_file_formats_reject_unprovided_browser_policy_context() {
        let batch = DropBatch::new(vec![meta()]).unwrap();
        let formats = Formats {
            descriptors: 1,
            contents: 2,
            preferred: 3,
        };
        let object = VirtualFiles::new(batch.clone(), formats);
        assert_eq!(object.validate(&fmt(1, -1, TYMED_HGLOBAL)), S_OK);
        assert_eq!(object.validate(&fmt(2, 0, TYMED_ISTREAM)), S_OK);
        assert_eq!(object.validate(&fmt(4, -1, TYMED_HGLOBAL)), DV_E_FORMATETC);
        let data: IDataObject = object.into();
        batch.cancel("drop-cancelled");
        assert_eq!(
            unsafe { data.GetData(&fmt(2, 0, TYMED_ISTREAM)) }
                .err()
                .unwrap()
                .code(),
            E_ABORT
        );
    }
    #[test]
    fn committed_drop_restores_input_without_old_cleanup_releasing_new_drag() {
        let gate = AtomicBool::new(true);
        let old = AtomicBool::new(true);
        release_owned_gate(&old, &gate);
        assert!(!gate.load(Ordering::Acquire));
        gate.store(true, Ordering::Release); // a new drag owns the gate
        release_owned_gate(&old, &gate); // old RAII fallback must be harmless
        assert!(gate.load(Ordering::Acquire));
    }
    #[test]
    fn post_drop_watchdog_bounds_no_content_but_allows_healthy_extraction() {
        let start = Instant::now();
        let later = start + POST_DROP_NEGOTIATION_LIMIT;
        assert!(post_drop_expired(false, start, later, WAIT_LIMIT));
        assert!(!post_drop_expired(true, start, later, WAIT_LIMIT));
        assert!(post_drop_expired(true, start, later, Duration::ZERO));
    }
    #[test]
    fn cache_budget_bounds_empty_batches_and_releases_both_reservations() {
        let budget = CacheBudget::new();
        for _ in 0..MAX_RETAINED_BATCHES {
            budget.reserve(0).unwrap();
        }
        assert!(budget.reserve(0).is_err());
        assert_eq!(budget.batches.load(Ordering::Acquire), MAX_RETAINED_BATCHES);
        assert_eq!(budget.bytes.load(Ordering::Acquire), 0);
        budget.release(0);
        budget.reserve(0).unwrap();
        for _ in 0..MAX_RETAINED_BATCHES {
            budget.release(0);
        }
        // A failed byte reservation also returns its batch slot.
        budget.reserve(ft::MAX_FILE_BYTES).unwrap();
        assert!(budget.reserve(1).is_err());
        assert_eq!(budget.batches.load(Ordering::Acquire), 1);
        assert_eq!(budget.bytes.load(Ordering::Acquire), ft::MAX_FILE_BYTES);
        budget.release(ft::MAX_FILE_BYTES);
        budget.reserve(1).unwrap();
        budget.release(1);
        assert_eq!(budget.batches.load(Ordering::Acquire), 0);
        assert_eq!(budget.bytes.load(Ordering::Acquire), 0);
    }
    #[test]
    fn renderer_acceptance_requires_copy_and_actual_paired_release() {
        // Chromium initially negotiates NONE even after a trusted DragEnter.
        assert!(!accepted_copy(DROPEFFECT_NONE.0 as usize));
        assert!(!accepted_copy(DROPEFFECT_MOVE.0 as usize));
        assert!(accepted_copy(DROPEFFECT_COPY.0 as usize));
        assert!(!virtual_drop_ready(false, MODIFIERKEYS_FLAGS(0)));
        assert!(!virtual_drop_ready(true, MK_LBUTTON));
        assert!(virtual_drop_ready(true, MODIFIERKEYS_FLAGS(0)));
    }
    fn meta() -> OfferMeta {
        OfferMeta {
            name: "tiny.txt".into(),
            size: 5,
            mime: "text/plain".into(),
            mtime_ms: 0,
            purpose: ft::Purpose::Drop,
            batch_id: 1,
            batch_index: 0,
            batch_count: 1,
            drop: Some(DropPoint {
                x: 1,
                y: 2,
                generation: 1,
                viewport_revision: 0,
            }),
        }
    }
    #[test]
    fn descriptor_is_available_before_verified_content() {
        let batch = DropBatch::new(vec![meta()]).unwrap();
        let bytes = descriptor_bytes(&batch.0.metas);
        assert_eq!(bytes.len(), 4 + size_of::<FILEDESCRIPTORW>());
        assert_eq!(&bytes[..4], &1u32.to_le_bytes());
        assert!(batch.result().is_none());
        batch.0.state.lock().unwrap().disposition = Some(true);
        assert!(batch.result().is_none());
        batch.verified(0);
        assert_eq!(batch.result(), Some(Ok(())));
    }
    #[test]
    fn stream_waits_for_verification_and_cache_lives_with_stream() {
        let batch = DropBatch::new(vec![meta()]).unwrap();
        let path = batch.path(0);
        let dir = batch.0.cache.clone();
        fs::write(&path, b"hello").unwrap();
        let stream = VerifiedStream::new(batch.clone(), 0, 0);
        let writer = batch.clone();
        let task = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            writer.verified(0);
        });
        let mut bytes = [0; 5];
        assert_eq!(stream.read(&mut bytes).unwrap(), 5);
        assert_eq!(&bytes, b"hello");
        task.join().unwrap();
        drop(batch);
        assert!(path.exists());
        drop(stream);
        assert!(!dir.exists());
    }
    #[test]
    fn failed_integrity_never_releases_content() {
        let batch = DropBatch::new(vec![meta()]).unwrap();
        fs::write(batch.path(0), b"wrong").unwrap();
        batch.cancel("hash-mismatch");
        assert_eq!(batch.result(), Some(Err("hash-mismatch".into())));
        assert!(VerifiedStream::new(batch, 0, 0).read(&mut [0; 5]).is_err());
    }
    #[test]
    fn descriptor_names_are_unique_safe_and_bounded() {
        let inputs = [
            "file.txt".to_owned(),
            "file (1).txt".to_owned(),
            "FILE.TXT".to_owned(),
            format!("{} .txt", "x".repeat(400)),
            format!("{} .txt", "x".repeat(400)),
        ];
        let metas = inputs
            .iter()
            .map(|name| {
                let mut m = meta();
                m.name = name.clone();
                m
            })
            .collect::<Vec<_>>();
        let bytes = descriptor_bytes(&metas);
        let names = (0..metas.len())
            .map(|index| {
                let fd: FILEDESCRIPTORW = unsafe {
                    std::ptr::read_unaligned(
                        bytes[4 + index * size_of::<FILEDESCRIPTORW>()..]
                            .as_ptr()
                            .cast(),
                    )
                };
                let units = fd.cFileName;
                String::from_utf16(&units[..units.iter().position(|v| *v == 0).unwrap()]).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(&names[..3], &["file.txt", "file (1).txt", "FILE (2).TXT"]);
        for (index, name) in names.iter().enumerate() {
            assert!(name.encode_utf16().count() <= 259);
            assert!(!name.ends_with(['.', ' ']));
            assert!(names[..index]
                .iter()
                .all(|prior| !prior.eq_ignore_ascii_case(name)));
        }
    }
    #[test]
    fn asynchronous_failed_completion_cannot_report_success() {
        let batch = DropBatch::new(vec![meta()]).unwrap();
        batch.verified(0);
        batch.accepted_for_test();
        let data: IDataObjectAsyncCapability =
            VirtualFiles::new(batch.clone(), Formats::register()).into();
        unsafe {
            data.StartOperation(None).unwrap();
        }
        assert!(batch.result().is_none());
        unsafe {
            data.EndOperation(E_FAIL, None, DROPEFFECT_COPY.0).unwrap();
        }
        assert_eq!(batch.result(), Some(Err("drop-failed".into())));
    }
    #[test]
    fn active_large_uploads_use_idle_timeout_with_overall_bound() {
        let created = Instant::now();
        let now = created + Duration::from_secs(15 * 60);
        let recent_progress = now - Duration::from_secs(1);
        assert_eq!(
            progress_remaining(created, recent_progress, now),
            Duration::from_secs(119)
        );
        assert!(progress_remaining(created, now - WAIT_LIMIT, now).is_zero());
        let overall_end = created + OVERALL_LIMIT;
        assert!(progress_remaining(created, overall_end, overall_end).is_zero());
        assert_eq!(
            progress_remaining(
                created,
                overall_end - Duration::from_secs(1),
                overall_end - Duration::from_secs(1)
            ),
            Duration::from_secs(1)
        );
    }
}
