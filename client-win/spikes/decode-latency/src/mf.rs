//! Direct MFT decode. The caller owns COM/MF startup and a video-capable D3D11
//! device with multithread protection enabled. No CPU-output fallback is allowed.
//!
//! MFT_ENUM_FLAG_HARDWARE only finds async hardware-registered transforms. The
//! inbox/extension H.264/HEVC decoders can also use a software-registered hybrid
//! MFT which parses the stream and uses the driver's D3D11 decoder through the
//! manager. Registration category alone therefore does not prove CPU decoding.
//! See https://learn.microsoft.com/windows/win32/medfound/supporting-direct3d-11-video-decoding-in-media-foundation
//! and https://learn.microsoft.com/windows/win32/api/mfapi/nf-mfapi-mftenumex

use crate::backend::{Codec, DecodeBackend, DecodedFrame};
use anyhow::{anyhow, bail, Context, Result};
use std::{mem::ManuallyDrop, ptr};
use windows::{
    core::{IUnknown, Interface, GUID, PWSTR, VARIANT},
    Win32::{
        Foundation::{E_NOINTERFACE, E_NOTIMPL},
        Graphics::{
            Direct3D11::{
                ID3D11Device, ID3D11Texture2D, ID3D11VideoDevice, D3D11_BIND_DECODER,
                D3D11_DECODER_PROFILE_H264_VLD_FGT, D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
                D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, D3D11_TEXTURE2D_DESC,
            },
            Dxgi::Common::DXGI_FORMAT_NV12,
        },
        Media::MediaFoundation::*,
        System::Com::CoTaskMemFree,
    },
};

struct Candidate {
    activation: IMFActivate,
    identity: String,
}

fn subtype(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
    }
}

unsafe fn activation_name(activation: &IMFActivate) -> String {
    let mut name = PWSTR::null();
    let mut length = 0;
    let friendly =
        match activation.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut length) {
            Ok(()) => {
                let value =
                    String::from_utf16_lossy(std::slice::from_raw_parts(name.0, length as usize));
                CoTaskMemFree(Some(name.0.cast()));
                value
            }
            Err(_) => "unnamed MFT".to_owned(),
        };
    let clsid = activation
        .GetGUID(&MFT_TRANSFORM_CLSID_Attribute)
        .map(|v| format!("{v:?}"))
        .unwrap_or_else(|_| "unknown CLSID".into());
    format!("{friendly}; CLSID={clsid}")
}

unsafe fn enumerate(codec: Codec) -> Result<Vec<Candidate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype(codec),
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let mut candidates = Vec::new();
    for (category, flags) in [
        ("hardware-registered", MFT_ENUM_FLAG_HARDWARE),
        ("synchronous/hybrid candidate", MFT_ENUM_FLAG_SYNCMFT),
        (
            "asynchronous/software-registered candidate",
            MFT_ENUM_FLAG_ASYNCMFT,
        ),
    ] {
        let mut array: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            flags | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut array,
            &mut count,
        )
        .with_context(|| format!("MFTEnumEx {codec:?} {category}"))?;
        if count != 0 {
            if array.is_null() {
                bail!("MFTEnumEx returned a null activation array with count={count}");
            }
            for index in 0..count as usize {
                // Move each reference out of the COM-allocated array; freeing the
                // array alone does not release these interface references.
                if let Some(activation) = ptr::read(array.add(index)) {
                    let identity = format!("{category}: {}", activation_name(&activation));
                    candidates.push(Candidate {
                        activation,
                        identity,
                    });
                }
            }
        }
        CoTaskMemFree(Some(array.cast()));
    }
    Ok(candidates)
}

