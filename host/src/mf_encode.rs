//! Windows inbox H.264 encoder and asynchronous hardware MFTs.
//!
//! MF exports are resolved at runtime: missing Media Feature Pack is a normal
//! backend-init failure, including on Windows N. No MF export is statically
//! referenced here. A synchronous public call owns exactly one input/output
//! picture; asynchronous MFT event waits never hold the device gate.

use crate::{
    encode::{EncodeStageTimings, EncodedFrame},
    gpu_gate::{self, GpuGate},
    mf_color::{matrix_for_nvenc, ColorMatrix, CpuConverter, Nv12Converter},
};
use anyhow::{ensure, Context, Result};
use std::{
    ffi::c_void,
    mem::ManuallyDrop,
    ptr,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};
use windows::core::{s, w, Interface, GUID, HRESULT, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Variant::VARIANT;

const CLSID_MICROSOFT_H264: GUID = GUID::from_u128(0x6ca50344_051a_4ded_9779_a43305165e35);
const EVENT_TIMEOUT: Duration = Duration::from_secs(3);

type CreateObject = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type CreateAttributes = unsafe extern "system" fn(*mut *mut c_void, u32) -> HRESULT;
type CreateBuffer = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;
type CreateAlignedBuffer = unsafe extern "system" fn(u32, u32, *mut *mut c_void) -> HRESULT;
type CreateSurfaceBuffer =
    unsafe extern "system" fn(*const GUID, *mut c_void, u32, i32, *mut *mut c_void) -> HRESULT;
type CreateManager = unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> HRESULT;
type EnumEx = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut *mut *mut c_void,
    *mut u32,
) -> HRESULT;
type Enum2 = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut c_void,
    *mut *mut *mut c_void,
    *mut u32,
) -> HRESULT;

struct Runtime {
    // Retain mfplat for the process lifetime, alongside its one MFStartup.
    _module: HMODULE,
    media_type: CreateObject,
    sample: CreateObject,
    attributes: CreateAttributes,
    buffer: CreateBuffer,
    aligned_buffer: CreateAlignedBuffer,
    surface_buffer: CreateSurfaceBuffer,
    manager: CreateManager,
    enumerate: EnumEx,
    enumerate_adapter: Option<Enum2>,
}
unsafe impl Send for Runtime {}
unsafe impl Sync for Runtime {}

