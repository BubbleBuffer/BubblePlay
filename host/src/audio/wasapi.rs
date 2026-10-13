//! WASAPI shared-mode loopback on the default render endpoint. Every COM object
//! is created and used on the capture thread.

use std::ffi::c_void;
use std::ptr;

use anyhow::{bail, ensure, Context, Result};
use tracing::warn;
use windows::core::{IUnknown, GUID, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::convert::{Converter, SourceFormat};
use super::wire::SAMPLE_RATE;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SUBTYPE_PCM: GUID = GUID::from_u128(0x0000_0001_0000_0010_8000_00aa_0038_9b71);
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);
/// Speaker mask for front left and front right, the stereo request.
const STEREO_MASK: u32 = 0x3;
/// 20 ms buffer: the engine may round this up, never down.
const BUFFER_HNS: i64 = 200_000;
/// The extensible format extension is 22 bytes after the 18-byte base.
const EXTENSIBLE_CBSIZE: u16 = 22;

/// First attempt is 48 kHz stereo float with conversion done by the engine.
/// The second attempt takes whatever the device mixes at.
#[derive(Clone, Copy)]
enum Negotiation {
    Stereo,
    Mix,
}

/// Balances CoInitializeEx on the capture thread. A thread that was already in
/// a different apartment keeps its mode and is not uninitialised.
pub struct ComApartment(bool);

impl ComApartment {
    pub fn enter() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            warn!(
                code = hr.0,
                "audio capture thread kept its existing COM apartment"
            );
        }
        Self(hr.is_ok())
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

struct OwnedEvent(HANDLE);

impl OwnedEvent {
    fn create() -> Result<Self> {
        // Auto-reset, unnamed, initially unsignalled.
        let handle =
            unsafe { CreateEventW(None, false, false, None) }.context("create loopback event")?;
        Ok(Self(handle))
    }
}

impl Drop for OwnedEvent {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Releases the buffer WASAPI allocated for the mix format.
struct MixFormatBuffer(*mut WAVEFORMATEX);

impl Drop for MixFormatBuffer {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const c_void)) };
    }
}

/// What one call to [`Device::read`] found.
#[derive(Debug, Default)]
pub struct Read {
    pub got_data: bool,
    /// The engine reported lost data (`AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`).
    pub discontinuity: bool,
}

/// An open loopback stream on one render endpoint.
pub struct Device {
    enumerator: IMMDeviceEnumerator,
    device_id: String,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: OwnedEvent,
    frame_bytes: usize,
    converter: Converter,
    silence: Vec<u8>,
}

impl Device {
    pub fn open() -> Result<Self> {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL) }
                .context("create device enumerator")?;
        let endpoint = default_endpoint(&enumerator)?;
        let device_id = endpoint_id(&endpoint)?;
        let (client, format) = match initialize(&endpoint, Negotiation::Stereo) {
            Ok(opened) => opened,
            Err(err) => {
                let reason = format!("{err:#}");
                tracing::debug!(error = %reason, "loopback refused 48 kHz stereo float; using mix format");
                initialize(&endpoint, Negotiation::Mix)?
            }
        };
        let event = OwnedEvent::create()?;
        unsafe { client.SetEventHandle(event.0) }.context("attach loopback event")?;
        let capture: IAudioCaptureClient =
            unsafe { client.GetService() }.context("get loopback capture service")?;
        unsafe { client.Start() }.context("start loopback")?;
        Ok(Self {
            enumerator,
            device_id,
            client,
            capture,
            event,
            frame_bytes: format.frame_bytes(),
            converter: Converter::new(format),
            silence: Vec::new(),
        })
    }

    pub fn format(&self) -> SourceFormat {
        *self.converter.format()
    }

    /// Blocks until the engine signals data or `timeout_ms` passes.
    pub fn wait(&self, timeout_ms: u32) {
        let _ = unsafe { WaitForSingleObject(self.event.0, timeout_ms) };
    }

    /// True when the default render endpoint is no longer the one this stream opened.
    /// A failed lookup counts as unchanged, so a transient error does not reopen the stream.
    pub fn default_changed(&self) -> bool {
        let Ok(endpoint) = default_endpoint(&self.enumerator) else {
            return false;
        };
        match endpoint_id(&endpoint) {
            Ok(id) => id != self.device_id,
            Err(_) => false,
        }
    }

    /// Appends 48 kHz stereo for every packet waiting in the engine.
    /// Silent packets are converted from zeros, so timing still advances.
    pub fn read(&mut self, out: &mut Vec<[f32; 2]>) -> windows::core::Result<Read> {
        let mut result = Read::default();
        loop {
            let pending = unsafe { self.capture.GetNextPacketSize() }?;
            if pending == 0 {
                break;
            }
            let mut data: *mut u8 = ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?
            };
            if frames == 0 {
                unsafe { self.capture.ReleaseBuffer(0) }?;
                break;
            }
            let byte_len = frames as usize * self.frame_bytes;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                // The engine's contents are undefined for silent packets.
                self.silence.clear();
                self.silence.resize(byte_len, 0);
                self.converter.push_bytes(&self.silence, out);
            } else {
                let bytes = unsafe { std::slice::from_raw_parts(data, byte_len) };
                self.converter.push_bytes(bytes, out);
            }
            unsafe { self.capture.ReleaseBuffer(frames) }?;
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                result.discontinuity = true;
            }
            result.got_data = true;
        }
        Ok(result)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