/// Installed candidates, not a promise of acceleration for a particular stream.
/// MfBackend::new validates device-manager negotiation; poll validates each surface.
pub fn availability_report(codec: Codec) -> Result<Vec<String>> {
    unsafe {
        let candidates = enumerate(codec)?;
        let mut report = Vec::new();
        for candidate in candidates {
            let detail = match candidate.activation.ActivateObject::<IMFTransform>() {
                Ok(transform) => match transform.GetAttributes() {
                    Ok(attrs) => format!(
                        "D3D11-aware={}; async={}; ICodecAPI={}; MF_LOW_LATENCY_initial={:?}",
                        attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0),
                        attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0),
                        match transform.cast::<ICodecAPI>() {
                            Ok(_) => "supported".to_owned(),
                            Err(e) => format!("unavailable ({:?})", e.code()),
                        },
                        attrs.GetUINT32(&MF_LOW_LATENCY),
                    ),
                    Err(e) => format!("GetAttributes failed: {e}"),
                },
                Err(e) => format!("activation failed: {e}"),
            };
            let _ = candidate.activation.ShutdownObject();
            report.push(format!("{}; {detail}", candidate.identity));
        }
        if report.is_empty() {
            report.push(format!("{codec:?}: no installed NV12 decoder candidates"));
        }
        Ok(report)
    }
}