impl Runtime {
    fn get() -> Result<&'static Self> {
        static RUNTIME: OnceLock<std::result::Result<Runtime, String>> = OnceLock::new();
        RUNTIME
            .get_or_init(|| Self::load().map_err(|e| format!("{e:#}")))
            .as_ref()
            .map_err(|e| {
                anyhow::anyhow!(
                    "Media Foundation unavailable: {e}. Windows N requires the Media Feature Pack"
                )
            })
    }
    fn load() -> Result<Self> {
        let module =
            unsafe { LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
                .context("load mfplat.dll")?;
        unsafe fn export<T: Copy>(module: HMODULE, name: PCSTR) -> Result<T> {
            let address = unsafe { GetProcAddress(module, name) }.context("MF export missing")?;
            Ok(unsafe { std::mem::transmute_copy(&address) })
        }
        // Resolve all required functions before startup. MFShutdown is not
        // called while another host session may still hold MF objects.
        let runtime = unsafe {
            Self {
                _module: module,
                media_type: export(module, s!("MFCreateMediaType"))?,
                sample: export(module, s!("MFCreateSample"))?,
                attributes: export(module, s!("MFCreateAttributes"))?,
                buffer: export(module, s!("MFCreateMemoryBuffer"))?,
                aligned_buffer: export(module, s!("MFCreateAlignedMemoryBuffer"))?,
                surface_buffer: export(module, s!("MFCreateDXGISurfaceBuffer"))?,
                manager: export(module, s!("MFCreateDXGIDeviceManager"))?,
                enumerate: export(module, s!("MFTEnumEx"))?,
                enumerate_adapter: export(module, s!("MFTEnum2")).ok(),
            }
        };
        let startup: unsafe extern "system" fn(u32, u32) -> HRESULT =
            unsafe { export(module, s!("MFStartup"))? };
        unsafe { startup(0x0002_0070, 0).ok() }.context("MFStartup")?;
        Ok(runtime)
    }
    fn object<T: Interface>(&self, create: CreateObject) -> Result<T> {
        let mut raw = ptr::null_mut();
        unsafe { create(&mut raw).ok() }?;
        ensure!(!raw.is_null(), "MF returned no object");
        Ok(unsafe { T::from_raw(raw) })
    }
    fn media_type(&self) -> Result<IMFMediaType> {
        self.object(self.media_type)
    }
    fn sample(&self) -> Result<IMFSample> {
        self.object(self.sample)
    }
    fn attributes(&self) -> Result<IMFAttributes> {
        let mut raw = ptr::null_mut();
        unsafe { (self.attributes)(&mut raw, 4).ok() }?;
        ensure!(!raw.is_null(), "MFCreateAttributes returned no object");
        Ok(unsafe { IMFAttributes::from_raw(raw) })
    }
    fn buffer(&self, len: u32, alignment: u32) -> Result<IMFMediaBuffer> {
        let mut raw = ptr::null_mut();
        unsafe {
            if alignment > 1 {
                (self.aligned_buffer)(len, alignment - 1, &mut raw).ok()?;
            } else {
                (self.buffer)(len, &mut raw).ok()?;
            }
        }
        ensure!(!raw.is_null(), "MFCreateMemoryBuffer returned no object");
        Ok(unsafe { IMFMediaBuffer::from_raw(raw) })
    }
    fn surface_buffer(&self, texture: &ID3D11Texture2D) -> Result<IMFMediaBuffer> {
        let mut raw = ptr::null_mut();
        unsafe {
            (self.surface_buffer)(&ID3D11Texture2D::IID, texture.as_raw(), 0, 0, &mut raw).ok()
        }?;
        ensure!(
            !raw.is_null(),
            "MFCreateDXGISurfaceBuffer returned no object"
        );
        let buffer = unsafe { IMFMediaBuffer::from_raw(raw) };
        // DXGI buffers are created with zero current length. Certified MFTs
        // may inspect it even though the sample remains on the GPU.
        let buffer2d: IMF2DBuffer = buffer.cast()?;
        let length = unsafe { buffer2d.GetContiguousLength() }?;
        unsafe { buffer.SetCurrentLength(length) }?;
        Ok(buffer)
    }
    fn manager(&self, device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
        let mut raw = ptr::null_mut();
        let mut token = 0;
        unsafe { (self.manager)(&mut token, &mut raw).ok() }?;
        ensure!(
            !raw.is_null(),
            "MFCreateDXGIDeviceManager returned no object"
        );
        let manager = unsafe { IMFDXGIDeviceManager::from_raw(raw) };
        unsafe { manager.ResetDevice(device, token) }?;
        Ok(manager)
    }
    fn enumerate(&self, device: &ID3D11Device, adapter_only: bool) -> Result<Vec<IMFActivate>> {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let flags = (MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0) as u32;
        let mut raw: *mut *mut c_void = ptr::null_mut();
        let mut count = 0;
        let result = if adapter_only {
            let enumerate = self
                .enumerate_adapter
                .context("MFTEnum2 unavailable on this Windows version")?;
            let dxgi: IDXGIDevice = device.cast()?;
            let desc = unsafe { dxgi.GetAdapter()?.GetDesc() }?;
            let luid = (u64::from(desc.AdapterLuid.HighPart as u32) << 32)
                | u64::from(desc.AdapterLuid.LowPart);
            let attributes = self.attributes()?;
            unsafe { attributes.SetUINT64(&MFT_ENUM_ADAPTER_LUID, luid) }?;
            unsafe {
                enumerate(
                    MFT_CATEGORY_VIDEO_ENCODER,
                    flags,
                    &input,
                    &output,
                    attributes.as_raw(),
                    &mut raw,
                    &mut count,
                )
            }
        } else {
            unsafe {
                (self.enumerate)(
                    MFT_CATEGORY_VIDEO_ENCODER,
                    flags,
                    &input,
                    &output,
                    &mut raw,
                    &mut count,
                )
            }
        };
        // The returned array is task-allocated; each interface reference is
        // separately owned. Release both even if enumeration reports an error.
        let mut activations = Vec::new();
        if !raw.is_null() {
            for index in 0..count as usize {
                let entry = unsafe { *raw.add(index) };
                if !entry.is_null() {
                    activations.push(unsafe { IMFActivate::from_raw(entry) });
                }
            }
            unsafe { CoTaskMemFree(Some(raw.cast())) };
        }
        result.ok().context("enumerate hardware H.264 MFT")?;
        Ok(activations)
    }
}

struct ComApartment(bool);
impl ComApartment {
    fn enter() -> Result<Self> {
        let status = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if status == HRESULT(0x8001_0106u32 as i32) {
            return Ok(Self(false));
        }
        status.ok().context("initialize COM for MF encoder")?;
        Ok(Self(true))
    }
}
impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

struct Transform {
    encoder: IMFTransform,
    codec: ICodecAPI,
    events: Option<IMFMediaEventGenerator>,
    activation: Option<IMFActivate>,
    _manager: Option<IMFDXGIDeviceManager>,
    input_id: u32,
    output_id: u32,
    input_credit: u32,
    output_credit: u32,
    name: String,
    label: String,
    gate: Arc<GpuGate>,
}
impl Drop for Transform {
    fn drop(&mut self) {
        let _gpu = self.gate.lock();
        unsafe {
            let _ = self
                .encoder
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.encoder.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .encoder
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Some(activation) = &self.activation {
                let _ = activation.ShutdownObject();
            }
        }
    }
}

fn set_codec(
    codec: &ICodecAPI,
    property: &GUID,
    value: VARIANT,
    label: &'static str,
    required: bool,
) -> Result<()> {
    let result = unsafe { codec.SetValue(property, &value) };
    match result {
        Ok(()) => Ok(()),
        Err(error) if !required => {
            warn!(%error, property = label, "MF encoder rejected requested optional property");
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("MF {label}")),
    }
}

fn media_type(
    runtime: &Runtime,
    width: u32,
    height: u32,
    fps: u32,
    subtype: &GUID,
) -> Result<IMFMediaType> {
    let media = runtime.media_type()?;
    unsafe {
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(width) << 32) | u64::from(height),
        )?;
        media.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1)?;
        media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // NVENC's current SPS omits video_signal_type. Do not declare output
        // colour metadata that would introduce new SPS range/matrix fields.
        // Input is already converted NV12, with its actual matrix explicit.
        if *subtype == MFVideoFormat_NV12 {
            media.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            media.SetUINT32(
                &MF_MT_YUV_MATRIX,
                match matrix_for_nvenc(width, height) {
                    ColorMatrix::Bt601 => MFVideoTransferMatrix_BT601.0 as u32,
                    ColorMatrix::Bt709 => MFVideoTransferMatrix_BT709.0 as u32,
                },
            )?;
        }
    }
    Ok(media)
}

