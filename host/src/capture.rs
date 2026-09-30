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
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_IDENTITY, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGISurface1, DXGI_OUTDUPL_MOVE_RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDCW, CreateDIBSection, DeleteDC, DeleteObject,
    GetMonitorInfoW, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
    DIB_RGB_COLORS, HMONITOR, MONITORINFO, SRCCOPY,
};
use windows_capture::dxgi_duplication_api::{
    DxgiDuplicationApi, DxgiDuplicationFormat, DxgiDuplicationFrame, Error as CaptureError,
};
use windows_capture::encoder::{ImageEncoder, ImageEncoderPixelFormat, ImageFormat};
use windows_capture::monitor::Monitor;

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
    /// Index into the bounded 250/500/1000/2000 ms retry schedule.
    recovery_backoff_step: usize,
    /// A recreated duplication is not healthy until it yields a real frame.
    awaiting_recovery_frame: bool,
    /// If a replacement immediately loses access, discard the complete
    /// D3D11/duplication session on the next attempt.
    fresh_recreate_required: bool,
    /// Retained for AccessLost recreation. `None` = primary monitor.
    monitor_filter: Option<String>,
    /// The selected output's DeviceName used by the GDI idle snapshot.
    monitor_device_name: String,
    idle_snapshot: IdleSnapshotState,
    availability: CaptureAvailability,
    availability_transition: Option<CaptureAvailability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureAvailability {
    Active,
    Unavailable,
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
const RECOVERY_BACKOFF_MS: [u64; 4] = [250, 500, 1_000, 2_000];

fn recovery_delay(step: usize) -> Duration {
    Duration::from_millis(RECOVERY_BACKOFF_MS[step.min(RECOVERY_BACKOFF_MS.len() - 1)])
}

fn idle_snapshot_retry_due(now: Instant, retry_at: Instant) -> bool {
    now >= retry_at
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
pub enum CapturedFrame<'a> {
    Desktop {
        frame: DxgiDuplicationFrame<'a>,
        /// Dirty rects plus both ends of every move rect.
        damage: Option<Vec<DamageRect>>,
        /// Destinations of DDA move rects (content that was moved, e.g. a
        /// scroll the compositor reported as a move).
        moved: Vec<DamageRect>,
    },
    IdleSnapshot {
        device: &'a ID3D11Device,
        texture: ID3D11Texture2D,
        width: u32,
        height: u32,
    },
    SecureSnapshot {
        device: &'a ID3D11Device,
        texture: ID3D11Texture2D,
        width: u32,
        height: u32,
    },
}

impl CapturedFrame<'_> {
    pub fn is_idle_snapshot(&self) -> bool {
        matches!(self, Self::IdleSnapshot { .. })
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
            Self::Desktop { damage, .. } => damage.as_deref(),
            Self::IdleSnapshot { .. } | Self::SecureSnapshot { .. } => None,
        }
    }

    /// Destinations of this frame's DDA move rects.
    pub fn moved(&self) -> &[DamageRect] {
        match self {
            Self::Desktop { moved, .. } => moved,
            Self::IdleSnapshot { .. } | Self::SecureSnapshot { .. } => &[],
        }
    }

    pub fn encode_png_crop(&mut self, rect: DamageRect) -> Result<Vec<u8>> {
        let Self::Desktop { frame, .. } = self else {
            bail!("lossless detail tiles require a live DDA desktop frame");
        };
        if rect.left >= rect.right
            || rect.top >= rect.bottom
            || rect.right > frame.width()
            || rect.bottom > frame.height()
        {
            bail!("lossless detail tile is outside the captured frame");
        }
        let buffer = frame
            .buffer_crop(rect.left, rect.top, rect.right, rect.bottom)
            .context("copy lossless detail tile to CPU")?;
        let format = match buffer.format() {
            DxgiDuplicationFormat::Bgra8 | DxgiDuplicationFormat::Bgra8Srgb => {
                ImageEncoderPixelFormat::Bgra8
            }
            DxgiDuplicationFormat::Rgba8 | DxgiDuplicationFormat::Rgba8Srgb => {
                ImageEncoderPixelFormat::Rgba8
            }
            other => bail!("unsupported lossless detail tile format {other:?}"),
        };
        let mut packed = Vec::new();
        let pixels = buffer.as_nopadding_buffer(&mut packed);
        ImageEncoder::new(ImageFormat::Png, format)
            .context("create PNG tile encoder")?
            .encode(pixels, rect.right - rect.left, rect.bottom - rect.top)
            .context("encode lossless detail tile")
    }
}