/// Startup capability check: installed D3D11-aware MFT plus a matching driver
/// NV12 decode profile. A specific stream is still validated by actual output.
pub fn hardware_decoder_available(codec: Codec, device: &ID3D11Device) -> Result<bool> {
    unsafe {
        let video: ID3D11VideoDevice = device.cast()?;
        let profiles: &[GUID] = match codec {
            Codec::H264 => &[
                D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
                D3D11_DECODER_PROFILE_H264_VLD_FGT,
            ],
            Codec::Hevc => &[D3D11_DECODER_PROFILE_HEVC_VLD_MAIN],
        };
        let mut driver_supported = false;
        for i in 0..video.GetVideoDecoderProfileCount() {
            let profile = video.GetVideoDecoderProfile(i)?;
            if profiles.contains(&profile)
                && video
                    .CheckVideoDecoderFormat(&profile, DXGI_FORMAT_NV12)?
                    .as_bool()
            {
                driver_supported = true;
                break;
            }
        }
        if !driver_supported {
            return Ok(false);
        }
        for candidate in enumerate(codec)? {
            let aware = candidate
                .activation
                .ActivateObject::<IMFTransform>()
                .ok()
                .and_then(|t| t.GetAttributes().ok())
                .and_then(|a| a.GetUINT32(&MF_SA_D3D11_AWARE).ok())
                .unwrap_or(0)
                != 0;
            let _ = candidate.activation.ShutdownObject();
            if aware {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

pub struct MfBackend {
    transform: IMFTransform,
    activation: IMFActivate,
    _manager: IMFDXGIDeviceManager,
    device: ID3D11Device,
    events: Option<IMFMediaEventGenerator>,
    input_id: u32,
    output_id: u32,
    input_credits: u32,
    output_credits: u32,
    draining: bool,
    drain_complete: bool,
    description: String,
}

impl MfBackend {
    pub fn new(codec: Codec, device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let mut errors = Vec::new();
            for candidate in enumerate(codec)? {
                match Self::try_candidate(&candidate, codec, device, width, height) {
                    Ok(backend) => return Ok(backend),
                    Err(error) => {
                        let _ = candidate.activation.ShutdownObject();
                        errors.push(format!("{}: {error:#}", candidate.identity));
                    }
                }
            }
            bail!("No usable D3D11/NV12 {codec:?} MFT. {}", errors.join("\n"))
        }
    }

    unsafe fn try_candidate(
        candidate: &Candidate,
        codec: Codec,
        device: &ID3D11Device,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let transform: IMFTransform = candidate.activation.ActivateObject()?;
        let attrs = transform.GetAttributes()?;
        if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
            bail!("MFT is not D3D11-aware; software fallback refused");
        }
        let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
        let events = if is_async {
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            Some(transform.cast::<IMFMediaEventGenerator>()?)
        } else {
            None
        };

        let low_latency_configuration = configure_low_latency(&transform, &attrs, codec)?;

        let mut reset_token = 0;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)?;
        let manager = manager.context("null DXGI device manager")?;
        manager.ResetDevice(device, reset_token)?;
        transform
            .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
            .context("MFT_MESSAGE_SET_D3D_MANAGER")?;

        let (mut input_count, mut output_count) = (0, 0);
        transform.GetStreamCount(&mut input_count, &mut output_count)?;
        if input_count != 1 || output_count != 1 {
            bail!("Expected one input/output stream; got {input_count}/{output_count}");
        }
        let mut input_ids = [0];
        let mut output_ids = [0];
        match transform.GetStreamIDs(&mut input_ids, &mut output_ids) {
            Ok(()) => {}
            Err(e) if e.code() == E_NOTIMPL => {}
            Err(e) => return Err(e.into()),
        }
        let input_id = input_ids[0];
        let output_id = output_ids[0];
        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &subtype(codec))?;
        input.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        input.SetUINT64(&MF_MT_FRAME_RATE, (60u64 << 32) | 1)?;
        input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        transform
            .SetInputType(input_id, &input, 0)
            .context("SetInputType Annex-B AU")?;
        let mut backend = Self {
            transform, activation: candidate.activation.clone(), _manager: manager,
            device: device.clone(), events, input_id, output_id,
            input_credits: 0, output_credits: 0, draining: false, drain_complete: false,
            description: format!("MF {codec:?}; {}; async={is_async}; {low_latency_configuration}; D3D11 manager attached; NV12 output required", candidate.identity),
        };
        backend.negotiate_output()?;
        backend
            .transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        backend
            .transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        backend.pump_events()?;
        Ok(backend)
    }

    unsafe fn negotiate_output(&mut self) -> Result<()> {
        for index in 0..64 {
            let output = match self.transform.GetOutputAvailableType(self.output_id, index) {
                Ok(v) => v,
                Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
                Err(e) => return Err(e).context("GetOutputAvailableType"),
            };
            if output.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_NV12 {
                continue;
            }
            self.transform
                .SetOutputType(self.output_id, &output, 0)
                .context("SetOutputType NV12")?;
            let info = self.transform.GetOutputStreamInfo(self.output_id)?;
            let sample_flags = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
            if info.dwFlags & sample_flags == 0 {
                bail!("MFT requires caller-provided output buffers; refusing CPU buffer fallback");
            }
            return Ok(());
        }
        bail!("MFT offers no NV12 output media type")
    }

    unsafe fn pump_events(&mut self) -> Result<()> {
        let Some(events) = &self.events else {
            return Ok(());
        };
        // Bound one poll call even if a misbehaving MFT produces events forever.
        for _ in 0..128 {
            let event = match events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(v) => v,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e).context("MFT GetEvent(NO_WAIT)"),
            };
            event
                .GetStatus()?
                .ok()
                .context("asynchronous MFT event status")?;
            match event.GetType()? as i32 {
                v if v == METransformNeedInput.0 => {
                    let stream = event
                        .GetUINT32(&MF_EVENT_MFT_INPUT_STREAM_ID)
                        .context("NeedInput missing required stream-id attribute")?;
                    if stream != self.input_id {
                        bail!("NeedInput event for unexpected stream {stream}");
                    }
                    if !self.draining {
                        self.input_credits = self.input_credits.saturating_add(1);
                    }
                }
                v if v == METransformHaveOutput.0 => {
                    self.output_credits = self.output_credits.saturating_add(1)
                }
                v if v == METransformDrainComplete.0 => self.drain_complete = true,
                v if v == MEError.0 => bail!("MFT emitted MEError"),
                _ => {}
            }
        }
        Ok(())
    }

    /// Used by the coordinator to distinguish a quiet async decoder from EOF.
    pub fn is_drained(&self) -> bool {
        self.draining && self.drain_complete && self.output_credits == 0
    }
}