impl Transform {
    #[allow(clippy::too_many_arguments)]
    fn configure(
        runtime: &Runtime,
        device: &ID3D11Device,
        encoder: IMFTransform,
        activation: Option<IMFActivate>,
        name: String,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        infinite_gop: bool,
        gop_seconds: u8,
    ) -> Result<Self> {
        let gate = gpu_gate::for_device(device);
        let _gpu = gate.lock();
        let attributes = unsafe { encoder.GetAttributes() }.context("MF encoder attributes")?;
        let is_async = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        let events = if is_async {
            unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }?;
            Some(
                encoder
                    .cast::<IMFMediaEventGenerator>()
                    .context("asynchronous MF event generator")?,
            )
        } else {
            None
        };
        ensure!(
            activation.is_none() || is_async,
            "hardware MF encoder must expose asynchronous events"
        );
        let _ = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) };
        let codec: ICodecAPI = encoder.cast().context("MF encoder ICodecAPI")?;
        set_codec(
            &codec,
            &CODECAPI_AVEncCommonRateControlMode,
            VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
            "CBR rate control",
            true,
        )?;
        set_codec(
            &codec,
            &CODECAPI_AVEncMPVDefaultBPictureCount,
            VARIANT::from(0u32),
            "zero B pictures",
            false,
        )?;
        set_codec(
            &codec,
            &CODECAPI_AVLowLatencyMode,
            VARIANT::from(true),
            "low latency VT_BOOL TRUE",
            true,
        )?;
        set_codec(
            &codec,
            &CODECAPI_AVEncCommonMeanBitRate,
            VARIANT::from(bitrate),
            "mean bitrate",
            true,
        )?;
        set_codec(
            &codec,
            &CODECAPI_AVEncCommonBufferSize,
            VARIANT::from(one_frame_buffer(bitrate, fps)),
            "one-frame CBR buffer (bytes)",
            true,
        )?;
        set_codec(
            &codec,
            &CODECAPI_AVEncMPVGOPSize,
            VARIANT::from(gop_frames(fps, gop_seconds, infinite_gop)),
            "GOP size",
            true,
        )?;
        set_codec(
            &codec,
            &CODECAPI_AVEncMPVSceneDetection,
            VARIANT::from(0u32),
            "disable scene-detection IDRs",
            false,
        )?;
        set_codec(
            &codec,
            &CODECAPI_AVEncH264CABACEnable,
            VARIANT::from(true),
            "CABAC",
            false,
        )?;
        set_codec(
            &codec,
            &CODECAPI_AVEncCommonQualityVsSpeed,
            VARIANT::from(0u32),
            "quality/speed",
            false,
        )?;
        if activation.is_none() {
            let workers = std::thread::available_parallelism()
                .map(|n| (n.get() / 2).clamp(1, 8) as u32)
                .unwrap_or(1);
            set_codec(
                &codec,
                &CODECAPI_AVEncNumWorkerThreads,
                VARIANT::from(workers),
                "bounded worker threads",
                false,
            )?;
        }
        let manager = if activation.is_some() {
            ensure!(
                unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) != 0,
                "hardware MF encoder does not accept D3D11 textures"
            );
            let manager = runtime.manager(device)?;
            unsafe {
                encoder.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
            }?;
            Some(manager)
        } else {
            None
        };
        let mut inputs = [0];
        let mut outputs = [0];
        // E_NOTIMPL means the standard single-stream IDs are both zero.
        match unsafe { encoder.GetStreamIDs(&mut inputs, &mut outputs) } {
            Ok(()) => {}
            Err(error) if error.code() == HRESULT(0x8000_4001u32 as i32) => {}
            Err(error) => return Err(error).context("MF stream IDs"),
        }
        let (input_id, output_id) = (inputs[0], outputs[0]);
        let output = media_type(runtime, width, height, fps, &MFVideoFormat_H264)?;
        unsafe {
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, 100)?;
            // No MF_MT_MPEG2_LEVEL: the MFT chooses a valid level itself.
            encoder.SetOutputType(output_id, &output, 0)?;
        }
        let input = media_type(runtime, width, height, fps, &MFVideoFormat_NV12)?;
        unsafe {
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width)?;
            input.SetUINT32(&MF_MT_SAMPLE_SIZE, width * height * 3 / 2)?;
            input.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
            input.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            encoder.SetInputType(input_id, &input, 0)?;
            encoder.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            encoder.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        drop(_gpu);
        let label = activation
            .as_ref()
            .map(hardware_backend_name)
            .unwrap_or_else(|| "Microsoft software H.264".into());
        Ok(Self {
            encoder,
            codec,
            events,
            activation,
            _manager: manager,
            input_id,
            output_id,
            input_credit: 0,
            output_credit: 0,
            name,
            label,
            gate,
        })
    }

    fn wait_event(&mut self, need_output: bool, deadline: Instant) -> Result<()> {
        let Some(events) = &self.events else {
            return Ok(());
        };
        while if need_output {
            self.output_credit == 0
        } else {
            self.input_credit == 0
        } {
            ensure!(
                Instant::now() < deadline,
                "MF asynchronous {} timed out",
                if need_output { "output" } else { "input" }
            );
            // Poll non-blocking GetEvent outside GpuGate. A driver that stops
            // producing events cannot indefinitely block the capture device.
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => return Err(error).context("MF encoder GetEvent"),
            };
            unsafe { event.GetStatus()?.ok() }.context("MF asynchronous event status")?;
            match unsafe { event.GetType() }? {
                601 => self.input_credit = self.input_credit.saturating_add(1),
                602 => self.output_credit = self.output_credit.saturating_add(1),
                _ => {}
            }
        }
        Ok(())
    }

    fn sequence_params(&self) -> Result<Vec<u8>> {
        let _gpu = self.gate.lock();
        let output = unsafe { self.encoder.GetOutputCurrentType(self.output_id) }?;
        let length = match unsafe { output.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) } {
            Ok(length) => length,
            Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => return Ok(Vec::new()),
            Err(error) => return Err(error).context("MF sequence header size"),
        };
        let mut data = vec![0; length as usize];
        unsafe { output.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut data, None) }?;
        Ok(data)
    }

    fn output(&self, runtime: &Runtime, raw_size: u32) -> Result<IMFSample> {
        let _gpu = self.gate.lock();
        let info = unsafe { self.encoder.GetOutputStreamInfo(self.output_id) }?;
        let sample = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
            let sample = runtime.sample()?;
            let buffer = runtime.buffer(info.cbSize.max(raw_size).max(65536), info.cbAlignment)?;
            unsafe { sample.AddBuffer(&buffer) }?;
            Some(sample)
        } else {
            None
        };
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_id,
            pSample: ManuallyDrop::new(sample),
            ..Default::default()
        };
        let mut status = 0;
        let result = unsafe {
            self.encoder
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };
        let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
        unsafe { ManuallyDrop::drop(&mut output.pEvents) };
        result.context("MF ProcessOutput (one input must produce one picture)")?;
        ensure!(
            output.dwStatus & MFT_OUTPUT_DATA_BUFFER_INCOMPLETE.0 as u32 == 0,
            "MF returned incomplete picture; delayed/multiple-output encoders are unsupported"
        );
        sample.context("MF ProcessOutput produced no sample")
    }
}

