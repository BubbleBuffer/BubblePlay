//! DXGI Desktop Duplication capture via the `windows-capture` crate.
//!
//! Zero-copy contract: `acquire()` hands out the crate's owned
//! [`DxgiDuplicationFrame`], whose `.texture()` is the GPU desktop frame.
//! The caller encodes it synchronously and drops the frame; the actual DDA
//! `ReleaseFrame` happens inside the *next* `acquire_next_frame` call.
//!
//! Duplicate-frame skipping: a DDA acquire can wake up for cursor-only updates
//! (`LastPresentTime` unchanged). We compare against the last present time we
//! encoded and report `is_new=false` so the caller can skip encoding.
//!
//! The NVENC session is opened on the D3D11 device this API owns. That device
//! is available before DDA produces a desktop frame, which permits the
//! one-shot idle-desktop snapshot below.

use anyhow::{bail, Context, Result};
use core::ffi::c_void;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_RESOURCE_MISC_GDI_COMPATIBLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_IDENTITY,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGISurface1, DXGI_OUTDUPL_MOVE_RECT,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDCW, CreateDIBSection, DeleteDC, DeleteObject,
    GetMonitorInfoW, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
    DIB_RGB_COLORS, HMONITOR, MONITORINFO, SRCCOPY,
};
use windows_capture::dxgi_duplication_api::{
    DxgiDuplicationApi, DxgiDuplicationFrame, Error as CaptureError,
};
use windows_capture::encoder::{ImageEncoder, ImageEncoderPixelFormat, ImageFormat};
use windows_capture::monitor::Monitor;

use crate::activity::Activity;
use crate::capture_wait::{CaptureInvalidation, DdaWaiter, RawDesktopFrame};
use crate::gpu_gate;

/// Longest AcquireNextFrame wait made while holding the device gate.
const DIRECT_ACQUIRE_MAX_MS: u32 = 16;
use crate::frame_damage::DamageRect;
use crate::input::DesktopRegion;

pub struct Capture {
    // Option lets AccessLost recovery consume/drop the invalid duplication
    // handle before opening its replacement. Creating a second duplication
    // while the old handle is still alive can fail with E_ACCESSDENIED.
    api: Option<DxgiDuplicationApi>,
    width: u32,
    height: u32,
    origin_x: i32,
    origin_y: i32,
    frames_acquired: u64,
    frames_new: u64,
    last_present_qpc: i64,
    device_obj: Option<ID3D11Device>,
    /// Set when an acquire hits AccessLost; recreation is deferred to the top
    /// of the *next* acquire because the escaping frame borrows `self.api`
    /// for the whole call, so we cannot take `&mut self` in the Err arm.
    pending_recreate: bool,
    /// Backoff deadline for a failed AccessLost recreation. Keeping this as
    /// state avoids sleeping inside `acquire()` and starving the RTC pump.
    next_recreate_attempt: Instant,
    /// A replacement stays unavailable until a real frame or an authorized
    /// GDI snapshot verifies that the normal desktop can be captured.
    awaiting_recovery_frame: bool,
    /// If a replacement immediately loses access, discard the complete
    /// D3D11/duplication session on the next attempt.
    fresh_recreate_required: bool,
    recreate_failed: bool,
    /// Retained for AccessLost recreation. `None` = primary monitor.
    monitor_filter: Option<String>,
    /// The selected output's DeviceName used by the GDI idle snapshot.
    monitor_device_name: String,
    idle_snapshot: IdleSnapshotState,
    idle_gdi: Option<GdiSnapshot>,
    damage_metadata: DamageMetadata,
    desktop_check: DesktopCheck,
    event_activity: Option<Activity>,
    waiter: Option<DdaWaiter>,
    invalidation: Option<CaptureInvalidation>,
    availability: CaptureAvailability,
    availability_transition: Option<CaptureAvailability>,
    /// Cause recorded by the transition that made the source unavailable.
    /// Cleared when it becomes active; read through `unavailable_reason()`.
    unavailable_reason: Option<CaptureUnavailableReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureAvailability {
    Active,
    Unavailable,
}

/// Why the duplication source is unavailable. HRESULTs are the native codes
/// Windows returned; `None` means the failing call supplied no HRESULT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureUnavailableReason {
    /// The console display is off; recovery waits for it to power on.
    DisplayOff,
    /// A display configuration notification invalidated the duplication.
    DisplayChanged,
    /// The input desktop is not Default (UAC, lock screen or another desktop).
    SecureDesktop,
    /// DXGI_ERROR_ACCESS_LOST (0x887A0026) from acquire or creation.
    AccessLost { hresult: u32 },
    /// E_ACCESSDENIED (0x80070005) from acquire or creation.
    AccessDenied { hresult: u32 },
    /// A recreation or verification attempt failed.
    RecreateFailed { hresult: Option<u32> },
    /// The duplication was recreated; no verified frame or snapshot has arrived.
    RecoveryAwaitingFrame,
}

