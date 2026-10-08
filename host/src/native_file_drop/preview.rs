//! Hover-only OLE source. It cannot commit a Drop or render file contents.
use super::*;

const PREVIEW_LEASE: Duration = Duration::from_secs(2);
const PREVIEW_LIMIT: Duration = Duration::from_secs(120);
const CANCEL_BARRIER: Duration = Duration::from_secs(2);
static PREVIEW_ACTIVE: AtomicBool = AtomicBool::new(false);

struct PreviewUpdate {
    point: POINT,
    heartbeat: Instant,
    error: Option<String>,
}
struct PreviewState {
    update: Mutex<PreviewUpdate>,
    cancelled: AtomicBool,
    finished: AtomicBool,
    created: Instant,
}
impl PreviewState {
    fn fail(&self, reason: &str) {
        let mut update = self.update.lock().unwrap();
        if update.error.is_none() {
            update.error = Some(reason.into());
        }
        self.cancelled.store(true, Ordering::Release);
    }
    fn expired(&self) -> bool {
        let update = self.update.lock().unwrap();
        preview_expired(self.created, update.heartbeat, Instant::now())
    }
}
fn preview_expired(created: Instant, heartbeat: Instant, now: Instant) -> bool {
    now.saturating_duration_since(heartbeat) >= PREVIEW_LEASE
        || now.saturating_duration_since(created) >= PREVIEW_LIMIT
}