fn default_endpoint(enumerator: &IMMDeviceEnumerator) -> Result<IMMDevice> {
    unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .context("no default playback device")
}

fn endpoint_id(endpoint: &IMMDevice) -> Result<String> {
    let raw: PWSTR = unsafe { endpoint.GetId() }.context("read playback device id")?;
    let text = unsafe { raw.to_string() };
    unsafe { CoTaskMemFree(Some(raw.0 as *const c_void)) };
    text.context("playback device id is not valid UTF-16")
}

/// Activates a fresh client and initialises it. A rejected Initialize leaves the
/// client unusable, so each negotiation attempt gets its own client.
fn initialize(endpoint: &IMMDevice, which: Negotiation) -> Result<(IAudioClient, SourceFormat)> {
    let client: IAudioClient =
        unsafe { endpoint.Activate(CLSCTX_ALL, None) }.context("activate audio client")?;
    let base_flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    match which {
        Negotiation::Stereo => {
            let request = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
                nChannels: 2,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: SAMPLE_RATE * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            let flags = base_flags
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            unsafe {
                client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    BUFFER_HNS,
                    0,
                    &request,
                    None,
                )
            }
            .context("initialize 48 kHz stereo float loopback")?;
            let format = SourceFormat::new(true, 32, 2, SAMPLE_RATE, STEREO_MASK)?;
            Ok((client, format))
        }
        Negotiation::Mix => {
            let mix = MixFormatBuffer(unsafe { client.GetMixFormat() }.context("read mix format")?);
            let format = unsafe { parse_wave_format(mix.0) }?;
            unsafe {
                client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    base_flags,
                    BUFFER_HNS,
                    0,
                    mix.0,
                    None,
                )
            }
            .context("initialize loopback with mix format")?;
            Ok((client, format))
        }
    }
}

/// Reads the device's WAVE format. The packed layouts are copied out with
/// `read_unaligned`, never referenced.
///
/// # Safety
/// `ptr` must point to a complete WAVEFORMATEX, followed by the extension when
/// its tag is WAVE_FORMAT_EXTENSIBLE.
unsafe fn parse_wave_format(ptr: *const WAVEFORMATEX) -> Result<SourceFormat> {
    let base = ptr::read_unaligned(ptr);
    let tag = base.wFormatTag;
    let (is_float, mask) = match tag {
        WAVE_FORMAT_IEEE_FLOAT => (true, 0),
        WAVE_FORMAT_PCM => (false, 0),
        WAVE_FORMAT_EXTENSIBLE => {
            ensure!(
                base.cbSize >= EXTENSIBLE_CBSIZE,
                "extensible mix format without its extension"
            );
            let ext = ptr::read_unaligned(ptr as *const WAVEFORMATEXTENSIBLE);
            let sub = ext.SubFormat;
            let mask = ext.dwChannelMask;
            if sub == SUBTYPE_IEEE_FLOAT {
                (true, mask)
            } else if sub == SUBTYPE_PCM {
                (false, mask)
            } else {
                bail!("unsupported mix subformat {sub:?}");
            }
        }
        other => bail!("unsupported mix format tag {other:#06x}"),
    };
    // wBitsPerSample is the container width; the samples fill it.
    let format = SourceFormat::new(
        is_float,
        base.wBitsPerSample,
        base.nChannels,
        base.nSamplesPerSec,
        mask,
    )?;
    let block_align = base.nBlockAlign;
    ensure!(
        block_align as usize == format.frame_bytes(),
        "mix format block align {} does not match {} byte frames",
        block_align,
        format.frame_bytes()
    );
    Ok(format)
}