impl fmt::Display for CaptureUnavailableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DisplayOff => f.write_str("display off"),
            Self::DisplayChanged => f.write_str("display configuration changed"),
            Self::SecureDesktop => f.write_str("input desktop is not Default"),
            Self::AccessLost { hresult } => {
                write!(
                    f,
                    "DXGI access lost (DXGI_ERROR_ACCESS_LOST, 0x{hresult:08X})"
                )
            }
            Self::AccessDenied { hresult } => {
                write!(f, "DDA access denied (E_ACCESSDENIED, 0x{hresult:08X})")
            }
            Self::RecreateFailed {
                hresult: Some(hresult),
            } => {
                write!(f, "DDA recreation failed (0x{hresult:08X})")
            }
            Self::RecreateFailed { hresult: None } => f.write_str("DDA recreation failed"),
            Self::RecoveryAwaitingFrame => f.write_str("DDA recreated; awaiting verified frame"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureRecoveryEvent {
    AccessLost,
    SessionRecreated,
    FrameAcquired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitorSwitchPlan {
    RecreateCurrent,
    OpenReplacement,
}

fn monitor_switch_plan(current: &str, requested: &str) -> MonitorSwitchPlan {
    if current.eq_ignore_ascii_case(requested) {
        MonitorSwitchPlan::RecreateCurrent
    } else {
        MonitorSwitchPlan::OpenReplacement
    }
}

fn availability_after(
    current: CaptureAvailability,
    event: CaptureRecoveryEvent,
) -> CaptureAvailability {
    match event {
        CaptureRecoveryEvent::AccessLost => CaptureAvailability::Unavailable,
        CaptureRecoveryEvent::SessionRecreated => current,
        CaptureRecoveryEvent::FrameAcquired => CaptureAvailability::Active,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub selector: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    pub selected: bool,
}

const IDLE_SNAPSHOT_RETRY_DELAY: Duration = Duration::from_millis(250);
pub const CAPTURE_RECOVERY_INTERVAL: Duration = Duration::from_millis(500);
const PARKED_RECOVERY_INTERVAL: Duration = Duration::from_secs(1);
const DESKTOP_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// Empty producer polls use this bounded housekeeping deadline. Real-frame
/// authorization, recovery and acquire errors bypass the cached result.
struct DesktopCheck {
    next_check: Instant,
    is_default: bool,
}

impl DesktopCheck {
    fn new(now: Instant) -> Self {
        Self {
            next_check: now,
            is_default: false,
        }
    }

    fn due(&self, now: Instant, force: bool) -> bool {
        force || now >= self.next_check
    }

    fn refresh(&mut self, force: bool) -> bool {
        let now = Instant::now();
        self.refresh_with(now, force, crate::desktop_session::input_desktop_is_default)
    }

    fn refresh_with(&mut self, now: Instant, force: bool, query: impl FnOnce() -> bool) -> bool {
        if self.due(now, force) {
            self.is_default = query();
            self.next_check = now + DESKTOP_CHECK_INTERVAL;
        }
        self.is_default
    }
}

/// Windows-capture reports AcquireNextFrame and DuplicateOutput HRESULTs
/// through WindowsError (sometimes nested inside DirectXError).
pub fn capture_error_is_unavailable(error: &CaptureError) -> bool {
    unavailable_reason_for_error(error).is_some()
}

const E_ACCESSDENIED_HRESULT: u32 = 0x80070005;
const DXGI_ERROR_ACCESS_LOST_HRESULT: u32 = 0x887A0026;

fn unavailable_reason_for_hresult(hresult: u32) -> Option<CaptureUnavailableReason> {
    match hresult {
        E_ACCESSDENIED_HRESULT => Some(CaptureUnavailableReason::AccessDenied { hresult }),
        DXGI_ERROR_ACCESS_LOST_HRESULT => Some(CaptureUnavailableReason::AccessLost { hresult }),
        _ => None,
    }
}

/// The unavailable reason for a DDA error, or `None` when it is not a
/// source-unavailable error (timeouts, invalid arguments and so on).
fn unavailable_reason_for_error(error: &CaptureError) -> Option<CaptureUnavailableReason> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = source {
        if matches!(
            error.downcast_ref::<CaptureError>(),
            Some(CaptureError::AccessLost)
        ) {
            return Some(CaptureUnavailableReason::AccessLost {
                hresult: DXGI_ERROR_ACCESS_LOST_HRESULT,
            });
        }
        if let Some(error) = error.downcast_ref::<windows::core::Error>() {
            if let Some(reason) = unavailable_reason_for_hresult(error.code().0 as u32) {
                return Some(reason);
            }
        }
        source = error.source();
    }
    None
}

/// Reason for a DDA error that the caller has already classified as
/// source-unavailable. Anything else is reported as a recreation failure.
fn dda_error_reason(error: &CaptureError) -> CaptureUnavailableReason {
    unavailable_reason_for_error(error).unwrap_or(CaptureUnavailableReason::RecreateFailed {
        hresult: error_chain_hresult(error),
    })
}

/// The first HRESULT in an error's source chain.
fn error_chain_hresult(error: &(dyn std::error::Error + 'static)) -> Option<u32> {
    let mut source = Some(error);
    while let Some(error) = source {
        if matches!(
            error.downcast_ref::<CaptureError>(),
            Some(CaptureError::AccessLost)
        ) {
            return Some(DXGI_ERROR_ACCESS_LOST_HRESULT);
        }
        if let Some(error) = error.downcast_ref::<windows::core::Error>() {
            return Some(error.code().0 as u32);
        }
        source = error.source();
    }
    None
}

/// Recreation failures keep the first HRESULT from anyhow's context chain.
fn recreate_failure_reason(error: &anyhow::Error) -> CaptureUnavailableReason {
    CaptureUnavailableReason::RecreateFailed {
        hresult: error.chain().find_map(error_chain_hresult),
    }
}

/// What `unavailable_reason()` reports. Display power outranks the recorded
/// cause, because recovery is gated on it. An active source reports nothing.
fn reported_unavailable_reason(
    availability: CaptureAvailability,
    display_off: bool,
    recorded: Option<CaptureUnavailableReason>,
) -> Option<CaptureUnavailableReason> {
    match availability {
        CaptureAvailability::Active => None,
        CaptureAvailability::Unavailable if display_off => {
            Some(CaptureUnavailableReason::DisplayOff)
        }
        CaptureAvailability::Unavailable => recorded,
    }
}

fn idle_snapshot_retry_due(now: Instant, retry_at: Instant) -> bool {
    now >= retry_at
}

fn recovery_attempt_due(pending: bool, now: Instant, retry_at: Instant) -> bool {
    pending && now >= retry_at
}

fn snapshot_can_verify_recovery(
    pending_recreate: bool,
    awaiting_recovery_frame: bool,
    recreate_failed: bool,
    has_api: bool,
) -> bool {
    !pending_recreate && awaiting_recovery_frame && !recreate_failed && has_api
}

fn supports_idle_snapshot_rotation(rotation: DXGI_MODE_ROTATION) -> bool {
    rotation == DXGI_MODE_ROTATION_IDENTITY
}

/// Keeps recovery-IDR snapshots retryable while also supporting a distinct
/// one-shot wake frame for deferred quiescent-detail verdicts.
struct IdleSnapshotState {
    pending: bool,
    retry_at: Instant,
    followup_at: Option<Instant>,
}

impl IdleSnapshotState {
    fn new(now: Instant) -> Self {
        Self {
            pending: true,
            retry_at: now,
            followup_at: None,
        }
    }

    fn is_pending(&self) -> bool {
        self.pending
    }

    fn deadline(&self) -> Option<Instant> {
        let pending = self.pending.then_some(self.retry_at);
        match (pending, self.followup_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn is_due(&self, now: Instant) -> bool {
        (self.is_pending() && idle_snapshot_retry_due(now, self.retry_at))
            || self
                .followup_at
                .is_some_and(|followup_at| idle_snapshot_retry_due(now, followup_at))
    }

    /// The copy only creates candidate input. Keep it pending until NVENC has
    /// produced an IDR that the main loop stores in `latest_idr`.
    fn on_snapshot_copied(&mut self, now: Instant) {
        if self.pending {
            self.retry_at = now + IDLE_SNAPSHOT_RETRY_DELAY;
        }
        self.followup_at = None;
    }

    fn on_snapshot_failed(&mut self, now: Instant) {
        if self.pending {
            self.retry_at = now + IDLE_SNAPSHOT_RETRY_DELAY;
        }
        if self.followup_at.is_some() {
            self.followup_at = Some(now + IDLE_SNAPSHOT_RETRY_DELAY);
        }
    }

    fn on_idr_cached(&mut self) {
        self.pending = false;
    }

    fn request(&mut self, now: Instant) {
        self.pending = true;
        self.retry_at = now;
    }

    fn request_followup(&mut self, now: Instant, delay: Duration) {
        self.followup_at = Some(now + delay);
    }

    fn cancel_followup(&mut self) {
        self.followup_at = None;
    }
}

/// A capture texture from DDA or the one-shot idle-desktop snapshot.
pub enum DesktopFrame<'a> {
    Direct(DxgiDuplicationFrame<'a>),
    Waited(RawDesktopFrame),
}

impl DesktopFrame<'_> {
    fn width(&self) -> u32 {
        match self {
            Self::Direct(frame) => frame.width(),
            Self::Waited(frame) => frame.width(),
        }
    }
    fn height(&self) -> u32 {
        match self {
            Self::Direct(frame) => frame.height(),
            Self::Waited(frame) => frame.height(),
        }
    }
    fn device(&self) -> &ID3D11Device {
        match self {
            Self::Direct(frame) => frame.device(),
            Self::Waited(frame) => frame.device(),
        }
    }
    fn texture(&self) -> &ID3D11Texture2D {
        match self {
            Self::Direct(frame) => frame.texture(),
            Self::Waited(frame) => frame.texture(),
        }
    }
    fn frame_info(&self) -> &windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_FRAME_INFO {
        match self {
            Self::Direct(frame) => frame.frame_info(),
            Self::Waited(frame) => frame.frame_info(),
        }
    }
    fn duplication(&self) -> &windows::Win32::Graphics::Dxgi::IDXGIOutputDuplication {
        match self {
            Self::Direct(frame) => frame.duplication(),
            Self::Waited(frame) => frame.duplication(),
        }
    }
}

pub enum CapturedFrame<'a> {
    Desktop {
        frame: DesktopFrame<'a>,
        metadata: &'a mut DamageMetadata,
        region: DesktopRegion,
    },
    IdleSnapshot {
        device: &'a ID3D11Device,
        texture: ID3D11Texture2D,
        width: u32,
        height: u32,
        region: DesktopRegion,
    },
    SecureSnapshot {
        device: &'a ID3D11Device,
        texture: ID3D11Texture2D,
        width: u32,
        height: u32,
    },
}

impl CapturedFrame<'_> {
    /// Physical bounds of the DDA output that produced this frame, including
    /// geometry changed during recreation before this frame was acquired.
    pub fn desktop_region(&self) -> Option<DesktopRegion> {
        match self {
            Self::Desktop { region, .. } | Self::IdleSnapshot { region, .. } => Some(*region),
            _ => None,
        }
    }

    pub fn is_idle_snapshot(&self) -> bool {
        matches!(self, Self::IdleSnapshot { .. })
    }

    pub fn is_secure_snapshot(&self) -> bool {
        matches!(self, Self::SecureSnapshot { .. })
    }

    pub fn width(&self) -> u32 {
        match self {
            Self::Desktop { frame, .. } => frame.width(),
            Self::IdleSnapshot { width, .. } | Self::SecureSnapshot { width, .. } => *width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::Desktop { frame, .. } => frame.height(),
            Self::IdleSnapshot { height, .. } | Self::SecureSnapshot { height, .. } => *height,
        }
    }

    pub fn device(&self) -> &ID3D11Device {
        match self {
            Self::Desktop { frame, .. } => frame.device(),
            Self::IdleSnapshot { device, .. } | Self::SecureSnapshot { device, .. } => device,
        }
    }

    pub fn texture(&self) -> &ID3D11Texture2D {
        match self {
            Self::Desktop { frame, .. } => frame.texture(),
            Self::IdleSnapshot { texture, .. } | Self::SecureSnapshot { texture, .. } => texture,
        }
    }

    pub fn damage(&self) -> Option<&[DamageRect]> {
        match self {
            Self::Desktop { metadata, .. } => metadata.valid.then_some(metadata.damage.as_slice()),
            Self::IdleSnapshot { .. } | Self::SecureSnapshot { .. } => None,
        }
    }

    /// Destinations of this frame's DDA move rects.
    pub fn moved(&self) -> &[DamageRect] {
        match self {
            Self::Desktop { metadata, .. } => &metadata.moved,
            Self::IdleSnapshot { .. } | Self::SecureSnapshot { .. } => &[],
        }
    }

    /// Fetch/process damage only after admitting this frame for encoding.
    /// Buffers live with Capture and are reused by subsequent acquisitions.
    pub fn prepare_damage(&mut self) {
        if let Self::Desktop {
            frame, metadata, ..
        } = self
        {
            metadata.prepare(frame, false);
        }
    }

    /// Preserve discarded WSS damage with one conservative union rectangle.
    /// Move destinations stay separate so scroll/motion classification is
    /// conservative too; no per-tile processing is performed here.
    pub fn prepare_damage_union(&mut self) {
        if let Self::Desktop {
            frame, metadata, ..
        } = self
        {
            metadata.prepare(frame, true);
        }
    }

    pub fn encode_png_crop(&mut self, rect: DamageRect) -> Result<Vec<u8>> {
        let Self::Desktop { frame, .. } = self else {
            bail!("lossless detail tiles require a live DDA desktop frame");
        };
        // Both frame kinds read through the device gate. The crate's buffer_crop
        // copies on the device without it, so it is not used.
        encode_texture_png_crop(frame.texture(), rect)
    }
}

pub fn encode_texture_png_crop(texture: &ID3D11Texture2D, rect: DamageRect) -> Result<Vec<u8>> {
    let mut source_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut source_desc) };
    let pixel_format = match source_desc.Format {
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => {
            ImageEncoderPixelFormat::Bgra8
        }
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => {
            ImageEncoderPixelFormat::Rgba8
        }
        other => bail!("unsupported lossless detail tile format {other:?}"),
    };
    if rect.left >= rect.right
        || rect.top >= rect.bottom
        || rect.right > source_desc.Width
        || rect.bottom > source_desc.Height
        || source_desc.SampleDesc.Count != 1
    {
        bail!("lossless detail tile is outside a single-sample texture");
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    let device = unsafe { texture.GetDevice() }.context("detail texture GetDevice")?;
    let gate = gpu_gate::for_device(&device);
    let packed = {
        // Copy, map and unmap are device work; PNG encoding is not.
        let _gpu = gate.lock();
        readback_bgra_rows(&device, texture, &source_desc, rect, width, height)?
    };
    ImageEncoder::new(ImageFormat::Png, pixel_format)
        .context("create scaled detail PNG encoder")?
        .encode(&packed, width, height)
        .context("encode scaled detail PNG")
}

/// Copies one rectangle to a staging texture and returns it packed. The caller
/// holds the device gate.
fn readback_bgra_rows(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    source_desc: &D3D11_TEXTURE2D_DESC,
    rect: DamageRect,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    let context = unsafe { device.GetImmediateContext() }.context("detail GetImmediateContext")?;
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: source_desc.Format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
        .context("CreateTexture2D(detail readback)")?;
    let staging = staging.context("CreateTexture2D(detail readback) returned no texture")?;
    let source_box = D3D11_BOX {
        left: rect.left,
        top: rect.top,
        front: 0,
        right: rect.right,
        bottom: rect.bottom,
        back: 1,
    };
    unsafe {
        context.CopySubresourceRegion(&staging, 0, 0, 0, 0, texture, 0, Some(&source_box));
    }
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
        .context("map detail texture readback")?;
    let packed = unsafe {
        let bytes = std::slice::from_raw_parts(
            mapped.pData.cast::<u8>(),
            mapped.RowPitch as usize * height as usize,
        );
        pack_bgra_rows(bytes, mapped.RowPitch as usize, width, height)
    };
    unsafe { context.Unmap(&staging, 0) };
    Ok(packed)
}

fn pack_bgra_rows(bytes: &[u8], row_pitch: usize, width: u32, height: u32) -> Vec<u8> {
    let row_bytes = width as usize * 4;
    let mut packed = Vec::with_capacity(row_bytes * height as usize);
    for row in 0..height as usize {
        let start = row * row_pitch;
        packed.extend_from_slice(&bytes[start..start + row_bytes]);
    }
    packed
}

fn clipped_damage_rect(rect: RECT, width: u32, height: u32) -> Option<DamageRect> {
    let left = rect.left.clamp(0, width as i32) as u32;
    let top = rect.top.clamp(0, height as i32) as u32;
    let right = rect.right.clamp(0, width as i32) as u32;
    let bottom = rect.bottom.clamp(0, height as i32) as u32;
    (left < right && top < bottom).then_some(DamageRect {
        left,
        top,
        right,
        bottom,
    })
}

fn move_damage_rects(
    movement: DXGI_OUTDUPL_MOVE_RECT,
    width: u32,
    height: u32,
) -> impl Iterator<Item = DamageRect> {
    let destination = movement.DestinationRect;
    let moved_width = destination.right.saturating_sub(destination.left);
    let moved_height = destination.bottom.saturating_sub(destination.top);
    let source = RECT {
        left: movement.SourcePoint.x,
        top: movement.SourcePoint.y,
        right: movement.SourcePoint.x.saturating_add(moved_width),
        bottom: movement.SourcePoint.y.saturating_add(moved_height),
    };
    [
        clipped_damage_rect(destination, width, height),
        clipped_damage_rect(source, width, height),
    ]
    .into_iter()
    .flatten()
}

/// Reusable metadata scratch for the live frame. Acquiring a frame resets its
/// flags without fetching rectangles or allocating buffers.
#[derive(Default)]
pub struct DamageMetadata {
    dirty: Vec<RECT>,
    moves: Vec<DXGI_OUTDUPL_MOVE_RECT>,
    damage: Vec<DamageRect>,
    moved: Vec<DamageRect>,
    loaded: bool,
    valid: bool,
    full: bool,
}

fn union_damage(rects: impl IntoIterator<Item = DamageRect>) -> Option<DamageRect> {
    rects.into_iter().reduce(|a, b| DamageRect {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    })
}

impl DamageMetadata {
    fn reset(&mut self) {
        self.loaded = false;
        self.valid = false;
        self.full = false;
        self.damage.clear();
        self.moved.clear();
    }

    fn load(&mut self, frame: &DesktopFrame<'_>) -> Result<()> {
        self.loaded = true;
        let bytes = frame.frame_info().TotalMetadataBufferSize as usize;
        if bytes == 0 {
            self.dirty.clear();
            self.moves.clear();
            return Ok(());
        }
        self.dirty
            .resize(bytes.div_ceil(std::mem::size_of::<RECT>()), RECT::default());
        self.moves.resize(
            bytes.div_ceil(std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>()),
            DXGI_OUTDUPL_MOVE_RECT::default(),
        );
        let mut dirty_bytes = 0;
        let mut move_bytes = 0;
        let duplication = frame.duplication();
        {
            let gate = gpu_gate::for_device(frame.device());
            let _gpu = gate.lock();
            unsafe {
                duplication.GetFrameDirtyRects(
                    (self.dirty.len() * std::mem::size_of::<RECT>()) as u32,
                    self.dirty.as_mut_ptr(),
                    &mut dirty_bytes,
                )
            }
            .context("IDXGIOutputDuplication::GetFrameDirtyRects")?;
            unsafe {
                duplication.GetFrameMoveRects(
                    (self.moves.len() * std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>()) as u32,
                    self.moves.as_mut_ptr(),
                    &mut move_bytes,
                )
            }
            .context("IDXGIOutputDuplication::GetFrameMoveRects")?;
        }
        self.dirty
            .truncate(dirty_bytes as usize / std::mem::size_of::<RECT>());
        self.moves
            .truncate(move_bytes as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>());
        Ok(())
    }

    fn prepare(&mut self, frame: &DesktopFrame<'_>, union: bool) {
        if self.full {
            return;
        }
        if !self.loaded {
            match self.load(frame) {
                Ok(()) => self.valid = true,
                Err(error) => {
                    debug!(?error, "DDA frame damage metadata unavailable");
                    return;
                }
            }
        }
        if !self.valid {
            return;
        }
        self.process(frame.width(), frame.height(), union);
    }

    fn process(&mut self, width: u32, height: u32, union: bool) {
        self.damage.clear();
        self.moved.clear();
        let damage = self
            .dirty
            .iter()
            .filter_map(|rect| clipped_damage_rect(*rect, width, height))
            .chain(
                self.moves
                    .iter()
                    .flat_map(|movement| move_damage_rects(*movement, width, height)),
            );
        if union {
            self.damage.extend(union_damage(damage));
        } else {
            self.damage.extend(damage);
        }
        // Retain individual move destinations even on discarded frames. A
        // bounding box would misclassify unchanged pixels as compositor moves.
        self.moved.extend(
            self.moves.iter().filter_map(|movement| {
                clipped_damage_rect(movement.DestinationRect, width, height)
            }),
        );
        self.full = !union;
    }
}

/// Uploads helper BGRA pixels on the interactive host's encoder device.
/// The helper never creates an encoder or supplies a D3D device to the host.
pub struct SecureFrameUpload {
    device: ID3D11Device,
    texture: Option<ID3D11Texture2D>,
    size: (u32, u32),
}

impl SecureFrameUpload {
    pub fn new(device: Option<&ID3D11Device>) -> Result<Self> {
        let device = if let Some(device) = device {
            device.clone()
        } else {
            let mut device = None;
            unsafe {
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                )
            }
            .context("create host device for secure frame upload")?;
            device.context("secure frame upload returned no host device")?
        };
        // Shared with the host encoder; protection must be on before use.
        gpu_gate::protect_device(&device)?;
        Ok(Self {
            device,
            texture: None,
            size: (0, 0),
        })
    }

    pub fn upload(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        pixels: &[u8],
    ) -> Result<CapturedFrame<'_>> {
        validate_bgra_upload(width, height, stride, pixels.len())?;
        if self.texture.is_none() || self.size != (width, height) {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            };
            let mut texture = None;
            let gate = gpu_gate::for_device(&self.device);
            let _gpu = gate.lock();
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
                .context("create host secure frame texture")?;
            self.texture = Some(texture.context("secure frame texture missing")?);
            self.size = (width, height);
        }
        let texture = self.texture.as_ref().expect("secure texture initialized");
        let gate = gpu_gate::for_device(&self.device);
        let _gpu = gate.lock();
        let context =
            unsafe { self.device.GetImmediateContext() }.context("secure upload context")?;
        unsafe { context.UpdateSubresource(texture, 0, None, pixels.as_ptr().cast(), stride, 0) };
        Ok(CapturedFrame::SecureSnapshot {
            device: &self.device,
            texture: texture.clone(),
            width,
            height,
        })
    }
}