/// Configure the decoder's documented low-latency property. The packaged HEVC
/// activation on this PC exposes IMFTransform/IMFAttributes but no ICodecAPI.
/// Microsoft documents IMFAttributes as a direct MFT configuration route and
/// explicitly states MF_LOW_LATENCY is the same GUID as CODECAPI_AVLowLatencyMode:
/// https://learn.microsoft.com/windows/win32/medfound/mf-low-latency
/// The HEVC decoder lists this property under its transform attributes, with
/// ICodecAPI described as an alternative interface:
/// https://learn.microsoft.com/windows/win32/medfound/h-265---hevc-video-decoder
/// Setter/readback evidence establishes configuration, not its latency effect.
unsafe fn configure_low_latency(
    transform: &IMFTransform,
    attrs: &IMFAttributes,
    codec: Codec,
) -> Result<String> {
    let api: ICodecAPI = match transform.cast() {
        Ok(api) => api,
        Err(e) if matches!(codec, Codec::Hevc) && e.code() == E_NOINTERFACE => {
            let before = attrs.GetUINT32(&MF_LOW_LATENCY);
            attrs.SetUINT32(&MF_LOW_LATENCY, 1).context(
                "HEVC ICodecAPI unavailable; documented MF_LOW_LATENCY SetUINT32 failed",
            )?;
            // Read through a fresh GetAttributes call, rather than only the
            // retained attribute-store reference used for the setter.
            let readback = transform
                .GetAttributes()?
                .GetUINT32(&MF_LOW_LATENCY)
                .context("HEVC MF_LOW_LATENCY readback")?;
            if readback != 1 {
                bail!("HEVC MF_LOW_LATENCY setter accepted but readback={readback}");
            }
            return Ok(format!("ICodecAPI=unavailable ({:?}); low_latency=MF_LOW_LATENCY/IMFAttributes UINT32 (same GUID as CODECAPI_AVLowLatencyMode); initial={before:?}; readback={readback}; configuration accepted, behavior effect unverified", e.code()));
        }
        Err(e) => return Err(e).context("ICodecAPI required for low latency"),
    };
    // Microsoft's H.264 decoder is the documented exception using VT_UI4.
    // Other implementations use VT_BOOL; vendor H264 can reject UI4.
    // https://learn.microsoft.com/windows/win32/medfound/codecapi-avlowlatencymode
    let low_latency_type = match codec {
        Codec::H264 => {
            if api
                .SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(1u32))
                .is_ok()
            {
                "VT_UI4"
            } else {
                api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true))
                    .context("CODECAPI_AVLowLatencyMode UI4 and BOOL rejected")?;
                "VT_BOOL"
            }
        }
        Codec::Hevc => {
            api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true))
                .context("CODECAPI_AVLowLatencyMode VT_BOOL")?;
            "VT_BOOL"
        }
    };
    let low_latency_readback = match api.GetValue(&CODECAPI_AVLowLatencyMode) {
        Ok(value) => {
            if !bool::try_from(&value).context("low latency readback is not boolean-compatible")? {
                bail!("MFT accepted low latency setter but reports disabled");
            }
            format!("{value:?}")
        }
        Err(e) => format!("unavailable ({e}); setter accepted"),
    };

    Ok(format!(
        "ICodecAPI=supported; low_latency={low_latency_type}; readback={low_latency_readback}"
    ))
}

impl DecodeBackend for MfBackend {
    fn submit(&mut self, access_unit: &[u8], pts_100ns: i64) -> Result<bool> {
        unsafe {
            if self.draining {
                bail!("Cannot submit after drain started");
            }
            self.pump_events()?;
            if self.events.is_some() && self.input_credits == 0 {
                return Ok(false);
            }
            if access_unit.is_empty() {
                bail!("Empty access unit");
            }
            let size: u32 = access_unit.len().try_into().context("AU larger than u32")?;
            let buffer = MFCreateMemoryBuffer(size)?;
            let mut data = ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            ptr::copy_nonoverlapping(access_unit.as_ptr(), data, access_unit.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(size)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts_100ns)?;
            sample.SetSampleDuration(10_000_000 / 60)?;
            match self.transform.ProcessInput(self.input_id, &sample, 0) {
                Ok(()) => {
                    if self.events.is_some() {
                        self.input_credits -= 1;
                    }
                    Ok(true)
                }
                Err(e) if e.code() == MF_E_NOTACCEPTING => Ok(false),
                Err(e) => Err(e).context("MFT ProcessInput"),
            }
        }
    }

