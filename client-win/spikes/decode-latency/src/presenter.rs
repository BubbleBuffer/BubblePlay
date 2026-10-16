use crate::{
    backend::DecodedFrame,
    timing::{frequency, qpc},
};
use anyhow::{bail, Context, Result};
use std::{
    ffi::c_void,
    mem::{size_of, ManuallyDrop},
};
use windows::{
    core::{w, Interface},
    Win32::{
        Foundation::*,
        Graphics::{
            Direct3D::*,
            Direct3D11::*,
            Dxgi::{Common::*, *},
            Gdi::*,
        },
        System::{LibraryLoader::GetModuleHandleW, Threading::CreateEventW},
        UI::WindowsAndMessaging::*,
    },
};

pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_CLOSE {
        let _ = DestroyWindow(hwnd);
        return LRESULT(0);
    }
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}
pub fn pump() -> Result<()> {
    unsafe {
        let mut m = MSG::default();
        while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
            if m.message == WM_QUIT {
                bail!("measurement window closed");
            }
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    }
    Ok(())
}

// Kernel waits, awakened only by the event or actual HWND messages.
fn wait_event(handle: HANDLE, timeout_ms: u32, label: &str) -> Result<()> {
    let end = qpc() + frequency() * timeout_ms as i64 / 1000;
    loop {
        let remaining = ((end - qpc()).max(0) * 1000 / frequency()).max(1) as u32;
        let result = unsafe {
            MsgWaitForMultipleObjectsEx(
                Some(&[handle]),
                remaining,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
        if result == WAIT_OBJECT_0 {
            return Ok(());
        }
        if result.0 == WAIT_OBJECT_0.0 + 1 {
            pump()?;
            if qpc() < end {
                continue;
            }
        }
        bail!("{label} wait failed/timed out: {:?}", result);
    }
}

pub struct Presenter {
    pub device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    swap: IDXGISwapChain3,
    hwnd: HWND,
    wait: HANDLE,
    context4: ID3D11DeviceContext4,
    fence: ID3D11Fence,
    fence_event: HANDLE,
    fence_value: u64,
    pub gpu_fence_waits: u64,
    pub wait_timeouts: u64,
    pub gpu_polls: u64,
    pub adapter: String,
    pub display_mode: String,
    pub display_details: String,
}
#[derive(Default, Clone)]
pub struct PresentRecord {
    pub qpc: i64,
    pub present_id: u32,
    pub stats: Option<DXGI_FRAME_STATISTICS>,
    pub stats_error: Option<i32>,
    pub media: Option<DXGI_FRAME_STATISTICS_MEDIA>,
}
impl Presenter {
    pub fn new(title: &str) -> Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let cls = w!("BubblePlayDecodeLatencySpike");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: cls,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let mut rect = RECT {
                left: 0,
                top: 0,
                // Keep an ordinary window fully on this 1080p desktop. The backing
                // buffer and video processor stay 1920x1080; DXGI/DWM scales to HWND.
                right: 1280,
                bottom: 720,
            };
            let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
            AdjustWindowRect(&mut rect, style, false)?;
            let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                cls,
                windows::core::PCWSTR(title.as_ptr()),
                style,
                40,
                40,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                instance,
                None,
            )?;
            let _ = ShowWindow(hwnd, SW_SHOW);
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.context("D3D device")?;
            let context = context.context("D3D context")?;
            let expected_context: windows::core::IUnknown = device.GetImmediateContext()?.cast()?;
            let actual_context: windows::core::IUnknown = context.cast()?;
            if expected_context != actual_context {
                bail!("presenter context isn't the device immediate context");
            }
            let multithread: ID3D11Multithread = context.cast()?;
            let _ = multithread.SetMultithreadProtected(true);
            let dxdevice: IDXGIDevice = device.cast()?;
            let adapter = dxdevice.GetAdapter()?;
            let desc = adapter.GetDesc()?;
            let adapter_name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_owned();
            let factory: IDXGIFactory5 = adapter.GetParent()?;
            let mut tearing = BOOL(0);
            factory.CheckFeatureSupport(
                DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                &mut tearing as *mut _ as *mut c_void,
                size_of::<BOOL>() as u32,
            )?;
            if !tearing.as_bool() {
                bail!("windowed tearing unavailable");
            }
            let sd = DXGI_SWAP_CHAIN_DESC1 {
                Width: WIDTH,
                Height: HEIGHT,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: (DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING
                    | DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT)
                    .0 as u32,
                ..Default::default()
            };
            let swap: IDXGISwapChain3 = factory
                .CreateSwapChainForHwnd(&device, hwnd, &sd, None, None)?
                .cast()?;
            factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER)?;
            swap.SetMaximumFrameLatency(1)?;
            let wait = swap.GetFrameLatencyWaitableObject();
            if wait.is_invalid() {
                bail!("frame latency waitable missing");
            }
            let output = swap.GetContainingOutput()?.GetDesc()?;
            let mut dm = DEVMODEW {
                dmSize: size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            let display_mode = if EnumDisplaySettingsW(
                windows::core::PCWSTR(output.DeviceName.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut dm,
            )
            .as_bool()
            {
                format!(
                    "{}x{} {}Hz; desktop={:?}",
                    dm.dmPelsWidth,
                    dm.dmPelsHeight,
                    dm.dmDisplayFrequency,
                    output.DesktopCoordinates
                )
            } else {
                format!("desktop={:?}; mode unavailable", output.DesktopCoordinates)
            };
            let display_details = crate::display::report(&device, hwnd)?;
            let video_device: ID3D11VideoDevice = device.cast()?;
            let video_context: ID3D11VideoContext = context.cast()?;
            let cd = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                InputWidth: WIDTH,
                InputHeight: HEIGHT,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                OutputWidth: WIDTH,
                OutputHeight: HEIGHT,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&cd)?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let vc1: ID3D11VideoContext1 = video_context.cast()?;
            vc1.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
            vc1.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            let device5: ID3D11Device5 = device
                .cast()
                .context("ID3D11Device5 required for GPU fence")?;
            let context4: ID3D11DeviceContext4 = context
                .cast()
                .context("ID3D11DeviceContext4 required for GPU fence")?;
            let mut fence: Option<ID3D11Fence> = None;
            device5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence)?;
            let fence_event = CreateEventW(None, false, false, windows::core::PCWSTR::null())?;
            Ok(Self {
                device,
                context,
                video_device,
                video_context,
                processor,
                enumerator,
                swap,
                hwnd,
                wait,
                context4,
                fence: fence.context("null fence")?,
                fence_event,
                fence_value: 0,
                gpu_fence_waits: 0,
                wait_timeouts: 0,
                gpu_polls: 0,
                adapter: adapter_name,
                display_mode,
                display_details,
            })
        }
    }
    // Both decoders enqueue video work on this device's single immediate context.
    // Signal follows their DecodeEndFrame commands; the native event observes the
    // GPU timeline without a probe copy or CPU query polling.
    pub fn decode_complete(&mut self, frame: &DecodedFrame) -> Result<i64> {
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            frame.texture.GetDesc(&mut desc);
            if desc.Format != DXGI_FORMAT_NV12 {
                bail!("decoder surface is not NV12");
            }
            let source: windows::core::IUnknown = frame.texture.GetDevice()?.cast()?;
            let expected: windows::core::IUnknown = self.device.cast()?;
            if source != expected {
                bail!("decoder returned surface on another device");
            }
            self.device
                .GetDeviceRemovedReason()
                .context("device removed before decode fence")?;
            self.fence_value += 1;
            self.fence
                .SetEventOnCompletion(self.fence_value, self.fence_event)?;
            self.context4.Signal(&self.fence, self.fence_value)?;
            self.context.Flush();
            self.gpu_fence_waits += 1;
            if let Err(error) = wait_event(self.fence_event, 10_000, "decode fence") {
                bail!(
                    "{error:#}; expected_fence={}, completed_fence={}, device_removed_reason={:?}",
                    self.fence_value,
                    self.fence.GetCompletedValue(),
                    self.device.GetDeviceRemovedReason()
                );
            }
            Ok(qpc())
        }
    }
    pub fn present(
        &mut self,
        frame: &DecodedFrame,
        index: u32,
        vsync: bool,
    ) -> Result<PresentRecord> {
        unsafe {
            wait_event(self.wait, 2000, "frame latency waitable")?;
            let mut iv = None;
            let id = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: frame.array_slice,
                    },
                },
            };
            self.video_device.CreateVideoProcessorInputView(
                &frame.texture,
                &self.enumerator,
                &id,
                Some(&mut iv),
            )?;
            let back: ID3D11Texture2D =
                self.swap.GetBuffer(self.swap.GetCurrentBackBufferIndex())?;
            let od = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut ov = None;
            self.video_device.CreateVideoProcessorOutputView(
                &back,
                &self.enumerator,
                &od,
                Some(&mut ov),
            )?;
            let rect = RECT {
                left: 0,
                top: 0,
                right: WIDTH as i32,
                bottom: HEIGHT as i32,
            };
            self.video_context.VideoProcessorSetStreamSourceRect(
                &self.processor,
                0,
                true,
                Some(&rect),
            );
            self.video_context.VideoProcessorSetStreamDestRect(
                &self.processor,
                0,
                true,
                Some(&rect),
            );
            self.video_context.VideoProcessorSetOutputTargetRect(
                &self.processor,
                true,
                Some(&rect),
            );
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: BOOL(1),
                InputFrameOrField: index,
                pInputSurface: ManuallyDrop::new(iv),
                ..Default::default()
            };
            let result = self.video_context.VideoProcessorBlt(
                &self.processor,
                ov.as_ref().unwrap(),
                index,
                std::slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result?;
            let hr = self.swap.Present(
                if vsync { 1 } else { 0 },
                if vsync {
                    DXGI_PRESENT(0)
                } else {
                    DXGI_PRESENT_ALLOW_TEARING
                },
            );
            let presented = qpc();
            hr.ok()?;
            if hr != windows::core::HRESULT(0) {
                bail!("Present status {:?}; measurement invalid (occluded?)", hr);
            }
            let present_id = self.swap.GetLastPresentCount()?;
            let mut stats = DXGI_FRAME_STATISTICS::default();
            let (stats, stats_error) = match self.swap.GetFrameStatistics(&mut stats) {
                Ok(()) => (Some(stats), None),
                Err(e) => (None, Some(e.code().0)),
            };
            let mut media = DXGI_FRAME_STATISTICS_MEDIA::default();
            let media = self
                .swap
                .cast::<IDXGISwapChainMedia>()
                .ok()
                .and_then(|s| s.GetFrameStatisticsMedia(&mut media).ok().map(|_| media));
            Ok(PresentRecord {
                qpc: presented,
                present_id,
                stats,
                stats_error,
                media,
            })
        }
    }
    pub fn final_stats(&self) -> Option<DXGI_FRAME_STATISTICS> {
        unsafe {
            let mut s = DXGI_FRAME_STATISTICS::default();
            self.swap.GetFrameStatistics(&mut s).ok().map(|_| s)
        }
    }
    // Read only our synthetic swap-chain buffer after timing is over. This never
    // captures the desktop. Verify video processing produced varied RGB pixels.
    pub fn rgb_check(&self) -> Result<String> {
        unsafe {
            let back: ID3D11Texture2D =
                self.swap.GetBuffer(self.swap.GetCurrentBackBufferIndex())?;
            let mut d = D3D11_TEXTURE2D_DESC::default();
            back.GetDesc(&mut d);
            d.Usage = D3D11_USAGE_STAGING;
            d.BindFlags = 0;
            d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            d.MiscFlags = 0;
            let mut staging = None;
            self.device.CreateTexture2D(&d, None, Some(&mut staging))?;
            let staging = staging.unwrap();
            self.context.CopyResource(&staging, &back);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let mut min = 255u8;
            let mut max = 0u8;
            let mut sum = 0u64;
            let mut count = 0u64;
            let mut hash = 14695981039346656037u64;
            for y in (0..HEIGHT as usize).step_by(8) {
                for x in (0..WIDTH as usize).step_by(8) {
                    for c in 0..3 {
                        let v = *((m.pData as *const u8).add(y * m.RowPitch as usize + x * 4 + c));
                        min = min.min(v);
                        max = max.max(v);
                        sum += v as u64;
                        count += 1;
                        hash = (hash ^ v as u64).wrapping_mul(1099511628211);
                    }
                }
            }
            self.context.Unmap(&staging, 0);
            if max.saturating_sub(min) < 32 {
                bail!("video-processor RGB output insufficient variation: min={min}, max={max}");
            }
            Ok(format!(
                "RGB sampled_min={min},max={max},mean={:.3},fnv64={hash:016x},samples={count}",
                sum as f64 / count as f64
            ))
        }
    }
}
impl Drop for Presenter {
    fn drop(&mut self) {
        unsafe {
            self.context.ClearState();
            self.context.Flush();
            let _ = CloseHandle(self.wait);
            let _ = CloseHandle(self.fence_event);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