fn validate_bgra_upload(width: u32, height: u32, stride: u32, len: usize) -> Result<()> {
    let row = width.checked_mul(4).context("BGRA row overflow")?;
    let size = (stride as usize)
        .checked_mul(height as usize)
        .context("BGRA size overflow")?;
    anyhow::ensure!(
        width > 0
            && height > 0
            && width <= 16384
            && height <= 16384
            && stride >= row
            && stride.is_multiple_of(4)
            && size == len
            && size <= 256 * 1024 * 1024,
        "invalid secure BGRA upload dimensions"
    );
    Ok(())
}

fn gdi_texture_desc(width: u32, height: u32) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32,
    }
}

/// Texture, surface and display DC survive idle-IDR retries. The DXGI target
/// DC is acquired/released for each copy as required before GPU encoding.
struct GdiSnapshot {
    texture: ID3D11Texture2D,
    surface: IDXGISurface1,
    source_dc: windows::Win32::Graphics::Gdi::HDC,
    width: u32,
    height: u32,
}

impl Drop for GdiSnapshot {
    fn drop(&mut self) {
        let _ = unsafe { DeleteDC(self.source_dc) };
    }
}

impl GdiSnapshot {
    fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        monitor_device_name: &str,
    ) -> Result<Self> {
        let desc = gdi_texture_desc(width, height);
        let mut texture = None;
        let gate = gpu_gate::for_device(device);
        let gpu = gate.lock();
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .context("CreateTexture2D(GDI capture)")?;
        drop(gpu);
        let texture = texture.context("CreateTexture2D(GDI capture) returned no texture")?;
        let surface = texture
            .cast()
            .context("cast GDI texture to IDXGISurface1")?;
        let device_name: Vec<u16> = monitor_device_name.encode_utf16().chain(Some(0)).collect();
        let source_dc = unsafe {
            CreateDCW(
                w!("DISPLAY"),
                PCWSTR(device_name.as_ptr()),
                PCWSTR::null(),
                None,
            )
        };
        if source_dc.is_invalid() {
            bail!("CreateDCW failed for {monitor_device_name}");
        }
        Ok(Self {
            texture,
            surface,
            source_dc,
            width,
            height,
        })
    }

    fn capture(&self, device: &ID3D11Device) -> Result<ID3D11Texture2D> {
        let gate = gpu_gate::for_device(device);
        copy_with_dib_fallback(
            &gate,
            || self.copy_direct(),
            || {
                capture_gdi_via_dib(
                    device,
                    &self.texture,
                    self.source_dc,
                    self.width,
                    self.height,
                )
            },
        )?;
        Ok(self.texture.clone())
    }

    /// The surface DC and BitBlt write the device's GDI-compatible texture.
    /// The caller holds the device gate.
    fn copy_direct(&self) -> Result<()> {
        let target_dc = unsafe { self.surface.GetDC(true) }.context("IDXGISurface1::GetDC")?;
        let blit = unsafe {
            BitBlt(
                target_dc,
                0,
                0,
                self.width as i32,
                self.height as i32,
                Some(self.source_dc),
                0,
                0,
                SRCCOPY | CAPTUREBLT,
            )
        };
        let release = unsafe { self.surface.ReleaseDC(None) };
        blit.context("BitBlt desktop into GDI texture")?;
        release.context("IDXGISurface1::ReleaseDC")?;
        Ok(())
    }
}

/// Runs `direct` under the device gate. When it fails, the gate is released
/// before `fallback` runs: the fallback takes the same gate for its upload,
/// and the gate panics on same-thread re-entry.
fn copy_with_dib_fallback(
    gate: &gpu_gate::GpuGate,
    direct: impl FnOnce() -> Result<()>,
    fallback: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let direct_result = {
        let _gpu = gate.lock();
        direct()
    };
    match direct_result {
        Ok(()) => Ok(()),
        Err(direct_error) => {
            warn!(
                ?direct_error,
                "direct GDI/DXGI capture failed; using CPU DIB fallback"
            );
            fallback()
        }
    }
}

/// Some secure-desktop display drivers reject `IDXGISurface1::GetDC` even
/// though ordinary GDI capture still works. Capture into a top-down 32-bit
/// DIB and upload it once to the same GPU-resident texture used by NVENC.
fn capture_gdi_via_dib(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    source_dc: windows::Win32::Graphics::Gdi::HDC,
    width: u32,
    height: u32,
) -> Result<()> {
    let memory_dc = unsafe { CreateCompatibleDC(Some(source_dc)) };
    if memory_dc.is_invalid() {
        bail!("CreateCompatibleDC failed for secure capture fallback");
    }
    let result = (|| -> Result<()> {
        let mut bits = std::ptr::null_mut::<c_void>();
        let bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: width.saturating_mul(height).saturating_mul(4),
                ..Default::default()
            },
            ..Default::default()
        };
        let bitmap = unsafe {
            CreateDIBSection(
                Some(memory_dc),
                &bitmap_info,
                DIB_RGB_COLORS,
                &mut bits,
                None::<HANDLE>,
                0,
            )
        }
        .context("CreateDIBSection(secure capture fallback)")?;
        if bits.is_null() {
            let _ = unsafe { DeleteObject(bitmap.into()) };
            bail!("CreateDIBSection returned no pixel storage");
        }
        let previous = unsafe { SelectObject(memory_dc, bitmap.into()) };
        if previous.is_invalid() {
            let _ = unsafe { DeleteObject(bitmap.into()) };
            bail!("SelectObject failed for secure capture fallback");
        }
        let blit = unsafe {
            BitBlt(
                memory_dc,
                0,
                0,
                width as i32,
                height as i32,
                Some(source_dc),
                0,
                0,
                SRCCOPY | CAPTUREBLT,
            )
        };
        let upload = match blit {
            Ok(()) => {
                let gate = gpu_gate::for_device(device);
                let _gpu = gate.lock();
                let context = unsafe { device.GetImmediateContext() }
                    .context("get D3D11 context for secure capture fallback")?;
                unsafe {
                    context.UpdateSubresource(texture, 0, None, bits, width.saturating_mul(4), 0)
                };
                Ok(())
            }
            Err(error) => Err(anyhow::anyhow!(
                "BitBlt into secure capture DIB failed: {}",
                error
            )),
        };
        let _ = unsafe { SelectObject(memory_dc, previous) };
        let _ = unsafe { DeleteObject(bitmap.into()) };
        upload
    })();
    let _ = unsafe { DeleteDC(memory_dc) };
    result
}