fn gop_frames(fps: u32, seconds: u8, infinite: bool) -> u32 {
    if infinite {
        u32::MAX
    } else {
        fps.saturating_mul(u32::from(seconds)).max(1)
    }
}
fn one_frame_buffer(bitrate: u32, fps: u32) -> u32 {
    (u64::from(bitrate).div_ceil(u64::from(fps.max(1)) * 8)).clamp(1, u64::from(u32::MAX)) as u32
}

pub struct MfEncoder {
    transform: Transform,
    converter: Option<Nv12Converter>,
    cpu_converter: Option<CpuConverter>,
    device: ID3D11Device,
    runtime: &'static Runtime,
    nv12: Vec<u8>,
    sequence: Vec<u8>,
    width: u32,
    height: u32,
    fps: u32,
    applied_bitrate: u32,
    desired_bitrate: u32,
    desired_profile: Option<(u32, u32, u32)>,
    reset_pending: bool,
    infinite_gop: bool,
    hardware: bool,
    gop_seconds: u8,
    force_idr_next: bool,
    frame_index: u64,
    // COM initialization remains alive until all encoder interfaces are gone.
    _com: ComApartment,
}

impl MfEncoder {
    pub fn new(
        device: *mut c_void,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        infinite_gop: bool,
        hardware: bool,
    ) -> Result<Self> {
        Self::new_with_gop(
            device,
            width,
            height,
            fps,
            bitrate,
            infinite_gop,
            hardware,
            2,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_gop(
        device: *mut c_void,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        infinite_gop: bool,
        hardware: bool,
        gop_seconds: u8,
    ) -> Result<Self> {
        ensure!(!device.is_null(), "MF requires a capture D3D11 device");
        validate_profile(width, height, fps)?;
        ensure!(
            bitrate > 0 && matches!(gop_seconds, 1 | 2 | 4),
            "invalid MF bitrate/GOP"
        );
        let com = ComApartment::enter()?;
        let runtime = Runtime::get()?;
        let device = unsafe { ID3D11Device::from_raw_borrowed(&device) }
            .context("MF capture device")?
            .clone();
        gpu_gate::protect_device(&device)?;
        let converter = match Nv12Converter::new(&device, width, height, fps) {
            Ok(converter) => Some(converter),
            Err(error) if !hardware => {
                warn!(%error, "MF software uses CPU BGRA -> NV12 conversion");
                None
            }
            Err(error) => return Err(error).context("hardware MF GPU NV12 converter"),
        };
        let transform = if hardware {
            let mut failures = Vec::new();
            let mut selected = None;
            // Try the capture adapter first, then merit-sorted global hardware
            // MFTs. Rejected candidates never select software implicitly.
            for adapter_only in [true, false] {
                let activations = match runtime.enumerate(&device, adapter_only) {
                    Ok(activations) => activations,
                    Err(error) => {
                        failures.push(format!("enumerate adapter_only={adapter_only}: {error:#}"));
                        continue;
                    }
                };
                for activation in activations {
                    let name = activation_name(&activation);
                    let result = (|| {
                        let encoder = unsafe { activation.ActivateObject::<IMFTransform>() }?;
                        Transform::configure(
                            runtime,
                            &device,
                            encoder,
                            Some(activation.clone()),
                            name.clone(),
                            width,
                            height,
                            fps,
                            bitrate,
                            infinite_gop,
                            gop_seconds,
                        )
                    })();
                    match result {
                        Ok(transform) => {
                            selected = Some(transform);
                            break;
                        }
                        Err(error) => {
                            let _gpu = gpu_gate::for_device(&device);
                            let _guard = _gpu.lock();
                            let _ = unsafe { activation.ShutdownObject() };
                            failures.push(format!("{name}: {error:#}"));
                        }
                    }
                }
                if selected.is_some() {
                    break;
                }
            }
            selected.with_context(|| {
                format!(
                    "no usable hardware MF H.264 encoder: {}",
                    failures.join("; ")
                )
            })?
        } else {
            let encoder: IMFTransform =
                unsafe { CoCreateInstance(&CLSID_MICROSOFT_H264, None, CLSCTX_INPROC_SERVER) }
                    .context("Microsoft inbox H.264 encoder (mfh264enc.dll)")?;
            Transform::configure(
                runtime,
                &device,
                encoder,
                None,
                "Microsoft H.264 Video Encoder MFT".into(),
                width,
                height,
                fps,
                bitrate,
                infinite_gop,
                gop_seconds,
            )?
        };
        let cpu_converter = if converter.is_none() {
            Some(CpuConverter::new(&device, width, height)?)
        } else {
            None
        };
        let sequence = transform.sequence_params()?;
        if !sequence.is_empty() {
            validate_sequence(&sequence)?;
        }
        info!(hardware, backend = %transform.label, implementation = %transform.name, width, height, fps, bitrate,
            gop_frames = gop_frames(fps, gop_seconds, infinite_gop), cbr_buffer_bytes = one_frame_buffer(bitrate, fps),
            "MF H.264 High encoder initialized");
        Ok(Self {
            transform,
            converter,
            cpu_converter,
            device,
            runtime,
            nv12: Vec::new(),
            sequence,
            width,
            height,
            fps,
            applied_bitrate: bitrate,
            desired_bitrate: bitrate,
            desired_profile: None,
            reset_pending: false,
            infinite_gop,
            hardware,
            gop_seconds,
            force_idr_next: true,
            frame_index: 0,
            _com: com,
        })
    }
    pub fn backend_name(&self) -> &str {
        &self.transform.label
    }
    pub fn implementation_name(&self) -> &str {
        &self.transform.name
    }
    pub fn force_idr(&mut self) {
        self.force_idr_next = true;
    }
    pub fn idr_pending(&self) -> bool {
        self.force_idr_next || self.desired_profile.is_some() || self.reset_pending
    }
    pub fn bitrate(&self) -> u32 {
        self.desired_bitrate
    }
    pub fn fps(&self) -> u32 {
        self.desired_profile.map(|p| p.2).unwrap_or(self.fps)
    }
    pub fn infinite_gop(&self) -> bool {
        self.infinite_gop
    }
    pub fn is_for_device(&self, device: *mut c_void) -> bool {
        self.device.as_raw() == device
    }
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        ensure!(bitrate > 0, "MF bitrate must be positive");
        self.desired_bitrate = bitrate;
        Ok(())
    }
    pub fn max_qp(&self) -> Option<u32> {
        None
    }
    pub fn set_max_qp(&mut self, max_qp: Option<u32>) -> Result<()> {
        debug!(
            ?max_qp,
            "MF QP ceiling unsupported; CBR controls remain in effect"
        );
        Ok(())
    }
    pub fn reset_rate_control(&mut self) -> Result<()> {
        self.reset_pending = true;
        Ok(())
    }
    pub fn reconfigure_profile(&mut self, width: u32, height: u32, fps: u32) -> Result<()> {
        validate_profile(width, height, fps)?;
        if (width, height, fps) != (self.width, self.height, self.fps) {
            self.desired_profile = Some((width, height, fps));
        } else {
            self.desired_profile = None;
        }
        Ok(())
    }
    fn apply_pending(&mut self) -> Result<()> {
        if self.desired_profile.is_some() || self.reset_pending {
            let (width, height, fps) =
                self.desired_profile
                    .unwrap_or((self.width, self.height, self.fps));
            let replacement = Self::new_with_gop(
                self.device.as_raw(),
                width,
                height,
                fps,
                self.desired_bitrate,
                self.infinite_gop,
                self.hardware,
                self.gop_seconds,
            )?;
            *self = replacement;
        } else if self.applied_bitrate != self.desired_bitrate {
            let _gpu = self.transform.gate.lock();
            set_codec(
                &self.transform.codec,
                &CODECAPI_AVEncCommonMeanBitRate,
                VARIANT::from(self.desired_bitrate),
                "live mean bitrate",
                true,
            )?;
            set_codec(
                &self.transform.codec,
                &CODECAPI_AVEncCommonBufferSize,
                VARIANT::from(one_frame_buffer(self.desired_bitrate, self.fps)),
                "live one-frame CBR buffer",
                true,
            )?;
            self.applied_bitrate = self.desired_bitrate;
        }
        Ok(())
    }
    pub fn sequence_params(&self) -> Result<Vec<u8>> {
        let data = self.transform.sequence_params()?;
        if data.is_empty() {
            return Ok(self.sequence.clone());
        }
        validate_sequence(&data)?;
        Ok(data)
    }
    pub fn encode_texture(&mut self, texture: *mut c_void) -> Result<EncodedFrame> {
        ensure!(!texture.is_null(), "MF input texture is null");
        self.apply_pending()?;
        let started = Instant::now();
        let input =
            unsafe { ID3D11Texture2D::from_raw_borrowed(&texture) }.context("MF input texture")?;
        let buffer = if self.hardware {
            let nv12 = self
                .converter
                .as_ref()
                .context("MF hardware converter missing")?
                .convert(input)?;
            self.runtime.surface_buffer(&nv12)?
        } else {
            let converted = if let Some(converter) = &mut self.converter {
                converter
                    .convert(input)
                    .and_then(|_| converter.readback(&mut self.nv12))
            } else {
                Err(anyhow::anyhow!("GPU NV12 converter unavailable"))
            };
            if let Err(error) = converted {
                if self.cpu_converter.is_none() {
                    warn!(%error, "MF software GPU NV12 readback failed; switching to CPU conversion");
                    self.cpu_converter =
                        Some(CpuConverter::new(&self.device, self.width, self.height)?);
                    self.converter = None;
                }
                self.cpu_converter
                    .as_ref()
                    .unwrap()
                    .convert(input, &mut self.nv12)?;
            }
            let buffer = self.runtime.buffer(self.nv12.len() as u32, 0)?;
            write_buffer(&buffer, &self.nv12)?;
            buffer
        };
        let sample = self.runtime.sample()?;
        let timestamp = ((u128::from(self.frame_index) * 10_000_000) / u128::from(self.fps)) as i64;
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(timestamp)?;
            sample.SetSampleDuration(10_000_000 / i64::from(self.fps))?;
        }
        let deadline = Instant::now() + EVENT_TIMEOUT;
        self.transform.wait_event(false, deadline)?;
        let submit_started = Instant::now();
        {
            let _gpu = self.transform.gate.lock();
            if self.force_idr_next {
                set_codec(
                    &self.transform.codec,
                    &CODECAPI_AVEncVideoForceKeyFrame,
                    VARIANT::from(1u32),
                    "force next IDR",
                    true,
                )?;
            }
            unsafe {
                self.transform
                    .encoder
                    .ProcessInput(self.transform.input_id, &sample, 0)
            }
            .context("MF ProcessInput")?;
        }
        self.transform.input_credit = self.transform.input_credit.saturating_sub(1);
        let submit_us = submit_started.elapsed().as_micros() as u64;
        self.transform.wait_event(true, deadline)?;
        let output_started = Instant::now();
        let output = self
            .transform
            .output(self.runtime, self.width * self.height * 2)?;
        self.transform.output_credit = self.transform.output_credit.saturating_sub(1);
        let lock_us = output_started.elapsed().as_micros() as u64;
        let output_time =
            unsafe { output.GetSampleTime() }.context("MF encoded sample timestamp")?;
        ensure!(
            output_time == timestamp,
            "MF reordered/delayed picture: input {timestamp}, output {output_time}"
        );
        let frame_avg_qp =
            unsafe { output.GetUINT64(&MFSampleExtension_VideoEncodeQP) }.unwrap_or(0) as u32;
        let buffer = unsafe { output.ConvertToContiguousBuffer() }?;
        let mut data = read_buffer(&buffer)?;
        let inspection = inspect_access_unit(&data)?;
        ensure!(
            inspection.access_units == 1 && !inspection.has_b_slice,
            "MF must produce exactly one non-B access unit per input"
        );
        ensure!(
            !self.force_idr_next || inspection.is_idr,
            "MF encoder ignored forced IDR"
        );
        ensure!(
            !self.infinite_gop || !inspection.is_idr || self.force_idr_next,
            "MF encoder produced an unsolicited IDR with infinite GOP"
        );
        if inspection.is_idr {
            let current = self.transform.sequence_params()?;
            if !current.is_empty() {
                self.sequence = current;
            }
            if let Some(params) = extract_sequence(&data) {
                self.sequence = params;
            }
            validate_sequence(&self.sequence)?;
            data = prepend_sequence_if_needed(data, &self.sequence)?;
        }
        self.frame_index += 1;
        self.force_idr_next = false;
        Ok(EncodedFrame {
            data,
            is_idr: inspection.is_idr,
            encode_us: started.elapsed().as_micros() as u64,
            qp_map: None,
            roi_activity: None,
            stage_timings: EncodeStageTimings {
                submit_us,
                lock_us,
                ..Default::default()
            },
            frame_avg_qp,
        })
    }
}