    fn poll(&mut self) -> Result<Option<DecodedFrame>> {
        unsafe {
            self.pump_events()?;
            if self.events.is_some() && self.output_credits == 0 {
                return Ok(None);
            }
            for _ in 0..8 {
                let mut output = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: self.output_id,
                    ..Default::default()
                }];
                let mut status = 0;
                let result = self.transform.ProcessOutput(0, &mut output, &mut status);
                // Both fields transfer COM references even on some error paths.
                let sample = ManuallyDrop::take(&mut output[0].pSample);
                drop(ManuallyDrop::take(&mut output[0].pEvents));
                match result {
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.negotiate_output()?;
                        // For async MFT the same HaveOutput credit remains valid
                        // across format negotiation until output is retrieved.
                        continue;
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                        if self.events.is_some() {
                            bail!("Async MFT unexpectedly returned NEED_MORE_INPUT");
                        }
                        if self.draining {
                            self.drain_complete = true;
                        }
                        return Ok(None);
                    }
                    Err(e) => return Err(e).context("MFT ProcessOutput"),
                    Ok(()) => {}
                }
                if self.events.is_some() {
                    self.output_credits -= 1;
                }
                let Some(sample) = sample else {
                    return Ok(None);
                };
                let pts_100ns = sample.GetSampleTime().context("output timestamp")?;
                if sample.GetBufferCount()? != 1 {
                    bail!("Expected one DXGI output buffer");
                }
                let buffer = sample.GetBufferByIndex(0)?;
                let dxgi: IMFDXGIBuffer = buffer
                    .cast()
                    .context("MFT returned CPU output; fallback refused")?;
                let mut raw = ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
                if raw.is_null() {
                    bail!("DXGI output buffer returned a null texture");
                }
                let texture = ID3D11Texture2D::from_raw(raw);
                let surface_device: IUnknown = texture.GetDevice()?.cast()?;
                let expected_device: IUnknown = self.device.cast()?;
                if surface_device != expected_device {
                    bail!("MFT output texture belongs to a different D3D11 device");
                }
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                if desc.Format != DXGI_FORMAT_NV12 {
                    bail!("MFT texture format is {:?}, expected NV12", desc.Format);
                }
                if desc.BindFlags & D3D11_BIND_DECODER.0 as u32 == 0 {
                    bail!(
                        "MFT texture lacks D3D11_BIND_DECODER; hardware surface evidence missing"
                    );
                }
                let subresource = dxgi.GetSubresourceIndex()?;
                if desc.MipLevels == 0 || subresource % desc.MipLevels != 0 {
                    bail!("Unexpected output mip subresource {subresource}");
                }
                let array_slice = subresource / desc.MipLevels;
                if array_slice >= desc.ArraySize {
                    bail!("Output array slice out of bounds");
                }
                return Ok(Some(DecodedFrame {
                    pts_100ns,
                    texture,
                    array_slice,
                    _lease: Box::new(sample),
                }));
            }
            Err(anyhow!("MFT repeated stream changes without output"))
        }
    }

    fn drain(&mut self) -> Result<()> {
        if self.draining {
            return Ok(());
        }
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, self.input_id as usize)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        self.draining = true;
        self.input_credits = 0;
        Ok(())
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn is_drained(&self) -> bool {
        MfBackend::is_drained(self)
    }
}

impl Drop for MfBackend {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            let _ = self.activation.ShutdownObject();
        }
    }
}