/// Resolve the selected display's adapter without creating capture resources
/// or waiting for Desktop Duplication to produce a frame.
pub fn monitor_adapter(filter: Option<&str>) -> Result<IDXGIAdapter1> {
    let device_name = pick_monitor(filter)?.device_name()?;
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let mut adapter_index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_index) } {
        let mut output_index = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
            let desc = unsafe { output.GetDesc() }?;
            let length = desc
                .DeviceName
                .iter()
                .position(|&v| v == 0)
                .unwrap_or(desc.DeviceName.len());
            if String::from_utf16_lossy(&desc.DeviceName[..length])
                .eq_ignore_ascii_case(&device_name)
            {
                return Ok(adapter);
            }
            output_index += 1;
        }
        adapter_index += 1;
    }
    bail!("selected display {device_name} has no DXGI adapter")
}

/// Resolve a monitor from an optional filter: `None` or `"primary"` picks the
/// primary display; a numeric string picks the 1-based `Monitor` index;
/// anything else is a case-insensitive substring match against the device name
/// (`\\.\DISPLAY2`) or friendly name (`ParsecVDA`).
pub fn pick_monitor(filter: Option<&str>) -> Result<Monitor> {
    let Some(f) = filter else {
        return Monitor::primary().context("primary monitor");
    };
    let f = f.trim();
    if f.eq_ignore_ascii_case("primary") {
        return Monitor::primary().context("primary monitor");
    }
    if let Ok(idx) = f.parse::<usize>() {
        if let Ok(m) = Monitor::from_index(idx) {
            return Ok(m);
        }
    }
    let monitors = Monitor::enumerate().context("enumerate monitors")?;
    let needle = f.to_ascii_lowercase();
    for m in &monitors {
        let dev = m.device_name().unwrap_or_default();
        let name = m.name().unwrap_or_default();
        if dev.to_ascii_lowercase().contains(&needle) || name.to_ascii_lowercase().contains(&needle)
        {
            return Ok(*m);
        }
    }
    let listing: Vec<String> = monitors
        .iter()
        .map(|m| {
            format!(
                "{} ({})",
                m.device_name().unwrap_or_default(),
                m.name().unwrap_or_default()
            )
        })
        .collect();
    bail!(
        "no monitor matching {f:?}; available: {}",
        listing.join(", ")
    )
}

pub fn enumerate_monitors(selected: Option<&str>) -> Result<Vec<MonitorInfo>> {
    let monitors = Monitor::enumerate().context("enumerate monitors")?;
    let selected = selected.unwrap_or("primary");
    monitors
        .iter()
        .map(|monitor| {
            let selector = monitor.device_name().unwrap_or_default();
            let name = monitor
                .name()
                .or_else(|_| monitor.device_string())
                .unwrap_or_else(|_| selector.clone());
            let width = monitor.width().unwrap_or_default();
            let height = monitor.height().unwrap_or_default();
            let region =
                monitor_region(*monitor).unwrap_or(DesktopRegion::new(0, 0, width, height));
            let is_selected = selector.eq_ignore_ascii_case(selected)
                || selected.eq_ignore_ascii_case("primary")
                    && *monitor == Monitor::primary().unwrap_or(*monitor);
            Ok(MonitorInfo {
                selector,
                name,
                width,
                height,
                origin_x: region.x,
                origin_y: region.y,
                selected: is_selected,
            })
        })
        .collect()
}

pub fn monitor_region(monitor: Monitor) -> Result<DesktopRegion> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(HMONITOR(monitor.as_raw_hmonitor()), &raw mut info) }.as_bool() {
        bail!("GetMonitorInfoW failed for capture monitor");
    }
    let width = info
        .rcMonitor
        .right
        .saturating_sub(info.rcMonitor.left)
        .max(1) as u32;
    let height = info
        .rcMonitor
        .bottom
        .saturating_sub(info.rcMonitor.top)
        .max(1) as u32;
    Ok(DesktopRegion::new(
        info.rcMonitor.left,
        info.rcMonitor.top,
        width,
        height,
    ))
}