fn validate_profile(width: u32, height: u32, fps: u32) -> Result<()> {
    ensure!(
        width > 0
            && height > 0
            && width.is_multiple_of(2)
            && height.is_multiple_of(2)
            && width <= 8192
            && height <= 8192,
        "MF NV12 requires positive even dimensions at most 8192"
    );
    ensure!((1..=240).contains(&fps), "MF frame rate must be 1..240");
    Ok(())
}
fn activation_name(activation: &IMFActivate) -> String {
    activation_string(activation, &MFT_FRIENDLY_NAME_Attribute)
        .unwrap_or_else(|| "unnamed hardware H.264 MFT".into())
}
fn activation_string(activation: &IMFActivate, attribute: &GUID) -> Option<String> {
    let mut text = [0u16; 256];
    if unsafe { activation.GetString(attribute, &mut text, None) }.is_ok() {
        Some(String::from_utf16_lossy(
            &text[..text.iter().position(|&c| c == 0).unwrap_or(text.len())],
        ))
    } else {
        None
    }
}
fn hardware_backend_name(activation: &IMFActivate) -> String {
    let vendor = activation_string(activation, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let friendly = activation_name(activation);
    let lower = friendly.to_ascii_lowercase();
    if vendor.contains("10de") || lower.contains("nvidia") {
        "Media Foundation hardware H.264, NVIDIA".into()
    } else if vendor.contains("1002")
        || lower.contains("amd")
        || lower.contains("advanced micro devices")
    {
        "Media Foundation hardware H.264, AMD (experimental, untested)".into()
    } else if vendor.contains("8086") || lower.contains("intel") {
        "Media Foundation hardware H.264, Intel (experimental, untested)".into()
    } else {
        format!("Media Foundation hardware H.264, {friendly} (experimental, untested)")
    }
}
fn write_buffer(buffer: &IMFMediaBuffer, data: &[u8]) -> Result<()> {
    let mut raw = ptr::null_mut();
    let mut maximum = 0;
    unsafe { buffer.Lock(&mut raw, Some(&mut maximum), None) }?;
    let result = (|| {
        ensure!(
            !raw.is_null() && maximum as usize >= data.len(),
            "MF input buffer too small"
        );
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), raw, data.len()) };
        Ok(())
    })();
    unsafe { buffer.Unlock() }?;
    result?;
    unsafe { buffer.SetCurrentLength(data.len() as u32) }?;
    Ok(())
}
fn read_buffer(buffer: &IMFMediaBuffer) -> Result<Vec<u8>> {
    let mut raw = ptr::null_mut();
    let mut length = 0;
    unsafe { buffer.Lock(&mut raw, None, Some(&mut length)) }?;
    let result = if !raw.is_null() && length > 0 {
        Ok(unsafe { std::slice::from_raw_parts(raw, length as usize) }.to_vec())
    } else {
        Err(anyhow::anyhow!("MF encoded sample is empty"))
    };
    unsafe { buffer.Unlock() }?;
    result
}

