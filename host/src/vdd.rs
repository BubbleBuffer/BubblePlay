//! Inline ParsecVDA (Parsec Virtual Display Driver) keepalive.
//!
//! BubblePlay must capture a desktop even when no physical monitor is active
//! (headless, lid closed, no dummy plug). Parsec's VDD driver exposes a
//! virtual 1080p60 monitor, but it only stays plugged while a client holds
//! the control device open and pings it periodically. Instead of relying on
//! an external helper process, we do it inline:
//!
//! 1. Enumerate the VDD device interface
//!    (class GUID `00b41627-04c4-429e-a26e-0265cf50c8fa`) via SetupDi.
//! 2. Open it (`GENERIC_READ|GENERIC_WRITE`, overlapped — matches the
//!    known-good C# helper byte-for-byte).
//! 3. IOCTL `0x22E004` plugs one virtual display (returns its index).
//! 4. A background thread pings with IOCTL `0x22A00C` every 100 ms.
//! 5. On drop: stop pinging, best-effort unplug (IOCTL `0x22A008`), close.
//!
//! Protocol source: nomi-san/parsec-vdd (MIT), cross-verified against the
//! C# helper this project used earlier (see repo history / handoff notes).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};
use std::time::Instant as StdInstant;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

const VDD_INTERFACE_GUID: GUID = GUID::from_u128(0x00b41627_04c4_429e_a26e_0265cf50c8fa);
const IOCTL_VDD_ADD: u32 = 0x22E004;
const IOCTL_VDD_REMOVE: u32 = 0x22A008;
const IOCTL_VDD_PING: u32 = 0x22A00C;
/// The C# helper pinged every 100 ms; the driver's unplug timeout is ~200 ms
/// after pings stop, so this keeps comfortable margin.
const PING_INTERVAL: Duration = Duration::from_millis(100);

/// `HANDLE` is `!Send` in windows 0.62 (wraps `*mut c_void`); the ping thread
/// only calls DeviceIoControl with it, which is fine on any thread.
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}

pub struct VddKeepalive {
    handle: HANDLE,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VddKeepalive {
    /// Plug a virtual display and start the ping loop. Fails if the Parsec
    /// VDD driver is not installed/enumerable (callers may treat that as
    /// non-fatal — physical-monitor capture still works).
    pub fn start() -> Result<Self> {
        let path = find_device_path().context("find VDD device interface")?;
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                // GENERIC_READ | GENERIC_WRITE (param is raw u32 in windows 0.62)
                0xC0000000,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | FILE_FLAG_NO_BUFFERING | FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .context("open VDD control device")?;

        // Plug one display; the driver returns its index.
        let input = [0u8; 32];
        let mut out = [0u8; 4];
        let mut ret = 0u32;
        unsafe {
            DeviceIoControl(
                handle,
                IOCTL_VDD_ADD,
                Some(input.as_ptr() as *const _),
                input.len() as u32,
                Some(out.as_mut_ptr() as *mut _),
                out.len() as u32,
                Some(&mut ret),
                None,
            )
        }
        .context("IOCTL plug virtual display")?;
        let index = i32::from_le_bytes(out);
        info!(display_index = index, path = %path, "virtual display plugged (VDD keepalive active)");

        // Background ping loop. The handle is used from this thread only
        // until drop (which joins before issuing the final IOCTLs).
        let stop = Arc::new(AtomicBool::new(false));
        let stop_c = stop.clone();
        let handle_c = SendHandle(handle);
        let thread = std::thread::spawn(move || {
            // Whole-value move defeats disjoint capture of the HANDLE field,
            // which would otherwise bypass SendHandle's Send impl.
            let handle = handle_c;
            let mut failures = 0u32;
            // Match the reference implementations: ping FIRST, then sleep, so
            // the driver gets its first keepalive immediately after ADD.
            loop {
                let mut r = 0u32;
                let t0 = StdInstant::now();
                let ok = unsafe {
                    DeviceIoControl(
                        handle.0,
                        IOCTL_VDD_PING,
                        Some(input.as_ptr() as *const _),
                        input.len() as u32,
                        None,
                        0,
                        Some(&mut r),
                        None,
                    )
                    .is_ok()
                };
                let call_ms = t0.elapsed().as_millis();
                if ok {
                    if call_ms > 50 {
                        warn!(call_ms, "VDD ping slow");
                    }
                    failures = 0;
                } else {
                    failures += 1;
                    if failures == 1 || failures % 100 == 0 {
                        warn!(failures, call_ms, "VDD ping failed");
                    }
                }
                if stop_c.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(PING_INTERVAL);
            }
            debug!("VDD ping loop stopped");
        });

        Ok(Self {
            handle,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for VddKeepalive {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // Best-effort unplug; if the handle is already dead the driver
        // unplugs on its own once pings stop.
        let input = [0u8; 32];
        let mut r = 0u32;
        let res = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_VDD_REMOVE,
                Some(input.as_ptr() as *const _),
                input.len() as u32,
                None,
                0,
                Some(&mut r),
                None,
            )
        };
        if let Err(e) = res {
            debug!(?e, "VDD unplug skipped");
        } else {
            info!("virtual display unplugged");
        }
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// Enumerate the first present VDD device interface and return its `\?\...`
/// device path.
fn find_device_path() -> Result<String> {
    unsafe {
        let guid = VDD_INTERFACE_GUID;
        let info_set = SetupDiGetClassDevsW(
            Some(&guid),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )?;
        if info_set.is_invalid() {
            bail!("no VDD interface class present (Parsec VDD driver not installed?)");
        }

        let mut iface = SP_DEVICE_INTERFACE_DATA {
            cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            InterfaceClassGuid: guid,
            Flags: Default::default(),
            Reserved: 0,
        };
        if SetupDiEnumDeviceInterfaces(info_set, None, &guid, 0, &mut iface).is_err() {
            let _ = SetupDiDestroyDeviceInfoList(info_set);
            bail!("SetupDiEnumDeviceInterfaces: no VDD device enumerated");
        }

        // First call: ask for the required buffer size (returns false + size).
        let mut needed = 0u32;
        let _ = SetupDiGetDeviceInterfaceDetailW(
            info_set,
            &iface,
            None,
            0,
            Some(&mut needed),
            None,
        );
        if needed < 8 {
            let _ = SetupDiDestroyDeviceInfoList(info_set);
            bail!("SetupDiGetDeviceInterfaceDetail: no buffer size reported");
        }

        let mut buf = vec![0u8; needed as usize];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        // cbSize is documented as 8 on Win64 / 6 on Win32 (DWORD + the start
        // of the path field), not `size_of` of the bindgen struct.
        let cb_size: u32 = if cfg!(target_pointer_width = "64") { 8 } else { 6 };
        (detail as *mut u32).write(cb_size);

        SetupDiGetDeviceInterfaceDetailW(
            info_set,
            &iface,
            Some(detail),
            needed,
            Some(&mut needed),
            None,
        )
        .context("SetupDiGetDeviceInterfaceDetail")?;
        let _ = SetupDiDestroyDeviceInfoList(info_set);

        // The device path string starts right after the cbSize DWORD.
        let path_ptr = buf.as_ptr().add(4) as *const u16;
        let max_chars = (buf.len() - 4) / 2;
        let mut len = 0usize;
        while len < max_chars && *path_ptr.add(len) != 0 {
            len += 1;
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(path_ptr, len)))
    }
}