impl Capture {
    /// Create a duplication session. `monitor_filter` selects the target
    /// display (see [`pick_monitor`]); `None` captures the primary monitor.
    pub fn new(monitor_filter: Option<&str>) -> Result<Self> {
        let monitor = pick_monitor(monitor_filter)?;
        let region = monitor_region(monitor).context("selected monitor bounds")?;
        let dev = monitor.device_name().unwrap_or_default();
        let mut creation_reason = None;
        let api = match DxgiDuplicationApi::new(monitor) {
            Ok(api) => Some(api),
            Err(error) if capture_error_is_unavailable(&error) => {
                warn!(?error, monitor = %dev, "DDA creation unavailable; preserving session");
                creation_reason = Some(dda_error_reason(&error));
                None
            }
            Err(error) => return Err(error).context("DxgiDuplicationApi::new"),
        };
        let unavailable = api.is_none();
        let width = api.as_ref().map_or(region.width, DxgiDuplicationApi::width);
        let height = api
            .as_ref()
            .map_or(region.height, DxgiDuplicationApi::height);
        if let Some(api) = api.as_ref() {
            gpu_gate::protect_device(api.device())?;
        }
        let device_obj = api.as_ref().map(|api| api.device().clone());
        info!(width, height, monitor = %dev, unavailable, "DDA source initialized");
        Ok(Self {
            api,
            width,
            height,
            origin_x: region.x,
            origin_y: region.y,
            frames_acquired: 0,
            frames_new: 0,
            last_present_qpc: 0,
            device_obj,
            pending_recreate: unavailable,
            next_recreate_attempt: Instant::now()
                + if unavailable {
                    CAPTURE_RECOVERY_INTERVAL
                } else {
                    Duration::ZERO
                },
            awaiting_recovery_frame: false,
            fresh_recreate_required: false,
            recreate_failed: unavailable,
            // Pin the selected physical output; "primary" may resolve to a
            // different monitor after a desktop/topology transition.
            monitor_filter: Some(dev.clone()),
            monitor_device_name: dev,
            idle_snapshot: IdleSnapshotState::new(Instant::now()),
            idle_gdi: None,
            damage_metadata: DamageMetadata::default(),
            desktop_check: DesktopCheck::new(Instant::now()),
            event_activity: None,
            waiter: None,
            invalidation: None,
            availability: if unavailable {
                CaptureAvailability::Unavailable
            } else {
                CaptureAvailability::Active
            },
            availability_transition: unavailable.then_some(CaptureAvailability::Unavailable),
            unavailable_reason: creation_reason,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn region(&self) -> DesktopRegion {
        DesktopRegion::new(self.origin_x, self.origin_y, self.width, self.height)
    }

    pub fn monitor_selector(&self) -> &str {
        self.monitor_filter.as_deref().unwrap_or("primary")
    }

    pub fn monitors(&self) -> Result<Vec<MonitorInfo>> {
        enumerate_monitors(Some(self.monitor_selector()))
    }

    /// Replace the duplication session only after the new monitor has been
    /// opened successfully, leaving the current stream intact on failure.
    pub fn switch_monitor(&mut self, selector: &str) -> Result<()> {
        if self.availability == CaptureAvailability::Unavailable {
            bail!("capture unavailable; monitor selection is suspended until recovery");
        }
        let monitor = pick_monitor(Some(selector))?;
        let region = monitor_region(monitor).context("selected monitor bounds")?;
        let dev = monitor.device_name().unwrap_or_default();
        self.stop_waiter()?;
        if monitor_switch_plan(&self.monitor_device_name, &dev)
            == MonitorSwitchPlan::RecreateCurrent
        {
            // DuplicateOutput can reject a second live duplication object for
            // the same ParsecVDA output with E_INVALIDARG. The recovery path
            // consumes the current API first, then recreates it on the exact
            // same selector, which is also the required ownership order for a
            // silently stalled (timeout-only) duplication session.
            self.monitor_filter = Some(dev.clone());
            if let Err(error) = self.recreate(false) {
                // Recreate consumes the old duplication even when it fails.
                self.mark_unavailable(recreate_failure_reason(&error));
                return Err(error).context("recreate current monitor duplication");
            }
            self.pending_recreate = false;
            self.next_recreate_attempt = Instant::now();
            self.awaiting_recovery_frame = false;
            self.fresh_recreate_required = false;
            self.recreate_failed = false;
            self.availability = CaptureAvailability::Active;
            self.unavailable_reason = None;
            info!(
                width = self.width,
                height = self.height,
                monitor = %self.monitor_device_name,
                "DDA current monitor reselected"
            );
            return Ok(());
        }
        let api = match DxgiDuplicationApi::new(monitor) {
            Ok(api) => api,
            Err(error) if capture_error_is_unavailable(&error) => {
                self.mark_unavailable(dda_error_reason(&error));
                return Err(error).context("DxgiDuplicationApi::new monitor switch unavailable");
            }
            Err(error) => return Err(error).context("DxgiDuplicationApi::new monitor switch"),
        };
        self.width = api.width();
        self.height = api.height();
        self.origin_x = region.x;
        self.origin_y = region.y;
        gpu_gate::protect_device(api.device())?;
        self.device_obj = Some(api.device().clone());
        self.last_present_qpc = 0;
        self.monitor_filter = Some(dev.clone());
        self.monitor_device_name = dev;
        retire_api(self.api.replace(api));
        self.pending_recreate = false;
        self.next_recreate_attempt = Instant::now();
        self.awaiting_recovery_frame = false;
        self.fresh_recreate_required = false;
        self.recreate_failed = false;
        self.idle_snapshot = IdleSnapshotState::new(Instant::now());
        self.idle_gdi = None;
        self.desktop_check = DesktopCheck::new(Instant::now());
        self.availability = CaptureAvailability::Active;
        self.unavailable_reason = None;
        info!(width = self.width, height = self.height, monitor = %self.monitor_device_name, "DDA monitor switched");
        Ok(())
    }

    /// The D3D11 device NVENC must bind to (pinned from the first frame).
    pub fn device(&self) -> Option<&ID3D11Device> {
        self.device_obj.as_ref()
    }

    pub fn take_availability_transition(&mut self) -> Option<CaptureAvailability> {
        self.availability_transition.take()
    }

    pub fn availability(&self) -> CaptureAvailability {
        self.availability
    }

    /// Why the source is unavailable, or `None` when it is active or no cause
    /// was recorded. Reads only cached state and one display-power flag; it
    /// makes no capture, desktop or power calls.
    pub fn unavailable_reason(&self) -> Option<CaptureUnavailableReason> {
        reported_unavailable_reason(
            self.availability,
            self.display_is_off(),
            self.unavailable_reason,
        )
    }

    fn display_is_off(&self) -> bool {
        self.invalidation
            .as_ref()
            .is_some_and(CaptureInvalidation::display_is_off)
    }

    /// A short join grace may hide a transient loss, but never a failed
    /// recreation. This is a recovery hint, not authorization for pixels.
    pub fn can_recover_for_join(&self) -> bool {
        !self
            .invalidation
            .as_ref()
            .is_some_and(CaptureInvalidation::display_is_off)
            && (self.availability == CaptureAvailability::Active
                || (!self.recreate_failed
                    && (self.pending_recreate || self.awaiting_recovery_frame)))
    }

    /// Healthy parked capture has no maintenance timer. Windows display,
    /// power and desktop notifications wake its owner when invalidated.
    pub fn parked_maintenance_deadline(&self) -> Option<Instant> {
        self.pending_recreate.then_some(self.next_recreate_attempt)
    }

    /// Recover a known lost duplication without requesting desktop frames.
    /// A replacement remains unavailable until an authorized frame or GDI
    /// snapshot verifies it on viewer join.
    pub fn maintain_parked(&mut self, now: Instant) -> Result<()> {
        self.set_capture_demand(false);
        if let Some(waiter) = self.waiter.as_ref() {
            match waiter.poll() {
                Ok(frame) => drop(frame),
                Err(error) if capture_error_is_unavailable(&error) => {
                    self.mark_unavailable(dda_error_reason(&error));
                    self.next_recreate_attempt = now;
                }
                Err(error) => return Err(anyhow::anyhow!("parked capture waiter: {error:?}")),
            }
        }
        self.consume_invalidation(now);
        if !recovery_attempt_due(self.pending_recreate, now, self.next_recreate_attempt) {
            return Ok(());
        }
        // Never open duplication on a secure desktop, even if the event was
        // queued on Default or the source's cached desktop state is stale.
        let on_default = self.desktop_check.refresh(true);
        if !on_default {
            self.unavailable_reason = Some(CaptureUnavailableReason::SecureDesktop);
        }
        if !on_default
            || self
                .invalidation
                .as_ref()
                .is_some_and(CaptureInvalidation::display_is_off)
        {
            self.next_recreate_attempt = now + PARKED_RECOVERY_INTERVAL;
            return Ok(());
        }
        self.attempt_recovery(now, PARKED_RECOVERY_INTERVAL);
        Ok(())
    }

    fn consume_invalidation(&mut self, now: Instant) {
        if self
            .invalidation
            .as_ref()
            .is_some_and(CaptureInvalidation::take)
        {
            self.mark_unavailable(CaptureUnavailableReason::DisplayChanged);
            self.next_recreate_attempt = now;
            self.fresh_recreate_required = true;
            self.awaiting_recovery_frame = false;
            self.idle_gdi = None;
        }
    }

    fn attempt_recovery(&mut self, now: Instant, retry_delay: Duration) -> bool {
        if !recovery_attempt_due(self.pending_recreate, now, self.next_recreate_attempt) {
            return !self.pending_recreate;
        }
        if self
            .invalidation
            .as_ref()
            .is_some_and(CaptureInvalidation::display_is_off)
        {
            self.next_recreate_attempt = now + retry_delay;
            return false;
        }
        match self.recreate(self.fresh_recreate_required) {
            Ok(()) => {
                self.record_recreated();
                true
            }
            Err(error) => {
                self.record_recreate_failed(now, retry_delay, &error);
                debug!(
                    ?error,
                    retry_ms = retry_delay.as_millis(),
                    "DDA recreate still unavailable"
                );
                false
            }
        }
    }

    /// A replacement duplication exists but has not yet produced pixels.
    fn record_recreated(&mut self) {
        self.pending_recreate = false;
        self.awaiting_recovery_frame = true;
        self.fresh_recreate_required = false;
        self.recreate_failed = false;
        self.availability =
            availability_after(self.availability, CaptureRecoveryEvent::SessionRecreated);
        self.unavailable_reason = Some(CaptureUnavailableReason::RecoveryAwaitingFrame);
    }

    fn record_recreate_failed(
        &mut self,
        now: Instant,
        retry_delay: Duration,
        error: &anyhow::Error,
    ) {
        self.next_recreate_attempt = now + retry_delay;
        self.fresh_recreate_required = true;
        self.recreate_failed = true;
        self.unavailable_reason = Some(recreate_failure_reason(error));
    }

    /// The helper observed Winlogon before DDA reported its own access loss.
    /// Keep the selected output/device, but never reuse its desktop pixels.
    pub fn suspend_for_secure_desktop(&mut self) {
        if let Err(error) = self.stop_waiter() {
            warn!(%error, "capture waiter stop during secure transition failed");
        }
        self.idle_gdi = None;
        retire_api(self.api.take());
        self.mark_unavailable(CaptureUnavailableReason::SecureDesktop);
        self.fresh_recreate_required = true;
        self.awaiting_recovery_frame = false;
        self.idle_snapshot = IdleSnapshotState::new(Instant::now());
    }

    fn mark_unavailable(&mut self, reason: CaptureUnavailableReason) {
        if self.availability != CaptureAvailability::Unavailable {
            self.recreate_failed = false;
        }
        self.pending_recreate = true;
        self.next_recreate_attempt = Instant::now() + CAPTURE_RECOVERY_INTERVAL;
        self.availability = CaptureAvailability::Unavailable;
        self.availability_transition = Some(CaptureAvailability::Unavailable);
        self.unavailable_reason = Some(reason);
    }

    /// Enable before the first acquisition so this serve instance can poll
    /// completed frames without blocking signaling or input processing.
    pub fn enable_event_capture(&mut self, activity: Activity) {
        if let Some(invalidation) = self.invalidation.as_ref() {
            invalidation.set_activity(activity.clone());
        } else {
            match CaptureInvalidation::new(activity.clone()) {
                Ok(invalidation) => self.invalidation = Some(invalidation),
                Err(error) => warn!(%error, "capture display/power notifications unavailable"),
            }
        }
        self.event_activity = Some(activity);
    }

    pub fn request_frame(&mut self) -> Result<()> {
        self.request_frame_at(Instant::now())
    }

    pub fn request_frame_at(&mut self, not_before: Instant) -> Result<()> {
        let Some(activity) = self.event_activity.as_ref() else {
            return Ok(());
        };
        if self.pending_recreate {
            return Ok(());
        }
        if self.waiter.is_none() {
            let Some(api) = self.api.as_ref() else {
                return Ok(());
            };
            self.waiter = Some(DdaWaiter::new(
                api.duplication().clone(),
                api.device().clone(),
                activity.clone(),
            )?);
        }
        let waiter = self.waiter.as_mut().expect("waiter initialized");
        waiter.set_enabled(true);
        waiter.request_frame_at(not_before);
        Ok(())
    }

    pub fn set_capture_demand(&mut self, enabled: bool) {
        if let Some(waiter) = self.waiter.as_mut() {
            waiter.set_enabled(enabled);
        }
    }

    fn stop_waiter(&mut self) -> Result<()> {
        if let Some(mut waiter) = self.waiter.take() {
            waiter.stop()?;
        }
        Ok(())
    }

    pub fn idle_snapshot_deadline(&self) -> Option<Instant> {
        self.idle_snapshot.deadline()
    }

    /// Acquire the next frame, blocking up to `timeout` in synchronous mode
    /// or polling the outstanding waiter request in event mode.
    ///
    /// `Ok(None)` on timeout (desktop idle). Access-lost and access-denied
    /// enter an unavailable circuit with deferred 500 ms recreation attempts,
    /// returning `Ok(None)` while signaling and media sockets stay alive.
    /// The returned tuple is `(frame, is_new_present, recovered)`. `recovered`
    /// is true only for the first real DDA frame after an unavailable period.
    pub fn acquire<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> Result<Option<(CapturedFrame<'a>, bool, bool)>> {
        let now = Instant::now();
        self.consume_invalidation(now);
        let recovery_check = self.event_activity.is_none()
            || self.awaiting_recovery_frame
            || (self.pending_recreate && now >= self.next_recreate_attempt);
        if !self.desktop_check.refresh(recovery_check) {
            if self.availability != CaptureAvailability::Unavailable {
                self.suspend_for_secure_desktop();
                warn!("input desktop is not Default; normal capture suspended");
            }
            self.unavailable_reason = Some(CaptureUnavailableReason::SecureDesktop);
            return Ok(None);
        }
        // Deferred recovery from a previous AccessLost (see field doc).
        if self.pending_recreate && !self.attempt_recovery(now, CAPTURE_RECOVERY_INTERVAL) {
            return Ok(None);
        }
        // Never sleep across the next desktop-switch check deadline.
        let timeout = timeout.min(
            self.desktop_check
                .next_check
                .saturating_duration_since(Instant::now()),
        );
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let acquired = if self.event_activity.is_some() {
            match self.waiter.as_mut() {
                Some(waiter) => waiter.poll().map(|frame| frame.map(DesktopFrame::Waited)),
                None => Ok(None),
            }
        } else {
            let api = self
                .api
                .as_mut()
                .context("DDA session temporarily unavailable")?;
            // The crate's call also ReleaseFrames the previous frame, so the
            // whole call is one gated section, bounded to the 16 ms request.
            let gate = gpu_gate::for_device(api.device());
            let _gpu = gate.lock();
            api.acquire_next_frame(timeout_ms.min(DIRECT_ACQUIRE_MAX_MS))
                .map(|frame| Some(DesktopFrame::Direct(frame)))
        };
        let frame = match acquired {
            Ok(Some(f)) => f,
            Ok(None) => return Ok(None),
            Err(CaptureError::Timeout) => return Ok(None),
            Err(error) if capture_error_is_unavailable(&error) => {
                let on_default = self.desktop_check.refresh(true);
                self.unavailable_reason = Some(if on_default {
                    dda_error_reason(&error)
                } else {
                    CaptureUnavailableReason::SecureDesktop
                });
                self.idle_gdi = None;
                let first_loss = self.availability != CaptureAvailability::Unavailable;
                if first_loss {
                    warn!(
                        reason = %self.unavailable_reason.expect("acquire loss records its cause"),
                        "DDA access lost; capture unavailable until a replacement yields a frame"
                    );
                } else {
                    debug!("replacement DDA session lost access before producing a frame");
                }
                // Disjoint field write; `self.api` is borrowed for 'a by the
                // (absent) Ok frame, so no `&mut self` calls here.
                self.pending_recreate = true;
                // The first join acquisition can discover a loss that was
                // silent while parked. Try its replacement on the next poll,
                // rather than adding the normal failed-retry backoff.
                self.next_recreate_attempt = Instant::now();
                self.recreate_failed = self.awaiting_recovery_frame;
                if self.awaiting_recovery_frame {
                    self.fresh_recreate_required = true;
                }
                if first_loss {
                    self.availability =
                        availability_after(self.availability, CaptureRecoveryEvent::AccessLost);
                    self.availability_transition = Some(CaptureAvailability::Unavailable);
                }
                return Ok(None);
            }
            Err(e) => {
                self.desktop_check.refresh(true);
                return Err(anyhow::anyhow!("acquire_next_frame: {e:?}"));
            }
        };

        // A frame may have queued on Default before Windows switched to UAC
        // or lock. Always query the current input desktop before exposing its
        // texture; the active timer cache cannot authorize a real frame.
        // The waiter also checks immediately before its actual acquisition.
        if !self.desktop_check.refresh(true) {
            drop(frame);
            // self.api is borrowed for 'a by the absent returned frame. Update
            // disjoint recovery fields; replace the API only on the next call.
            if self.availability != CaptureAvailability::Unavailable {
                self.availability_transition = Some(CaptureAvailability::Unavailable);
                warn!("discarded DDA frame from a non-Default input desktop");
            }
            self.availability = CaptureAvailability::Unavailable;
            self.unavailable_reason = Some(CaptureUnavailableReason::SecureDesktop);
            self.pending_recreate = true;
            self.fresh_recreate_required = true;
            self.awaiting_recovery_frame = false;
            self.next_recreate_attempt = Instant::now() + CAPTURE_RECOVERY_INTERVAL;
            self.idle_gdi = None;
            return Ok(None);
        }

        // Keep the capture device pinned for diagnostics and AccessLost
        // recreation. It is also available before this first DDA frame.
        if self.device_obj.is_none() {
            self.device_obj = Some(frame.device().clone());
            info!("pinned capture D3D11 device for NVENC session");
        }

        let info = frame.frame_info();
        self.frames_acquired += 1;
        // DXGI_OUTDUPL_FRAME_INFO::LastPresentTime is a LARGE_INTEGER, which
        // the windows crate projects as a plain i64 (its QuadPart).
        let present_qpc = info.LastPresentTime;
        // New desktop present iff the present timestamp advanced. Cursor-only
        // wakes keep the timestamp and report is_new=false.
        let is_new = present_qpc != self.last_present_qpc && present_qpc != 0;
        if is_new {
            self.frames_new += 1;
            self.last_present_qpc = present_qpc;
        }

        // A successful recreate alone is not evidence that the desktop is
        // capturable. Only this real DDA frame closes the recovery circuit.
        let recovered = self.availability == CaptureAvailability::Unavailable;
        if recovered {
            self.availability =
                availability_after(self.availability, CaptureRecoveryEvent::FrameAcquired);
            self.unavailable_reason = None;
            self.awaiting_recovery_frame = false;
            self.fresh_recreate_required = false;
            self.recreate_failed = false;
            info!("DDA capture produced a verified recovery frame");
            self.availability_transition = None;
        }

        self.damage_metadata.reset();
        Ok(Some((
            CapturedFrame::Desktop {
                frame,
                metadata: &mut self.damage_metadata,
                region: DesktopRegion::new(self.origin_x, self.origin_y, self.width, self.height),
            },
            is_new,
            recovered,
        )))
    }

    /// Return a one-shot GPU-resident snapshot of the selected output when
    /// DDA has not produced an initial desktop texture. Failure is nonfatal
    /// and retried after a short backoff.
    pub fn take_idle_snapshot_frame(&mut self) -> Option<CapturedFrame<'_>> {
        self.take_idle_snapshot_frame_with_recovery()
            .map(|(frame, _)| frame)
    }

    /// A successfully recreated session may verify its recovery with an
    /// immediate GDI snapshot, even when the static desktop never presents.
    /// Failed recreation and secure desktops still cannot supply snapshots.
    pub fn take_idle_snapshot_frame_with_recovery(&mut self) -> Option<(CapturedFrame<'_>, bool)> {
        let now = Instant::now();
        self.consume_invalidation(now);
        if self.availability == CaptureAvailability::Unavailable
            && !snapshot_can_verify_recovery(
                self.pending_recreate,
                self.awaiting_recovery_frame,
                self.recreate_failed,
                self.api.is_some(),
            )
        {
            return None;
        }
        if !self.idle_snapshot.is_due(now) {
            return None;
        }
        if self
            .invalidation
            .as_ref()
            .is_some_and(CaptureInvalidation::display_is_off)
        {
            return None;
        }
        if !self.desktop_check.refresh(true) {
            self.suspend_for_secure_desktop();
            return None;
        }
        match self.capture_idle_snapshot() {
            Ok(texture) => {
                if self
                    .invalidation
                    .as_ref()
                    .is_some_and(CaptureInvalidation::display_is_off)
                {
                    drop(texture);
                    self.consume_invalidation(now);
                    return None;
                }
                if !self.desktop_check.refresh(true) {
                    drop(texture);
                    self.suspend_for_secure_desktop();
                    warn!("discarded idle snapshot after input desktop left Default");
                    return None;
                }
                self.idle_snapshot.on_snapshot_copied(now);
                let recovered = self.availability == CaptureAvailability::Unavailable;
                if recovered {
                    self.availability = CaptureAvailability::Active;
                    self.unavailable_reason = None;
                    self.awaiting_recovery_frame = false;
                    self.fresh_recreate_required = false;
                    self.recreate_failed = false;
                    self.availability_transition = None;
                    info!("normal capture produced an authorized GDI recovery snapshot");
                }
                debug!(
                    width = self.width,
                    height = self.height,
                    monitor = %self.monitor_device_name,
                    "captured idle-desktop snapshot frame"
                );
                Some((
                    CapturedFrame::IdleSnapshot {
                        device: self.api.as_ref().expect("DDA session present").device(),
                        texture,
                        width: self.width,
                        height: self.height,
                        region: DesktopRegion::new(
                            self.origin_x,
                            self.origin_y,
                            self.width,
                            self.height,
                        ),
                    },
                    recovered,
                ))
            }
            Err(e) => {
                if self.awaiting_recovery_frame {
                    self.recreate_failed = true;
                    self.pending_recreate = true;
                    self.fresh_recreate_required = true;
                    self.next_recreate_attempt = now + CAPTURE_RECOVERY_INTERVAL;
                    self.unavailable_reason = Some(recreate_failure_reason(&e));
                }
                self.idle_snapshot.on_snapshot_failed(now);
                warn!(
                    ?e,
                    retry_ms = IDLE_SNAPSHOT_RETRY_DELAY.as_millis(),
                    "idle-desktop snapshot failed — will retry"
                );
                None
            }
        }
    }

    /// Mark the idle snapshot complete only once its encoded IDR is available
    /// for replay. Until then encoder/scaler failures retry the snapshot.
    pub fn acknowledge_idle_snapshot_idr(&mut self) {
        self.idle_snapshot.on_idr_cached();
    }

    /// Capture watchdog: the duplication delivered nothing while a frame was
    /// needed, without reporting an error (DXGI WAIT_TIMEOUT is silent). Send it
    /// through the normal loss path: unavailable now, recreated on the next
    /// acquire, active again on its first verified frame, which rebuilds the
    /// encoder and so starts with an IDR. Returns false while a recovery is
    /// already pending.
    pub fn recreate_stalled_duplication(&mut self, now: Instant) -> bool {
        if self.pending_recreate {
            return false;
        }
        self.mark_unavailable(CaptureUnavailableReason::RecoveryAwaitingFrame);
        self.next_recreate_attempt = now;
        if let Some(activity) = self.event_activity.as_ref() {
            activity.notify();
        }
        true
    }

    /// Re-arm the GPU-resident desktop snapshot path for a forced keyframe.
    /// A static desktop may never produce another DDA present, so clearing the
    /// cached IDR must always be paired with an explicit snapshot request.
    pub fn request_idle_snapshot(&mut self) {
        self.idle_snapshot.request(Instant::now());
        if let Some(activity) = self.event_activity.as_ref() {
            activity.notify();
        }
    }

    /// Schedule one idle capture after a grace period so a deferred GPU
    /// verdict can be collected if DDA produces no following desktop frame.
    /// Unlike recovery snapshots, this wake is consumed by one copied frame
    /// and never waits for an encoded IDR.
    pub fn request_idle_followup_snapshot(&mut self, delay: Duration) {
        self.idle_snapshot.request_followup(Instant::now(), delay);
        if delay.is_zero() {
            if let Some(activity) = self.event_activity.as_ref() {
                activity.notify();
            }
        }
    }

    pub fn cancel_idle_followup_snapshot(&mut self) {
        self.idle_snapshot.cancel_followup();
    }

    fn capture_idle_snapshot(&mut self) -> Result<ID3D11Texture2D> {
        let api = self
            .api
            .as_ref()
            .context("DDA session temporarily unavailable")?;
        let output = unsafe { api.output().GetDesc() }.context("IDXGIOutput::GetDesc")?;
        if !supports_idle_snapshot_rotation(output.Rotation) {
            bail!(
                "idle-desktop snapshot does not support rotated output {:?}",
                output.Rotation
            );
        }
        if self.idle_gdi.is_none() {
            self.idle_gdi = Some(GdiSnapshot::new(
                api.device(),
                self.width,
                self.height,
                &self.monitor_device_name,
            )?);
        }
        self.idle_gdi
            .as_ref()
            .expect("idle GDI initialized")
            .capture(api.device())
    }

    fn recreate(&mut self, force_fresh: bool) -> Result<()> {
        if let Err(error) = self.stop_waiter() {
            // A release can discover the same invalidation after the owner's
            // last poll. The joined waiter has dropped its duplication refs;
            // access loss is expected here and must not defer replacement.
            // Live-frame ownership, panics and IO failures still abort it.
            if error
                .downcast_ref::<CaptureError>()
                .is_some_and(capture_error_is_unavailable)
            {
                debug!(%error, "lost duplication reported during waiter shutdown");
            } else {
                return Err(error);
            }
        }
        self.idle_gdi = None;
        self.desktop_check = DesktopCheck::new(Instant::now());
        // The crate's recovery API drops IDXGIOutputDuplication before asking
        // DXGI for its replacement and preserves the D3D11 device. If that
        // fails after a larger display/topology transition, `old` has still
        // been consumed, so a completely fresh session cannot conflict with
        // the invalid handle. `None` is a valid retry state when both attempts
        // failed on the previous pass (for example during secure desktop).
        let old_api = self.api.take();
        let api = match (force_fresh, old_api) {
            (true, old) => {
                retire_api(old);
                let monitor = pick_monitor(self.monitor_filter.as_deref())?;
                DxgiDuplicationApi::new(monitor)
                    .context("DxgiDuplicationApi fresh-session recreate")?
            }
            (false, Some(old)) => match recreate_in_place(old) {
                Ok(api) => api,
                Err(in_place_error) => {
                    warn!(
                        ?in_place_error,
                        "in-place DDA recreate failed; opening a fresh session"
                    );
                    let monitor = pick_monitor(self.monitor_filter.as_deref())?;
                    DxgiDuplicationApi::new(monitor).context("DxgiDuplicationApi fresh recreate")?
                }
            },
            (false, None) => {
                let monitor = pick_monitor(self.monitor_filter.as_deref())?;
                DxgiDuplicationApi::new(monitor).context("DxgiDuplicationApi retry recreate")?
            }
        };
        self.width = api.width();
        self.height = api.height();
        let monitor = pick_monitor(self.monitor_filter.as_deref())?;
        let region = monitor_region(monitor).context("recreated monitor bounds")?;
        self.origin_x = region.x;
        self.origin_y = region.y;
        // The recreated API has its own device; use it for a possible idle
        // snapshot before the next DDA frame arrives.
        gpu_gate::protect_device(api.device())?;
        self.device_obj = Some(api.device().clone());
        self.last_present_qpc = 0;
        self.monitor_device_name = monitor.device_name().unwrap_or_default();
        self.api = Some(api);
        self.idle_snapshot = IdleSnapshotState::new(Instant::now());
        info!(
            width = self.width,
            height = self.height,
            "DDA session recreated"
        );
        Ok(())
    }

    pub fn stats(&self) -> (u64, u64) {
        (self.frames_acquired, self.frames_new)
    }
}

// Join the acquisition worker before field teardown drops the duplication.
impl Drop for Capture {
    fn drop(&mut self) {
        if let Err(error) = self.stop_waiter() {
            warn!(%error, "capture waiter stop failed during drop");
        }
        retire_api(self.api.take());
    }
}

/// In-place recreate drops the duplication and re-duplicates on the same device,
/// so it runs under that device's gate.
fn recreate_in_place(old: DxgiDuplicationApi) -> Result<DxgiDuplicationApi, CaptureError> {
    let gate = gpu_gate::for_device(old.device());
    let _gpu = gate.lock();
    old.recreate()
}

/// Drops a duplication session and its device under that device's gate. Releasing
/// the duplication is native work on the device that NVENC may be using. Callers
/// must have stopped the waiter first, so no worker still owns the duplication.
fn retire_api(api: Option<DxgiDuplicationApi>) {
    let Some(api) = api else { return };
    let gate = gpu_gate::for_device(api.device());
    let _gpu = gate.lock();
    drop(api);
}

/// Raw D3D11 texture pointer from a frame, for NVENC register_resource.
pub fn texture_ptr(frame: &CapturedFrame<'_>) -> *mut c_void {
    frame.texture().as_raw()
}

#[cfg(test)]
mod tests {
    #[test]
    fn dib_fallback_runs_after_the_direct_copy_released_the_device_gate() {
        let gate = crate::gpu_gate::for_device_key(0x7001);
        let mut fallback_ran = false;
        let result = super::copy_with_dib_fallback(
            &gate,
            || anyhow::bail!("GetDC rejected"),
            || {
                // The real fallback locks this gate; re-entry would panic.
                let _gpu = gate.lock();
                fallback_ran = true;
                Ok(())
            },
        );
        assert!(result.is_ok());
        assert!(fallback_ran);
    }

    #[test]
    fn desktop_checks_are_bounded_and_recovery_bypasses_cache() {
        let now = std::time::Instant::now();
        let state = super::DesktopCheck {
            next_check: now + super::DESKTOP_CHECK_INTERVAL,
            is_default: true,
        };
        assert!(!state.due(now, false));
        assert!(!state.due(now + std::time::Duration::from_millis(249), false));
        assert!(state.due(now + std::time::Duration::from_millis(250), false));
        assert!(state.due(now, true));
    }

    #[test]
    fn queued_frame_cannot_reuse_default_observation_from_before_secure_switch() {
        let now = std::time::Instant::now();
        let mut state = super::DesktopCheck {
            next_check: now + super::DESKTOP_CHECK_INTERVAL,
            is_default: true,
        };
        // Housekeeping may use the cached observation before its deadline.
        assert!(state.refresh_with(now, false, || panic!("timer probe is not due")));
        // A real queued frame must query again even in that same interval.
        // Windows now reports secure (or an unreadable desktop), so the old
        // Default observation must never authorize delivery of the texture.
        assert!(!state.refresh_with(now, true, || false));
        assert!(!state.is_default);
        // Verified recovery likewise uses a fresh observation, rather than
        // waiting for the unavailable cached result to expire.
        assert!(state.refresh_with(now, true, || true));
    }

    #[test]
    fn discarded_damage_union_preserves_both_move_ends_and_can_expand_after_admission() {
        use windows::Win32::Foundation::{POINT, RECT};
        let mut metadata = super::DamageMetadata::default();
        metadata.dirty.push(RECT {
            left: 10,
            top: 10,
            right: 20,
            bottom: 20,
        });
        metadata
            .moves
            .push(windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_MOVE_RECT {
                SourcePoint: POINT { x: 80, y: 80 },
                DestinationRect: RECT {
                    left: 30,
                    top: 30,
                    right: 40,
                    bottom: 40,
                },
            });
        metadata.process(100, 100, true);
        assert_eq!(
            metadata.damage,
            vec![crate::frame_damage::DamageRect {
                left: 10,
                top: 10,
                right: 90,
                bottom: 90,
            }]
        );
        assert_eq!(
            metadata.moved,
            vec![crate::frame_damage::DamageRect {
                left: 30,
                top: 30,
                right: 40,
                bottom: 40,
            }]
        );
        assert!(!metadata.full);
        metadata.process(100, 100, false);
        assert_eq!(metadata.damage.len(), 3);
        assert!(metadata.full);
        let capacity = metadata.damage.capacity();
        metadata.reset();
        assert!(!metadata.valid);
        assert!(!metadata.loaded);
        assert!(metadata.damage.is_empty());
        assert_eq!(metadata.damage.capacity(), capacity);
    }

    #[test]
    fn secure_upload_rejects_malformed_bgra_dimensions() {
        assert!(super::validate_bgra_upload(2, 2, 8, 16).is_ok());
        for (width, height, stride, len) in [
            (0, 2, 8, 16),
            (2, 0, 8, 0),
            (2, 2, 4, 8),
            (2, 2, 9, 18),
            (2, 2, 8, 15),
            (u32::MAX, 1, 4, 4),
        ] {
            assert!(super::validate_bgra_upload(width, height, stride, len).is_err());
        }
    }
    use super::{
        availability_after, gdi_texture_desc, idle_snapshot_retry_due, monitor_switch_plan,
        move_damage_rects, pack_bgra_rows, supports_idle_snapshot_rotation, CaptureAvailability,
        CaptureRecoveryEvent, IdleSnapshotState, MonitorSwitchPlan,
    };

    #[test]
    fn same_monitor_reselection_consumes_current_duplication_before_reopening() {
        assert_eq!(
            monitor_switch_plan(r"\\.\DISPLAY6", r"\\.\display6"),
            MonitorSwitchPlan::RecreateCurrent
        );
        assert_eq!(
            monitor_switch_plan(r"\\.\DISPLAY5", r"\\.\DISPLAY6"),
            MonitorSwitchPlan::OpenReplacement
        );
    }
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_RESOURCE_MISC_GDI_COMPATIBLE,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE90,
    };
    use windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_MOVE_RECT;

    #[test]
    fn packed_bgra_readback_drops_row_pitch_padding() {
        let source = [
            1, 2, 3, 4, 5, 6, 7, 8, 90, 91, 92, 93, 94, 95, 96, 97, 9, 10, 11, 12, 13, 14, 15, 16,
            80, 81, 82, 83, 84, 85, 86, 87,
        ];
        assert_eq!(
            pack_bgra_rows(&source, 16, 2, 2),
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn failed_idle_snapshot_waits_for_its_retry_deadline() {
        let now = Instant::now();
        let retry_at = now + Duration::from_millis(250);
        assert!(!idle_snapshot_retry_due(now, retry_at));
        assert!(!idle_snapshot_retry_due(
            retry_at - Duration::from_millis(1),
            retry_at
        ));
        assert!(idle_snapshot_retry_due(retry_at, retry_at));
    }

    #[test]
    fn successful_snapshot_copy_stays_pending_until_its_idr_is_cached() {
        let now = Instant::now();
        let mut state = IdleSnapshotState::new(now);
        state.on_snapshot_copied(now);
        assert!(state.is_pending());
        state.on_idr_cached();
        assert!(!state.is_pending());
    }

    #[test]
    fn snapshot_ack_stops_startup_deadline_without_erasing_detail_followup() {
        let now = Instant::now();
        let mut state = IdleSnapshotState::new(now);
        assert_eq!(state.deadline(), Some(now));
        state.on_snapshot_copied(now);
        assert_eq!(
            state.deadline(),
            Some(now + super::IDLE_SNAPSHOT_RETRY_DELAY)
        );
        let followup = now + Duration::from_millis(34);
        state.request_followup(now, Duration::from_millis(34));
        state.on_idr_cached();
        assert!(!state.pending);
        assert_eq!(state.deadline(), Some(followup));
        assert!(state.is_due(followup));
        state.on_snapshot_copied(followup);
        assert_eq!(state.deadline(), None);
        state.request(followup);
        assert_eq!(state.deadline(), Some(followup));
    }

    #[test]
    fn forced_keyframe_rearms_a_cached_idle_snapshot() {
        let now = Instant::now();
        let mut state = IdleSnapshotState::new(now);
        state.on_idr_cached();
        assert!(!state.is_pending());
        state.request(now);
        assert!(state.is_pending());
        assert!(state.is_due(now));
    }

    #[test]
    fn quiescent_followup_snapshot_is_one_shot_without_idr_ack() {
        let now = Instant::now();
        let delay = Duration::from_millis(34);
        let mut state = IdleSnapshotState::new(now);
        state.on_idr_cached();

        state.request_followup(now, delay);
        assert!(!state.is_due(now));
        assert!(state.is_due(now + delay));

        state.on_snapshot_copied(now + delay);
        assert!(!state.is_due(now + delay));
        assert!(!state.is_pending());
    }

    #[test]
    fn quiescent_followup_does_not_replace_recovery_idr_snapshot_state() {
        let now = Instant::now();
        let mut state = IdleSnapshotState::new(now);
        state.request_followup(now, Duration::ZERO);

        state.on_snapshot_copied(now);

        assert!(state.is_pending());
        assert!(!state.is_due(now));
        assert!(state.is_due(now + super::IDLE_SNAPSHOT_RETRY_DELAY));
    }

    #[test]
    fn active_frames_rearm_or_cancel_the_quiescent_followup_deadline() {
        let now = Instant::now();
        let delay = Duration::from_millis(34);
        let mut state = IdleSnapshotState::new(now);
        state.on_idr_cached();

        state.request_followup(now, delay);
        state.request_followup(now + Duration::from_millis(20), delay);
        assert!(!state.is_due(now + delay));
        assert!(state.is_due(now + Duration::from_millis(54)));

        state.cancel_followup();
        assert!(!state.is_due(now + Duration::from_secs(1)));
    }

    #[test]
    fn idle_snapshot_rejects_rotated_outputs() {
        assert!(supports_idle_snapshot_rotation(DXGI_MODE_ROTATION_IDENTITY));
        assert!(!supports_idle_snapshot_rotation(
            DXGI_MODE_ROTATION_ROTATE90
        ));
    }

    #[test]
    fn access_denied_and_access_lost_are_source_unavailable() {
        use windows::core::{Error as WindowsError, HRESULT};
        use windows_capture::dxgi_duplication_api::Error;
        assert!(super::capture_error_is_unavailable(&Error::AccessLost));
        for code in [0x80070005u32, 0x887A0026] {
            assert!(super::capture_error_is_unavailable(&Error::WindowsError(
                WindowsError::from_hresult(HRESULT(code as i32)),
            )));
        }
        assert!(!super::capture_error_is_unavailable(&Error::Timeout));
        assert!(!super::capture_error_is_unavailable(&Error::WindowsError(
            WindowsError::from_hresult(HRESULT(0x80070057u32 as i32)),
        )));
        assert!(super::capture_error_is_unavailable(&Error::DirectXError(
            windows_capture::d3d11::Error::WindowsError(WindowsError::from_hresult(HRESULT(
                0x80070005u32 as i32
            )),),
        )));
        assert_eq!(super::CAPTURE_RECOVERY_INTERVAL, Duration::from_millis(500));
    }

    fn capture_without_native_session(now: Instant) -> super::Capture {
        let selected = r"\\.\DISPLAY6";
        super::Capture {
            api: None,
            width: 1920,
            height: 1080,
            origin_x: -1920,
            origin_y: 0,
            frames_acquired: 0,
            frames_new: 0,
            last_present_qpc: 0,
            device_obj: None,
            pending_recreate: true,
            next_recreate_attempt: now + Duration::from_secs(30),
            awaiting_recovery_frame: false,
            fresh_recreate_required: false,
            recreate_failed: false,
            monitor_filter: Some(selected.into()),
            monitor_device_name: selected.into(),
            idle_snapshot: IdleSnapshotState::new(now),
            idle_gdi: None,
            damage_metadata: super::DamageMetadata::default(),
            desktop_check: super::DesktopCheck::new(now),
            event_activity: None,
            waiter: None,
            invalidation: None,
            availability: CaptureAvailability::Unavailable,
            availability_transition: None,
            unavailable_reason: None,
        }
    }

    #[test]
    fn unavailable_capture_refuses_monitor_switch_and_retains_recovery_output() {
        let now = Instant::now();
        let selected = r"\\.\DISPLAY6";
        let mut capture = capture_without_native_session(now);
        assert!(capture.switch_monitor(r"\\.\DISPLAY1").is_err());
        assert!(capture.switch_monitor(selected).is_err());
        assert_eq!(capture.monitor_selector(), selected);
        assert_eq!(
            capture.region(),
            crate::input::DesktopRegion::new(-1920, 0, 1920, 1080)
        );
        assert!(capture.acquire(Duration::ZERO).unwrap().is_none());
        assert!(capture.take_idle_snapshot_frame().is_none());
        assert_eq!(capture.monitor_filter.as_deref(), Some(selected));
        assert_eq!(capture.availability(), CaptureAvailability::Unavailable);
    }

    #[test]
    fn healthy_parked_capture_has_no_deadline_or_native_work() {
        let now = Instant::now();
        let mut capture = capture_without_native_session(now);
        capture.pending_recreate = false;
        capture.availability = CaptureAvailability::Active;
        assert!(capture.can_recover_for_join());
        assert_eq!(capture.parked_maintenance_deadline(), None);
        // No native API/device exists in this fixture. Any attempted frame,
        // snapshot or healthy output query would fail or panic.
        for seconds in 0..10 {
            capture
                .maintain_parked(now + Duration::from_secs(seconds))
                .unwrap();
        }
        assert_eq!(capture.stats(), (0, 0));
        assert_eq!(capture.availability(), CaptureAvailability::Active);
    }

    #[test]
    fn stalled_duplication_takes_the_loss_path_once_and_recreates_on_the_next_acquire() {
        let now = Instant::now();
        let mut capture = capture_without_native_session(now);
        capture.pending_recreate = false;
        capture.availability = CaptureAvailability::Active;
        assert!(capture.recreate_stalled_duplication(now));
        assert!(capture.pending_recreate);
        assert_eq!(capture.availability(), CaptureAvailability::Unavailable);
        assert_eq!(
            capture.availability_transition,
            Some(CaptureAvailability::Unavailable)
        );
        assert_eq!(
            capture.unavailable_reason,
            Some(super::CaptureUnavailableReason::RecoveryAwaitingFrame)
        );
        // The existing recovery runs it on the next acquire, without backoff.
        assert!(super::recovery_attempt_due(
            capture.pending_recreate,
            now,
            capture.next_recreate_attempt
        ));
        // A pending recovery is not stacked or postponed.
        capture.next_recreate_attempt = now + Duration::from_secs(1);
        assert!(!capture.recreate_stalled_duplication(now));
        assert_eq!(capture.next_recreate_attempt, now + Duration::from_secs(1));
    }

    #[test]
    fn parked_recreation_runs_only_when_lost_and_retry_is_due() {
        let now = Instant::now();
        let deadline = now + super::PARKED_RECOVERY_INTERVAL;
        assert!(!super::recovery_attempt_due(false, deadline, now));
        assert!(!super::recovery_attempt_due(true, now, deadline));
        assert!(super::recovery_attempt_due(true, deadline, deadline));
        let mut capture = capture_without_native_session(now);
        capture.maintain_parked(now).unwrap();
        assert_eq!(
            capture.parked_maintenance_deadline(),
            Some(now + Duration::from_secs(30))
        );
        assert_eq!(capture.stats(), (0, 0));
    }

    #[test]
    fn failed_recreation_ends_join_grace_until_a_replacement_succeeds() {
        let now = Instant::now();
        let mut capture = capture_without_native_session(now);
        assert!(capture.can_recover_for_join());
        capture.recreate_failed = true;
        assert!(!capture.can_recover_for_join());
        capture.pending_recreate = false;
        capture.awaiting_recovery_frame = true;
        assert!(!capture.can_recover_for_join());
        capture.recreate_failed = false;
        assert!(capture.can_recover_for_join());
        assert_eq!(capture.availability(), CaptureAvailability::Unavailable);
    }

    #[test]
    fn dda_errors_map_to_native_unavailable_reasons() {
        use super::CaptureUnavailableReason as Reason;
        use windows::core::{Error as WindowsError, HRESULT};
        use windows_capture::dxgi_duplication_api::Error;
        assert_eq!(
            super::dda_error_reason(&Error::AccessLost),
            Reason::AccessLost {
                hresult: 0x887A0026
            }
        );
        assert_eq!(
            super::unavailable_reason_for_error(&Error::WindowsError(WindowsError::from_hresult(
                HRESULT(0x80070005u32 as i32)
            ))),
            Some(Reason::AccessDenied {
                hresult: 0x80070005
            })
        );
        assert_eq!(
            super::unavailable_reason_for_error(&Error::WindowsError(WindowsError::from_hresult(
                HRESULT(0x887A0026u32 as i32)
            ))),
            Some(Reason::AccessLost {
                hresult: 0x887A0026
            })
        );
        // A wrapped DirectX error keeps the native access-denied code.
        assert_eq!(
            super::unavailable_reason_for_error(&Error::DirectXError(
                windows_capture::d3d11::Error::WindowsError(WindowsError::from_hresult(HRESULT(
                    0x80070005u32 as i32
                )))
            )),
            Some(Reason::AccessDenied {
                hresult: 0x80070005
            })
        );
        assert_eq!(super::unavailable_reason_for_error(&Error::Timeout), None);
        assert_eq!(
            super::unavailable_reason_for_error(&Error::WindowsError(WindowsError::from_hresult(
                HRESULT(0x80070057u32 as i32)
            ))),
            None
        );
        // Non-unavailable errors still report their HRESULT when they cause a
        // failed recreation.
        assert_eq!(
            super::dda_error_reason(&Error::WindowsError(WindowsError::from_hresult(HRESULT(
                0x80070057u32 as i32
            )))),
            Reason::RecreateFailed {
                hresult: Some(0x80070057)
            }
        );
        assert_eq!(
            super::dda_error_reason(&Error::Timeout),
            Reason::RecreateFailed { hresult: None }
        );
    }

    #[test]
    fn recreate_failures_report_the_first_native_code_in_context() {
        use super::CaptureUnavailableReason as Reason;
        use anyhow::Context;
        use windows::core::{Error as WindowsError, HRESULT};
        use windows_capture::dxgi_duplication_api::Error;
        let failed: Result<(), Error> = Err(Error::WindowsError(WindowsError::from_hresult(
            HRESULT(0x887A0022u32 as i32),
        )));
        let error = failed
            .context("DxgiDuplicationApi fresh-session recreate")
            .unwrap_err();
        assert_eq!(
            super::recreate_failure_reason(&error),
            Reason::RecreateFailed {
                hresult: Some(0x887A0022)
            }
        );
        assert_eq!(
            super::recreate_failure_reason(&anyhow::anyhow!("pick_monitor: no display")),
            Reason::RecreateFailed { hresult: None }
        );
    }

    #[test]
    fn unavailable_state_records_recreate_failure_then_awaiting_frame_then_secure_desktop() {
        use super::CaptureUnavailableReason as Reason;
        use windows::core::{Error as WindowsError, HRESULT};
        let now = Instant::now();
        let mut capture = capture_without_native_session(now);
        // The fixture has no recorded transition, so no cause is reported.
        assert_eq!(capture.unavailable_reason(), None);

        capture.record_recreate_failed(
            now,
            Duration::from_millis(500),
            &anyhow::Error::new(WindowsError::from_hresult(HRESULT(0x887A0022u32 as i32))),
        );
        assert_eq!(
            capture.unavailable_reason(),
            Some(Reason::RecreateFailed {
                hresult: Some(0x887A0022)
            })
        );
        assert!(!capture.can_recover_for_join());

        capture.record_recreated();
        assert_eq!(capture.availability(), CaptureAvailability::Unavailable);
        assert_eq!(
            capture.unavailable_reason(),
            Some(Reason::RecoveryAwaitingFrame)
        );

        capture.suspend_for_secure_desktop();
        assert_eq!(capture.availability(), CaptureAvailability::Unavailable);
        assert_eq!(capture.unavailable_reason(), Some(Reason::SecureDesktop));
        assert_eq!(capture.stats(), (0, 0));
    }

    #[test]
    fn reported_reason_gives_display_power_precedence_and_nothing_when_active() {
        use super::CaptureUnavailableReason as Reason;
        use super::{reported_unavailable_reason as report, CaptureAvailability as A};
        let lost = Some(Reason::AccessLost {
            hresult: 0x887A0026,
        });
        assert_eq!(report(A::Active, true, lost), None);
        assert_eq!(report(A::Active, false, lost), None);
        assert_eq!(report(A::Unavailable, true, lost), Some(Reason::DisplayOff));
        assert_eq!(report(A::Unavailable, true, None), Some(Reason::DisplayOff));
        assert_eq!(report(A::Unavailable, false, lost), lost);
        assert_eq!(report(A::Unavailable, false, None), None);
    }

    #[test]
    fn unavailable_reasons_render_native_codes_for_logs() {
        use super::CaptureUnavailableReason as Reason;
        assert_eq!(
            Reason::AccessLost {
                hresult: 0x887A0026
            }
            .to_string(),
            "DXGI access lost (DXGI_ERROR_ACCESS_LOST, 0x887A0026)"
        );
        assert_eq!(
            Reason::AccessDenied {
                hresult: 0x80070005
            }
            .to_string(),
            "DDA access denied (E_ACCESSDENIED, 0x80070005)"
        );
        assert_eq!(
            Reason::RecreateFailed {
                hresult: Some(0x887A0022)
            }
            .to_string(),
            "DDA recreation failed (0x887A0022)"
        );
        assert_eq!(
            Reason::RecreateFailed { hresult: None }.to_string(),
            "DDA recreation failed"
        );
        assert_eq!(Reason::DisplayOff.to_string(), "display off");
        assert_eq!(
            Reason::SecureDesktop.to_string(),
            "input desktop is not Default"
        );
    }

    #[test]
    fn only_a_successful_replacement_allows_gdi_to_verify_recovery() {
        assert!(super::snapshot_can_verify_recovery(
            false, true, false, true
        ));
        for (pending, awaiting, failed, has_api) in [
            (true, false, false, false),
            (true, true, false, true),
            (false, true, true, true),
            (false, true, false, false),
            (false, false, false, true),
        ] {
            assert!(!super::snapshot_can_verify_recovery(
                pending, awaiting, failed, has_api
            ));
        }
    }

    #[test]
    fn recreated_session_cannot_report_active_without_verified_pixels() {
        let unavailable = availability_after(
            CaptureAvailability::Active,
            CaptureRecoveryEvent::AccessLost,
        );
        assert_eq!(unavailable, CaptureAvailability::Unavailable);
        assert_eq!(
            availability_after(unavailable, CaptureRecoveryEvent::SessionRecreated),
            CaptureAvailability::Unavailable,
        );
        assert_eq!(
            availability_after(unavailable, CaptureRecoveryEvent::FrameAcquired),
            CaptureAvailability::Active,
        );
    }

    #[test]
    fn secure_gdi_texture_is_gpu_resident_bgra_and_gdi_compatible() {
        let desc = gdi_texture_desc(1920, 1080);
        assert_eq!((desc.Width, desc.Height), (1920, 1080));
        assert_eq!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
        assert_ne!(desc.BindFlags & D3D11_BIND_RENDER_TARGET.0 as u32, 0);
        assert_ne!(
            desc.MiscFlags & D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32,
            0
        );
        assert_eq!(desc.CPUAccessFlags, 0);
    }

    #[test]
    fn moved_desktop_pixels_mark_both_source_and_destination_as_changed() {
        let regions = move_damage_rects(
            DXGI_OUTDUPL_MOVE_RECT {
                SourcePoint: POINT { x: 10, y: 20 },
                DestinationRect: RECT {
                    left: 100,
                    top: 200,
                    right: 140,
                    bottom: 230,
                },
            },
            1920,
            1080,
        )
        .collect::<Vec<_>>();

        assert_eq!(
            regions,
            vec![
                crate::frame_damage::DamageRect {
                    left: 100,
                    top: 200,
                    right: 140,
                    bottom: 230,
                },
                crate::frame_damage::DamageRect {
                    left: 10,
                    top: 20,
                    right: 50,
                    bottom: 50,
                },
            ]
        );
    }
}