pub fn encode_texture_png_crop(texture: &ID3D11Texture2D, rect: DamageRect) -> Result<Vec<u8>> {
    let mut source_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut source_desc) };
    if rect.left >= rect.right
        || rect.top >= rect.bottom
        || rect.right > source_desc.Width
        || rect.bottom > source_desc.Height
        || source_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
        || source_desc.SampleDesc.Count != 1
    {
        bail!("lossless detail tile is outside a single-sample BGRA texture");
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    let device = unsafe { texture.GetDevice() }.context("detail texture GetDevice")?;
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
    ImageEncoder::new(ImageFormat::Png, ImageEncoderPixelFormat::Bgra8)
        .context("create scaled detail PNG encoder")?
        .encode(&packed, width, height)
        .context("encode scaled detail PNG")
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

/// The frame's damage (dirty rects and both ends of each move) and the move
/// destinations on their own.
fn duplication_damage(
    frame: &DxgiDuplicationFrame<'_>,
) -> Result<(Vec<DamageRect>, Vec<DamageRect>)> {
    let metadata_bytes = frame.frame_info().TotalMetadataBufferSize;
    if metadata_bytes == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let duplication = frame.duplication();
    let mut damage = Vec::new();

    let dirty_capacity = (metadata_bytes as usize)
        .div_ceil(std::mem::size_of::<RECT>())
        .max(1);
    let mut dirty = vec![RECT::default(); dirty_capacity];
    let mut dirty_bytes = 0;
    unsafe {
        duplication.GetFrameDirtyRects(
            (dirty.len() * std::mem::size_of::<RECT>()) as u32,
            dirty.as_mut_ptr(),
            &mut dirty_bytes,
        )
    }
    .context("IDXGIOutputDuplication::GetFrameDirtyRects")?;
    dirty.truncate(dirty_bytes as usize / std::mem::size_of::<RECT>());
    damage.extend(
        dirty
            .into_iter()
            .filter_map(|rect| clipped_damage_rect(rect, frame.width(), frame.height())),
    );

    let move_capacity = (metadata_bytes as usize)
        .div_ceil(std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>())
        .max(1);
    let mut moves = vec![DXGI_OUTDUPL_MOVE_RECT::default(); move_capacity];
    let mut move_bytes = 0;
    unsafe {
        duplication.GetFrameMoveRects(
            (moves.len() * std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>()) as u32,
            moves.as_mut_ptr(),
            &mut move_bytes,
        )
    }
    .context("IDXGIOutputDuplication::GetFrameMoveRects")?;
    moves.truncate(move_bytes as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>());
    let moved = moves
        .iter()
        .filter_map(|movement| {
            clipped_damage_rect(movement.DestinationRect, frame.width(), frame.height())
        })
        .collect();
    damage.extend(
        moves
            .into_iter()
            .flat_map(|movement| move_damage_rects(movement, frame.width(), frame.height())),
    );
    Ok((damage, moved))
}

/// GDI-backed capture source used on the Winlogon/UAC desktop where desktop
/// duplication is intentionally unavailable. It owns a standalone D3D11
/// device so cold-boot capture does not depend on a previous DDA session.
pub struct SecureCapture {
    device: ID3D11Device,
    width: u32,
    height: u32,
    monitor_device_name: String,
}

impl SecureCapture {
    pub fn primary() -> Result<Self> {
        let monitor = Monitor::primary().context("find primary monitor for secure capture")?;
        let width = monitor.width().context("secure monitor width")?;
        let height = monitor.height().context("secure monitor height")?;
        let monitor_device_name = monitor.device_name().unwrap_or_default();
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
        .context("D3D11CreateDevice(secure capture)")?;
        Ok(Self {
            device: device.context("secure D3D11 device was not returned")?,
            width,
            height,
            monitor_device_name,
        })
    }

    pub fn capture(&self) -> Result<CapturedFrame<'_>> {
        let texture = capture_gdi_texture(
            &self.device,
            self.width,
            self.height,
            &self.monitor_device_name,
        )?;
        Ok(CapturedFrame::SecureSnapshot {
            device: &self.device,
            texture,
            width: self.width,
            height: self.height,
        })
    }
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

fn capture_gdi_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    monitor_device_name: &str,
) -> Result<ID3D11Texture2D> {
    let desc = gdi_texture_desc(width, height);
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("CreateTexture2D(GDI capture)")?;
    let texture = texture.context("CreateTexture2D(GDI capture) returned no texture")?;
    let surface: IDXGISurface1 = texture
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
    let copy_result = (|| -> Result<()> {
        let target_dc = unsafe { surface.GetDC(true) }.context("IDXGISurface1::GetDC")?;
        let blit = unsafe {
            BitBlt(
                target_dc,
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
        let release = unsafe { surface.ReleaseDC(None) };
        blit.context("BitBlt desktop into GDI texture")?;
        release.context("IDXGISurface1::ReleaseDC")?;
        Ok(())
    })();
    match copy_result {
        Ok(()) => {
            let _ = unsafe { DeleteDC(source_dc) };
            Ok(texture)
        }
        Err(direct_error) => {
            warn!(
                ?direct_error,
                "direct GDI/DXGI capture failed; using CPU DIB fallback"
            );
            let fallback = capture_gdi_via_dib(device, &texture, source_dc, width, height);
            let _ = unsafe { DeleteDC(source_dc) };
            fallback?;
            Ok(texture)
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
        let upload = if blit.is_ok() {
            let context = unsafe { device.GetImmediateContext() }
                .context("get D3D11 context for secure capture fallback")?;
            unsafe {
                context.UpdateSubresource(texture, 0, None, bits, width.saturating_mul(4), 0)
            };
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "BitBlt into secure capture DIB failed: {}",
                blit.unwrap_err()
            ))
        };
        let _ = unsafe { SelectObject(memory_dc, previous) };
        let _ = unsafe { DeleteObject(bitmap.into()) };
        upload
    })();
    let _ = unsafe { DeleteDC(memory_dc) };
    result
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
        let api = DxgiDuplicationApi::new(monitor).context("DxgiDuplicationApi::new")?;
        let width = api.width();
        let height = api.height();
        let device_obj = api.device().clone();
        info!(width, height, monitor = %dev, "DDA session created");
        Ok(Self {
            api: Some(api),
            width,
            height,
            origin_x: region.x,
            origin_y: region.y,
            frames_acquired: 0,
            frames_new: 0,
            last_present_qpc: 0,
            device_obj: Some(device_obj),
            pending_recreate: false,
            next_recreate_attempt: Instant::now(),
            recovery_backoff_step: 0,
            awaiting_recovery_frame: false,
            fresh_recreate_required: false,
            monitor_filter: monitor_filter.map(|s| s.to_string()),
            monitor_device_name: dev,
            idle_snapshot: IdleSnapshotState::new(Instant::now()),
            availability: CaptureAvailability::Active,
            availability_transition: None,
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
        let monitor = pick_monitor(Some(selector))?;
        let region = monitor_region(monitor).context("selected monitor bounds")?;
        let dev = monitor.device_name().unwrap_or_default();
        if monitor_switch_plan(&self.monitor_device_name, &dev)
            == MonitorSwitchPlan::RecreateCurrent
        {
            // DuplicateOutput can reject a second live duplication object for
            // the same ParsecVDA output with E_INVALIDARG. The recovery path
            // consumes the current API first, then recreates it on the exact
            // same selector, which is also the required ownership order for a
            // silently stalled (timeout-only) duplication session.
            self.monitor_filter = Some(dev.clone());
            self.recreate(false)
                .context("recreate current monitor duplication")?;
            self.pending_recreate = false;
            self.next_recreate_attempt = Instant::now();
            self.recovery_backoff_step = 0;
            self.awaiting_recovery_frame = false;
            self.fresh_recreate_required = false;
            self.availability = CaptureAvailability::Active;
            info!(
                width = self.width,
                height = self.height,
                monitor = %self.monitor_device_name,
                "DDA current monitor reselected"
            );
            return Ok(());
        }
        let api =
            DxgiDuplicationApi::new(monitor).context("DxgiDuplicationApi::new monitor switch")?;
        self.width = api.width();
        self.height = api.height();
        self.origin_x = region.x;
        self.origin_y = region.y;
        self.device_obj = Some(api.device().clone());
        self.last_present_qpc = 0;
        self.monitor_filter = Some(dev.clone());
        self.monitor_device_name = dev;
        self.api = Some(api);
        self.pending_recreate = false;
        self.next_recreate_attempt = Instant::now();
        self.recovery_backoff_step = 0;
        self.awaiting_recovery_frame = false;
        self.fresh_recreate_required = false;
        self.idle_snapshot = IdleSnapshotState::new(Instant::now());
        self.availability = CaptureAvailability::Active;
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

    /// Acquire the next frame, blocking up to `timeout`.
    ///
    /// `Ok(None)` on timeout (desktop idle). On `AccessLost` we recreate the
    /// session inline and return `Ok(None)` — the caller just retries.
    /// The returned tuple is `(frame, is_new_present, recovered)`. `recovered`
    /// is true only for the first real DDA frame after an unavailable period.
    pub fn acquire<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> Result<Option<(CapturedFrame<'a>, bool, bool)>> {
        // Deferred recovery from a previous AccessLost (see field doc).
        if self.pending_recreate {
            if Instant::now() < self.next_recreate_attempt {
                return Ok(None);
            }
            match self.recreate(self.fresh_recreate_required) {
                Ok(()) => {
                    self.pending_recreate = false;
                    self.awaiting_recovery_frame = true;
                    self.fresh_recreate_required = false;
                    self.availability = availability_after(
                        self.availability,
                        CaptureRecoveryEvent::SessionRecreated,
                    );
                }
                Err(e) => {
                    // Recreation can legitimately fail for a while: a UAC
                    // secure-desktop switch or a locked screen invalidates the
                    // session AND denies new ones until the desktop is back.
                    // Never kill or block the stream over this — the RTC loop
                    // keeps pumping while we wait for the next retry deadline.
                    let delay = recovery_delay(self.recovery_backoff_step);
                    self.recovery_backoff_step = self.recovery_backoff_step.saturating_add(1);
                    self.next_recreate_attempt = Instant::now() + delay;
                    self.fresh_recreate_required = true;
                    debug!(
                        ?e,
                        retry_ms = delay.as_millis(),
                        "DDA recreate still unavailable"
                    );
                    return Ok(None);
                }
            }
        }
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let frame = match self
            .api
            .as_mut()
            .context("DDA session temporarily unavailable")?
            .acquire_next_frame(timeout_ms)
        {
            Ok(f) => f,
            Err(CaptureError::Timeout) => return Ok(None),
            Err(CaptureError::AccessLost) => {
                let first_loss = self.availability != CaptureAvailability::Unavailable;
                if first_loss {
                    warn!(
                        "DDA access lost; capture unavailable until a replacement yields a frame"
                    );
                } else {
                    debug!("replacement DDA session lost access before producing a frame");
                }
                // Disjoint field write; `self.api` is borrowed for 'a by the
                // (absent) Ok frame, so no `&mut self` calls here.
                self.pending_recreate = true;
                let delay = recovery_delay(self.recovery_backoff_step);
                self.recovery_backoff_step = self.recovery_backoff_step.saturating_add(1);
                self.next_recreate_attempt = Instant::now() + delay;
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
            Err(e) => return Err(anyhow::anyhow!("acquire_next_frame: {e:?}")),
        };

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
            self.awaiting_recovery_frame = false;
            self.fresh_recreate_required = false;
            self.recovery_backoff_step = 0;
            info!("DDA capture produced a verified recovery frame");
        }

        let (damage, moved) = if is_new {
            match duplication_damage(&frame) {
                Ok((damage, moved)) => (Some(damage), moved),
                Err(error) => {
                    debug!(?error, "DDA frame damage metadata unavailable");
                    (None, Vec::new())
                }
            }
        } else {
            (None, Vec::new())
        };
        Ok(Some((
            CapturedFrame::Desktop {
                frame,
                damage,
                moved,
            },
            is_new,
            recovered,
        )))
    }

    /// Return a one-shot GPU-resident snapshot of the selected output when
    /// DDA has not produced an initial desktop texture. Failure is nonfatal
    /// and retried after a short backoff, leaving the zero-timeout DDA pump
    /// untouched.
    pub fn take_idle_snapshot_frame(&mut self) -> Option<CapturedFrame<'_>> {
        let now = Instant::now();
        if !self.idle_snapshot.is_due(now) {
            return None;
        }
        match self.capture_idle_snapshot() {
            Ok(texture) => {
                self.idle_snapshot.on_snapshot_copied(now);
                debug!(
                    width = self.width,
                    height = self.height,
                    monitor = %self.monitor_device_name,
                    "captured idle-desktop snapshot frame"
                );
                Some(CapturedFrame::IdleSnapshot {
                    device: self.api.as_ref().expect("DDA session present").device(),
                    texture,
                    width: self.width,
                    height: self.height,
                })
            }
            Err(e) => {
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

    /// Re-arm the GPU-resident desktop snapshot path for a forced keyframe.
    /// A static desktop may never produce another DDA present, so clearing the
    /// cached IDR must always be paired with an explicit snapshot request.
    pub fn request_idle_snapshot(&mut self) {
        self.idle_snapshot.request(Instant::now());
    }

    /// Schedule one idle capture after a grace period so a deferred GPU
    /// verdict can be collected if DDA produces no following desktop frame.
    /// Unlike recovery snapshots, this wake is consumed by one copied frame
    /// and never waits for an encoded IDR.
    pub fn request_idle_followup_snapshot(&mut self, delay: Duration) {
        self.idle_snapshot.request_followup(Instant::now(), delay);
    }

    pub fn cancel_idle_followup_snapshot(&mut self) {
        self.idle_snapshot.cancel_followup();
    }

    fn capture_idle_snapshot(&self) -> Result<ID3D11Texture2D> {
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
        capture_gdi_texture(
            api.device(),
            self.width,
            self.height,
            &self.monitor_device_name,
        )
    }

    fn recreate(&mut self, force_fresh: bool) -> Result<()> {
        // The crate's recovery API drops IDXGIOutputDuplication before asking
        // DXGI for its replacement and preserves the D3D11 device. If that
        // fails after a larger display/topology transition, `old` has still
        // been consumed, so a completely fresh session cannot conflict with
        // the invalid handle. `None` is a valid retry state when both attempts
        // failed on the previous pass (for example during secure desktop).
        let old_api = self.api.take();
        let api = match (force_fresh, old_api) {
            (true, old) => {
                drop(old);
                let monitor = pick_monitor(self.monitor_filter.as_deref())?;
                DxgiDuplicationApi::new(monitor)
                    .context("DxgiDuplicationApi fresh-session recreate")?
            }
            (false, Some(old)) => match old.recreate() {
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

/// Raw D3D11 texture pointer from a frame, for NVENC register_resource.
pub fn texture_ptr(frame: &CapturedFrame<'_>) -> *mut c_void {
    frame.texture().as_raw() as *mut c_void
}

#[cfg(test)]
mod tests {
    use super::{
        availability_after, gdi_texture_desc, idle_snapshot_retry_due, monitor_switch_plan,
        move_damage_rects, pack_bgra_rows, recovery_delay, supports_idle_snapshot_rotation,
        CaptureAvailability, CaptureRecoveryEvent, IdleSnapshotState, MonitorSwitchPlan,
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
    fn capture_recovery_backoff_caps_at_two_seconds() {
        assert_eq!(recovery_delay(0), Duration::from_millis(250));
        assert_eq!(recovery_delay(1), Duration::from_millis(500));
        assert_eq!(recovery_delay(2), Duration::from_secs(1));
        assert_eq!(recovery_delay(3), Duration::from_secs(2));
        assert_eq!(recovery_delay(99), Duration::from_secs(2));
    }

    #[test]
    fn recreated_session_cannot_report_active_before_a_real_frame() {
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
