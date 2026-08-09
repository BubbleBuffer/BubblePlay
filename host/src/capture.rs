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
//! The NVENC session is opened on the D3D11 device this API uses, obtained
//! from the first acquired frame via `frame.device()`.

use anyhow::{bail, Context, Result};
use core::ffi::c_void;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows_capture::dxgi_duplication_api::{
    DxgiDuplicationApi, DxgiDuplicationFrame, Error as CaptureError,
};
use windows_capture::monitor::Monitor;

pub struct Capture {
    api: DxgiDuplicationApi,
    width: u32,
    height: u32,
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
    /// Retained for AccessLost recreation. `None` = primary monitor.
    monitor_filter: Option<String>,
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

impl Capture {
    /// Create a duplication session. `monitor_filter` selects the target
    /// display (see [`pick_monitor`]); `None` captures the primary monitor.
    pub fn new(monitor_filter: Option<&str>) -> Result<Self> {
        let monitor = pick_monitor(monitor_filter)?;
        let dev = monitor.device_name().unwrap_or_default();
        let api = DxgiDuplicationApi::new(monitor).context("DxgiDuplicationApi::new")?;
        let width = api.width();
        let height = api.height();
        info!(width, height, monitor = %dev, "DDA session created");
        Ok(Self {
            api,
            width,
            height,
            frames_acquired: 0,
            frames_new: 0,
            last_present_qpc: 0,
            device_obj: None,
            pending_recreate: false,
            next_recreate_attempt: Instant::now(),
            monitor_filter: monitor_filter.map(|s| s.to_string()),
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The D3D11 device NVENC must bind to (pinned from the first frame).
    pub fn device(&self) -> Option<&ID3D11Device> {
        self.device_obj.as_ref()
    }

    /// Acquire the next frame, blocking up to `timeout`.
    ///
    /// `Ok(None)` on timeout (desktop idle). On `AccessLost` we recreate the
    /// session inline and return `Ok(None)` — the caller just retries.
    /// The returned bool tuple is `(frame, is_new_present)`.
    pub fn acquire<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> Result<Option<(DxgiDuplicationFrame<'a>, bool)>> {
        // Deferred recovery from a previous AccessLost (see field doc).
        if self.pending_recreate {
            if Instant::now() < self.next_recreate_attempt {
                return Ok(None);
            }
            match self.recreate() {
                Ok(()) => {
                    self.pending_recreate = false;
                }
                Err(e) => {
                    // Recreation can legitimately fail for a while: a UAC
                    // secure-desktop switch or a locked screen invalidates the
                    // session AND denies new ones until the desktop is back.
                    // Never kill or block the stream over this — the RTC loop
                    // keeps pumping while we wait for the next retry deadline.
                    warn!(?e, "DDA recreate failed — will retry");
                    self.next_recreate_attempt = Instant::now() + Duration::from_millis(250);
                    return Ok(None);
                }
            }
        }
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let frame = match self.api.acquire_next_frame(timeout_ms) {
            Ok(f) => f,
            Err(CaptureError::Timeout) => return Ok(None),
            Err(CaptureError::AccessLost) => {
                warn!("DDA access lost — will recreate on next acquire");
                // Disjoint field write; `self.api` is borrowed for 'a by the
                // (absent) Ok frame, so no `&mut self` calls here.
                self.pending_recreate = true;
                self.next_recreate_attempt = Instant::now();
                return Ok(None);
            }
            Err(e) => return Err(anyhow::anyhow!("acquire_next_frame: {e:?}")),
        };

        // First successful frame: pin the D3D11 device for NVENC.
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

        Ok(Some((frame, is_new)))
    }

    fn recreate(&mut self) -> Result<()> {
        let monitor = pick_monitor(self.monitor_filter.as_deref())?;
        let api = DxgiDuplicationApi::new(monitor).context("DxgiDuplicationApi recreate")?;
        self.api = api;
        self.width = self.api.width();
        self.height = self.api.height();
        // Device may change after recreation; force re-pin on next frame.
        self.device_obj = None;
        self.last_present_qpc = 0;
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
pub fn texture_ptr(frame: &DxgiDuplicationFrame<'_>) -> *mut c_void {
    frame.texture().as_raw() as *mut c_void
}