/// A native hover lease. The owner must cancel it on leave/disconnect/geometry
/// invalidation, and refresh it with validated host points while hovering.
pub struct NativeDropPreview {
    state: Arc<PreviewState>,
}
impl NativeDropPreview {
    pub fn start(count: u16, point: POINT) -> Result<Self> {
        Self::start_with_types(count, &[], point)
    }
    pub fn start_with_types(count: u16, mime_hints: &[String], point: POINT) -> Result<Self> {
        let names = preview_names(count, mime_hints)?;
        if PREVIEW_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            bail!("native hover preview already active");
        }
        let state = Arc::new(PreviewState {
            update: Mutex::new(PreviewUpdate {
                point,
                heartbeat: Instant::now(),
                error: None,
            }),
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            created: Instant::now(),
        });
        let thread_state = state.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("bubbleplay-native-hover".into())
            .spawn(move || {
                struct Finish(Arc<PreviewState>);
                impl Drop for Finish {
                    fn drop(&mut self) {
                        PREVIEW_ACTIVE.store(false, Ordering::Release);
                        self.0.finished.store(true, Ordering::Release);
                    }
                }
                let _finish = Finish(thread_state.clone());
                if let Err(error) = run_preview(thread_state.clone(), names) {
                    tracing::warn!(%error, "native hover preview failed");
                    thread_state.fail("drop-preview-unavailable");
                }
            })
        {
            PREVIEW_ACTIVE.store(false, Ordering::Release);
            return Err(error).context("spawn native hover preview");
        }
        Ok(Self { state })
    }
    pub fn update(&self, point: POINT) -> Result<()> {
        if self.state.cancelled.load(Ordering::Acquire) || self.finished() {
            bail!("native hover preview ended");
        }
        let mut update = self.state.update.lock().unwrap();
        update.point = point;
        update.heartbeat = Instant::now();
        Ok(())
    }
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Release);
    }
    pub fn finished(&self) -> bool {
        self.state.finished.load(Ordering::Acquire)
    }
    pub fn error(&self) -> Option<String> {
        self.state.update.lock().unwrap().error.clone()
    }
    /// Cancel the provisional source first; target-cached names never carry over
    /// to the real file data object. Waiting occurs on a coordinator thread.
    pub fn start_final(self, request: NativeDropRequest, point: POINT) -> Result<()> {
        self.cancel();
        let batch = request.batch.clone();
        let spawned = std::thread::Builder::new()
            .name("bubbleplay-hover-to-drop".into())
            .spawn(move || {
                let since = Instant::now();
                while !self.finished() && since.elapsed() < CANCEL_BARRIER {
                    std::thread::sleep(Duration::from_millis(5));
                }
                if !self.finished() {
                    request.reject("drop-unavailable");
                    return;
                }
                if let Err(error) = request.start(point) {
                    tracing::warn!(%error, "native final drop after preview failed");
                }
            });
        if let Err(error) = spawned {
            batch.cancel("drop-unavailable");
            return Err(error).context("spawn native hover cancellation barrier");
        }
        Ok(())
    }
}
impl Drop for NativeDropPreview {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn preview_names(count: u16, hints: &[String]) -> Result<Vec<String>> {
    if count == 0
        || count as u32 > ft::MAX_BATCH_FILES
        || hints.len() > count as usize
        || hints.iter().any(|mime| mime.len() > 96)
    {
        bail!("invalid native preview metadata");
    }
    Ok((0..count as usize)
        .map(|index| {
            let extension = match hints
                .get(index)
                .map(|v| v.trim().to_ascii_lowercase())
                .as_deref()
            {
                Some("image/png") => "png",
                Some("image/jpeg") => "jpg",
                Some("image/gif") => "gif",
                Some("image/webp") => "webp",
                Some("image/bmp") => "bmp",
                Some("text/plain") => "txt",
                Some("application/pdf") => "pdf",
                Some("video/mp4") => "mp4",
                Some("audio/mpeg") => "mp3",
                _ => "bin",
            };
            format!("incoming-file-{}.{}", index + 1, extension)
        })
        .collect())
}
fn preview_descriptors(names: &[String]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + names.len() * size_of::<FILEDESCRIPTORW>());
    bytes.extend_from_slice(&(names.len() as u32).to_le_bytes());
    for name in names {
        let mut fd = FILEDESCRIPTORW {
            // Protected browser drag data does not expose sizes. Never claim
            // these provisional names describe actual empty files.
            dwFlags: (FD_ATTRIBUTES.0 | FD_UNICODE.0) as u32,
            dwFileAttributes: FILE_ATTRIBUTE_NORMAL.0,
            ..FILEDESCRIPTORW::default()
        };
        for (index, unit) in name.encode_utf16().enumerate() {
            fd.cFileName[index] = unit;
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

#[implement(IDataObject)]
struct PreviewFiles {
    formats: Formats,
    names: Vec<String>,
}
impl PreviewFiles {
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
            if f.lindex < 0 || f.lindex as usize >= self.names.len() {
                return DV_E_LINDEX;
            }
            TYMED_ISTREAM
        } else if f.cfFormat == self.formats.descriptors || f.cfFormat == self.formats.preferred {
            if f.lindex != -1 {
                return DV_E_LINDEX;
            }
            TYMED_HGLOBAL
        } else {
            return DV_E_FORMATETC;
        };
        if f.tymed & medium.0 as u32 == 0 {
            DV_E_TYMED
        } else {
            S_OK
        }
    }
}
impl IDataObject_Impl for PreviewFiles_Impl {
    fn GetData(&self, ptr: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        self.validate(ptr).ok()?;
        let f = unsafe { &*ptr };
        if f.cfFormat == self.formats.contents {
            return Err(E_ABORT.into());
        }
        if f.cfFormat == self.formats.descriptors {
            hglobal(&preview_descriptors(&self.names))
        } else {
            hglobal(&DROPEFFECT_COPY.0.to_le_bytes())
        }
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_ABORT.into())
    }
    fn QueryGetData(&self, f: *const FORMATETC) -> HRESULT {
        self.validate(f)
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

#[implement(IDropSource)]
struct PreviewSource {
    state: Arc<PreviewState>,
    feedback: AtomicUsize,
    queries: AtomicUsize,
}
impl IDropSource_Impl for PreviewSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, _: MODIFIERKEYS_FLAGS) -> HRESULT {
        if self.queries.fetch_add(1, Ordering::Relaxed) == 0 {
            tracing::info!("native hover preview continuation started");
        }
        if escape.as_bool() || self.state.cancelled.load(Ordering::Acquire) || self.state.expired()
        {
            DRAGDROP_S_CANCEL
        } else {
            S_OK
        } // Never DROP, even if the physical/synthetic button is up.
    }
    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        if self.feedback.swap(effect.0 as usize, Ordering::AcqRel) != effect.0 as usize {
            tracing::info!(effect = effect.0, "native hover preview feedback changed");
        }
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

fn run_preview(state: Arc<PreviewState>, names: Vec<String>) -> Result<()> {
    unsafe {
        OleInitialize(None).context("initialize preview OLE")?;
        struct Ole;
        impl Drop for Ole {
            fn drop(&mut self) {
                unsafe {
                    OleUninitialize();
                }
            }
        }
        let _ole = Ole;
        if state.cancelled.load(Ordering::Acquire) || state.expired() {
            return Ok(());
        }
        if !crate::clipboard::input_desktop_is_default() {
            bail!("preview input desktop unavailable");
        }
        let drag = DragGuard::acquire()?;
        let queue = InputQueueGuard::attach()?; // refuses every held physical key/button
        let point = state.update.lock().unwrap().point;
        SetCursorPos(point.x, point.y).context("position native preview")?;
        let routing = Arc::new(DropRouting { queue, drag });
        let data: IDataObject = PreviewFiles {
            formats: Formats::register(),
            names,
        }
        .into();
        let source: IDropSource = PreviewSource {
            state: state.clone(),
            feedback: AtomicUsize::new(usize::MAX),
            queries: AtomicUsize::new(0),
        }
        .into();
        let returned = Arc::new(AtomicBool::new(false));
        let helper = PreviewWake::start(state.clone(), routing.clone(), returned.clone(), point)?;
        let mut effect = DROPEFFECT_NONE;
        tracing::info!("native hover preview starting");
        let result = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
        returned.store(true, Ordering::Release);
        drop(helper);
        routing.release();
        tracing::info!(
            hresult = result.0,
            effect = effect.0,
            "native hover preview cancelled"
        );
        if result == DRAGDROP_S_DROP {
            bail!("preview source unexpectedly committed");
        }
        Ok(())
    }
}

struct PreviewWake {
    returned: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl PreviewWake {
    unsafe fn start(
        state: Arc<PreviewState>,
        routing: Arc<DropRouting>,
        returned: Arc<AtomicBool>,
        point: POINT,
    ) -> Result<Self> {
        let native = windows::Win32::System::Threading::GetCurrentThreadId();
        let stopped = returned.clone();
        let thread = std::thread::Builder::new()
            .name("bubbleplay-hover-wake".into())
            .spawn(move || {
                let mut last_point = point;
                let mut down_owned = false;
                let mut cancel_since = None;
                while !stopped.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(30));
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    if state.expired() {
                        state.fail("drop-preview-timeout");
                    }
                    let cancelled = state.cancelled.load(Ordering::Acquire);
                    if cancelled && cancel_since.is_none() {
                        cancel_since = Some(Instant::now());
                    }
                    if cancel_since.is_some_and(|at: Instant| at.elapsed() >= CANCEL_BARRIER) {
                        unsafe {
                            let _ = PostThreadMessageW(native, WM_QUIT, WPARAM(0), LPARAM(0));
                        }
                        break;
                    }
                    unsafe {
                        let mut gui = GUITHREADINFO {
                            cbSize: size_of::<GUITHREADINFO>() as u32,
                            ..GUITHREADINFO::default()
                        };
                        let _ = GetGUIThreadInfo(native, &mut gui);
                        let mut process = 0;
                        let owner = GetWindowThreadProcessId(gui.hwndCapture, Some(&mut process));
                        let owned = !gui.hwndCapture.0.is_null()
                            && owner == native
                            && process == std::process::id();
                        if down_owned && cancelled {
                            if send_preview_mouse(MOUSEEVENTF_LEFTUP) {
                                down_owned = false;
                            }
                            continue;
                        }
                        if cancelled {
                            send_preview_mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE);
                            continue;
                        }
                        if !crate::clipboard::input_desktop_is_default() {
                            state.fail("drop-preview-unavailable");
                            continue;
                        }
                        let mut cursor = POINT::default();
                        if GetCursorPos(&mut cursor).is_err()
                            || cursor.x != last_point.x
                            || cursor.y != last_point.y
                        {
                            state.fail("drop-preview-local-input");
                            continue;
                        }
                        if !owned {
                            send_preview_mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE);
                            continue;
                        }
                        if !down_owned {
                            if (1..=255).any(|key| GetAsyncKeyState(key) < 0) {
                                state.fail("drop-preview-input-busy");
                                continue;
                            }
                            down_owned = send_preview_mouse(MOUSEEVENTF_LEFTDOWN);
                            if !down_owned {
                                state.fail("drop-preview-unavailable");
                            }
                            continue;
                        }
                        // Never press or relocate over local input. Ignore only the
                        // LEFT state this operation deliberately owns.
                        if (1..=255)
                            .any(|key| key != VK_LBUTTON.0 as i32 && GetAsyncKeyState(key) < 0)
                            || GetAsyncKeyState(VK_LBUTTON.0 as i32) >= 0
                        {
                            state.fail("drop-preview-local-input");
                            continue;
                        }
                        let target = state.update.lock().unwrap().point;
                        if target.x != last_point.x || target.y != last_point.y {
                            if SetCursorPos(target.x, target.y).is_err() {
                                state.fail("drop-preview-unavailable");
                                continue;
                            }
                            last_point = target;
                        }
                        send_preview_mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE);
                    }
                }
                if down_owned {
                    unsafe {
                        send_preview_mouse(MOUSEEVENTF_LEFTUP);
                    }
                }
                routing.release();
            })
            .context("spawn native hover wake")?;
        Ok(Self {
            returned,
            thread: Some(thread),
        })
    }
}
impl Drop for PreviewWake {
    fn drop(&mut self) {
        self.returned.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
unsafe fn send_preview_mouse(
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) -> bool {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: flags,
                ..MOUSEINPUT::default()
            },
        },
    };
    SendInput(&[input], size_of::<INPUT>() as i32) == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_descriptor_leaves_unknown_file_size_unspecified() {
        let names = preview_names(1, &["image/png".into()]).unwrap();
        let bytes = preview_descriptors(&names);
        let descriptor =
            unsafe { std::ptr::read_unaligned(bytes[4..].as_ptr().cast::<FILEDESCRIPTORW>()) };
        assert_eq!(descriptor.dwFlags & FD_FILESIZE.0 as u32, 0);
        assert_ne!(descriptor.dwFlags & FD_UNICODE.0 as u32, 0);
    }
    #[test]
    fn preview_source_never_commits_and_contents_never_render() {
        let state = Arc::new(PreviewState {
            update: Mutex::new(PreviewUpdate {
                point: POINT::default(),
                heartbeat: Instant::now(),
                error: None,
            }),
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            created: Instant::now(),
        });
        let source: IDropSource = PreviewSource {
            state: state.clone(),
            feedback: AtomicUsize::new(usize::MAX),
            queries: AtomicUsize::new(0),
        }
        .into();
        assert_eq!(
            unsafe { source.QueryContinueDrag(false, MODIFIERKEYS_FLAGS(0)) },
            S_OK
        );
        assert_eq!(unsafe { source.QueryContinueDrag(false, MK_LBUTTON) }, S_OK);
        let data: IDataObject = PreviewFiles {
            formats: Formats {
                descriptors: 1,
                contents: 2,
                preferred: 3,
            },
            names: vec!["incoming-file-1.png".into()],
        }
        .into();
        assert_eq!(
            unsafe { data.GetData(&fmt(2, 0, TYMED_ISTREAM)) }
                .err()
                .unwrap()
                .code(),
            E_ABORT
        );
        state.cancelled.store(true, Ordering::Release);
        assert_eq!(
            unsafe { source.QueryContinueDrag(false, MK_LBUTTON) },
            DRAGDROP_S_CANCEL
        );
    }
    #[test]
    fn preview_names_use_only_bounded_known_mime_extensions() {
        assert_eq!(
            preview_names(3, &["image/png".into(), "../../secret.exe".into()]).unwrap(),
            vec![
                "incoming-file-1.png",
                "incoming-file-2.bin",
                "incoming-file-3.bin"
            ]
        );
        assert!(preview_names(0, &[]).is_err());
        assert!(preview_names(65, &[]).is_err());
        assert!(preview_names(1, &["x".repeat(97)]).is_err());
    }
    #[test]
    fn heartbeat_cannot_extend_overall_preview_bound() {
        let start = Instant::now();
        assert!(!preview_expired(
            start,
            start,
            start + Duration::from_secs(1)
        ));
        assert!(preview_expired(start, start, start + PREVIEW_LEASE));
        assert!(preview_expired(
            start,
            start + PREVIEW_LIMIT,
            start + PREVIEW_LIMIT
        ));
    }
}