/// Minimal bitstream checks used by the synthetic probe and the backend.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AccessUnitInspection {
    pub access_units: usize,
    pub is_idr: bool,
    pub has_b_slice: bool,
    pub sps_before_idr: bool,
    pub pps_before_idr: bool,
    pub profile_idc: Option<u8>,
}

pub fn annex_b_nals(data: &[u8]) -> Result<Vec<&[u8]>> {
    let mut starts = Vec::new();
    let mut pos = 0;
    while pos + 3 <= data.len() {
        let prefix = if data[pos..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[pos..].starts_with(&[0, 0, 1]) {
            3
        } else {
            0
        };
        if prefix > 0 {
            starts.push((pos, pos + prefix));
            pos += prefix;
        } else {
            pos += 1;
        }
    }
    ensure!(
        !starts.is_empty() && data[..starts[0].0].iter().all(|&v| v == 0),
        "MF H.264 output is not Annex B"
    );
    let mut nals = Vec::with_capacity(starts.len());
    for (index, &(_, payload)) in starts.iter().enumerate() {
        let end = starts.get(index + 1).map(|s| s.0).unwrap_or(data.len());
        ensure!(end > payload, "empty Annex B NAL");
        let nal = &data[payload..end];
        ensure!(nal[0] & 0x80 == 0, "invalid H.264 forbidden_zero_bit");
        nals.push(nal);
    }
    Ok(nals)
}
fn slice_header(nal: &[u8]) -> Result<(u32, u32)> {
    let mut rbsp = Vec::with_capacity(nal.len().min(32));
    let mut zeros = 0;
    for &byte in nal.iter().skip(1).take(32) {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        rbsp.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    let mut bit = 0;
    fn ue(data: &[u8], bit: &mut usize) -> Result<u32> {
        let next = |bit: &mut usize| -> Result<u32> {
            ensure!(*bit < data.len() * 8, "truncated H.264 slice header");
            let value = u32::from((data[*bit / 8] >> (7 - *bit % 8)) & 1);
            *bit += 1;
            Ok(value)
        };
        let mut zeroes = 0;
        while next(bit)? == 0 {
            zeroes += 1;
            ensure!(zeroes < 32, "invalid H.264 exp-Golomb code");
        }
        let mut code = 1u32;
        for _ in 0..zeroes {
            code = (code << 1) | next(bit)?;
        }
        Ok(code - 1)
    }
    Ok((ue(&rbsp, &mut bit)?, ue(&rbsp, &mut bit)?))
}
pub fn inspect_access_unit(data: &[u8]) -> Result<AccessUnitInspection> {
    let mut result = AccessUnitInspection::default();
    let mut before_picture = true;
    let mut sps = false;
    let mut pps = false;
    for nal in annex_b_nals(data)? {
        match nal[0] & 31 {
            7 => {
                ensure!(nal.len() >= 4, "truncated H.264 SPS");
                result.profile_idc = Some(nal[1]);
                if before_picture {
                    sps = true;
                }
            }
            8 => {
                if before_picture {
                    pps = true;
                }
            }
            1 | 5 => {
                let (first_mb, slice_type) = slice_header(nal)?;
                ensure!(slice_type <= 9, "invalid H.264 slice type");
                if first_mb == 0 {
                    result.access_units += 1;
                }
                result.has_b_slice |= slice_type % 5 == 1;
                if nal[0] & 31 == 5 {
                    result.is_idr = true;
                    result.sps_before_idr = sps;
                    result.pps_before_idr = pps;
                }
                before_picture = false;
            }
            _ => {}
        }
    }
    Ok(result)
}
fn extract_sequence(data: &[u8]) -> Option<Vec<u8>> {
    let mut params = Vec::new();
    let mut sps = false;
    let mut pps = false;
    for nal in annex_b_nals(data).ok()? {
        match nal[0] & 31 {
            7 => sps = true,
            8 => pps = true,
            _ => continue,
        }
        params.extend_from_slice(&[0, 0, 0, 1]);
        params.extend_from_slice(nal);
    }
    (sps && pps).then_some(params)
}
fn validate_sequence(data: &[u8]) -> Result<()> {
    let nals = annex_b_nals(data)?;
    ensure!(
        nals.iter().any(|nal| nal[0] & 31 == 8),
        "MF sequence header is missing PPS"
    );
    let sps = nals
        .iter()
        .find(|nal| nal[0] & 31 == 7)
        .context("MF sequence header is missing SPS")?;
    ensure!(
        sps.len() >= 4 && sps[1] == 100,
        "MF must emit H.264 High profile_idc=100"
    );
    Ok(())
}
fn prepend_sequence_if_needed(data: Vec<u8>, sequence: &[u8]) -> Result<Vec<u8>> {
    let inspect = inspect_access_unit(&data)?;
    if inspect.sps_before_idr && inspect.pps_before_idr {
        validate_sequence(&data)?;
        return Ok(data);
    }
    validate_sequence(sequence)?;
    let mut result = Vec::with_capacity(sequence.len() + data.len());
    // An access-unit delimiter, when present, must remain the first NAL of
    // its AU. Insert configuration immediately after it, before all VCL.
    let nals = annex_b_nals(&data)?;
    let insert = if nals[0][0] & 31 == 9 {
        nals[0].as_ptr() as usize - data.as_ptr() as usize + nals[0].len()
    } else {
        0
    };
    result.extend_from_slice(&data[..insert]);
    result.extend_from_slice(sequence);
    result.extend_from_slice(&data[insert..]);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn headers() -> Vec<u8> {
        vec![0, 0, 0, 1, 0x67, 100, 0, 40, 0x80, 0, 0, 1, 0x68, 0x80]
    }
    #[test]
    fn annex_b_multiple_slices_are_one_picture_b_slices_and_multiple_pictures_are_detected() {
        // ue(0), ue(2) = 1011; ue(1), ue(2) = 010011.
        let data = [headers(), vec![0, 0, 1, 0x65, 0xb0, 0, 0, 1, 0x65, 0x4c]].concat();
        let inspected = inspect_access_unit(&data).unwrap();
        assert_eq!(inspected.access_units, 1);
        assert!(inspected.is_idr);
        assert_eq!(inspected.profile_idc, Some(100));
        assert!(inspected.sps_before_idr && inspected.pps_before_idr);
        assert!(!inspected.has_b_slice);
        assert!(
            inspect_access_unit(&[0, 0, 1, 0x41, 0xa0])
                .unwrap()
                .has_b_slice
        ); // ue(0), ue(1)
        assert_eq!(
            inspect_access_unit(&[0, 0, 1, 0x41, 0xc0, 0, 0, 1, 0x41, 0xc0])
                .unwrap()
                .access_units,
            2
        );
    }
    #[test]
    fn missing_headers_are_prepended_and_existing_headers_are_preserved() {
        let idr = vec![0, 0, 1, 0x65, 0xb0];
        let data = prepend_sequence_if_needed(idr.clone(), &headers()).unwrap();
        let inspected = inspect_access_unit(&data).unwrap();
        assert!(inspected.sps_before_idr && inspected.pps_before_idr);
        assert_eq!(
            prepend_sequence_if_needed(data.clone(), &headers()).unwrap(),
            data
        );
        let mut baseline = headers();
        baseline[5] = 66;
        assert!(prepend_sequence_if_needed(idr, &baseline).is_err());
    }
    #[test]
    fn malformed_output_and_truncated_slice_do_not_pass() {
        assert!(annex_b_nals(&[0, 0, 0, 2, 0x65, 0xb0]).is_err());
        assert!(inspect_access_unit(&[0, 0, 1, 0x65]).is_err());
        assert!(annex_b_nals(&[0, 0, 1]).is_err());
    }
    #[test]
    fn missing_headers_keep_access_unit_delimiter_first() {
        let idr = vec![0, 0, 1, 0x09, 0x10, 0, 0, 1, 0x65, 0xb0];
        let data = prepend_sequence_if_needed(idr, &headers()).unwrap();
        assert_eq!(annex_b_nals(&data).unwrap()[0][0] & 31, 9);
        let inspect = inspect_access_unit(&data).unwrap();
        assert_eq!(inspect.access_units, 1);
        assert!(inspect.sps_before_idr && inspect.pps_before_idr);
    }
    #[test]
    fn gop_and_one_frame_buffer_units() {
        assert_eq!(gop_frames(30, 2, true), u32::MAX);
        assert_eq!(gop_frames(60, 4, false), 240);
        assert_eq!(one_frame_buffer(12_000_000, 30), 50_000);
        assert_eq!(one_frame_buffer(12_000_001, 30), 50_001);
    }
}
