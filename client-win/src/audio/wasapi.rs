//! Shared event-driven render endpoint. Created and used only on its COM thread.
use super::{
    convert::{Converter, NativeFormat},
    sink::AudioSink,
    wire::SAMPLE_RATE,
};
use anyhow::{Context, Result};
use std::{ffi::c_void, ptr};
use windows::core::{IUnknown, Interface, GUID};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioClient3, IAudioRenderClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};

/// Opt only this worker into MMCSS. Failure leaves normal scheduling and must
/// not prevent playback or change any other thread/process priority.
pub struct AudioScheduling(HANDLE);
impl AudioScheduling {
    pub fn enter() -> Option<Self> {
        let mut index = 0;
        unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Audio"), &mut index) }
            .ok()
            .map(Self)
    }
}
impl Drop for AudioScheduling {
    fn drop(&mut self) {
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

pub struct ComApartment(bool);
impl ComApartment {
    pub fn enter() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .context("initialize audio COM apartment")?;
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

struct Event(HANDLE);
impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub struct WasapiSink {
    enumerator: IMMDeviceEnumerator,
    endpoint_id: String,
    client: IAudioClient,
    render: IAudioRenderClient,
    event: Event,
    frames: u32,
    period_ms: f64,
    converter: Converter,
}

impl WasapiSink {
    pub fn open() -> Result<Self> {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL) }
                .context("create playback device enumerator")?;
        let endpoint = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .context("no default playback endpoint")?;
        let endpoint_id = endpoint_id(&endpoint)?;
        // Prefer IAudioClient3's smallest supported engine period. Some drivers
        // or an already locked engine reject it: a fresh IAudioClient then
        // uses the standard shared period and engine PCM conversion.
        let (client, period_ms, native) = match initialize_low_period(&endpoint, false) {
            Ok(value) => value,
            Err(_) => initialize_low_period(&endpoint, true)
                .or_else(|_| initialize_standard(&endpoint))?,
        };
        let event = Event(
            unsafe { CreateEventW(None, false, false, None) }.context("create playback event")?,
        );
        unsafe { client.SetEventHandle(event.0) }.context("attach playback event")?;
        let render: IAudioRenderClient =
            unsafe { client.GetService() }.context("get audio render service")?;
        let frames = unsafe { client.GetBufferSize() }.context("query playback buffer")?;
        anyhow::ensure!(frames > 0, "empty WASAPI playback buffer");
        // Start with a fully initialized silent buffer; never hand the endpoint
        // uninitialized bytes even before the first render wake.
        let data = unsafe { render.GetBuffer(frames) }.context("prime playback buffer")?;
        anyhow::ensure!(!data.is_null(), "null WASAPI playback buffer");
        unsafe { ptr::write_bytes(data, 0, frames as usize * native.frame_bytes()) };
        unsafe { render.ReleaseBuffer(frames, 0) }.context("release playback prime")?;
        unsafe { client.Start() }.context("start audio playback")?;
        Ok(Self {
            enumerator,
            endpoint_id,
            client,
            render,
            event,
            frames,
            period_ms,
            converter: Converter::new(native),
        })
    }
}

impl AudioSink for WasapiSink {
    fn writable(&mut self, _now_us: u64) -> Result<(usize, f64)> {
        let padding =
            unsafe { self.client.GetCurrentPadding() }.context("query playback padding")?;
        anyhow::ensure!(padding <= self.frames, "invalid WASAPI playback padding");
        Ok((
            self.converter.input_capacity(self.frames - padding),
            padding as f64 * 1000.0 / self.converter.format.sample_rate as f64,
        ))
    }
    fn write(&mut self, pcm: &[f32]) -> Result<()> {
        let bytes = self.converter.convert(pcm);
        let frames = (bytes.len() / self.converter.format.frame_bytes()) as u32;
        if frames == 0 {
            return Ok(());
        }
        let data = unsafe { self.render.GetBuffer(frames) }.context("get playback buffer")?;
        anyhow::ensure!(!data.is_null(), "null WASAPI render buffer");
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len()) };
        unsafe { self.render.ReleaseBuffer(frames, 0) }.context("release playback buffer")?;
        Ok(())
    }
    fn clear(&mut self) -> Result<()> {
        unsafe { self.client.Stop() }.context("stop audio before flush")?;
        unsafe { self.client.Reset() }.context("flush playback buffer")?;
        self.converter.reset();
        unsafe { self.client.Start() }.context("restart flushed audio")?;
        Ok(())
    }
    fn wait(&self, timeout_ms: u32) {
        let _ = unsafe { WaitForSingleObject(self.event.0, timeout_ms) };
    }
    fn default_changed(&self) -> bool {
        let current = unsafe { self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) };
        current
            .ok()
            .and_then(|endpoint| endpoint_id(&endpoint).ok())
            .is_none_or(|id| id != self.endpoint_id)
    }
    fn buffer_ms(&self) -> f64 {
        self.frames as f64 * 1000.0 / self.converter.format.sample_rate as f64
    }
    fn period_ms(&self) -> f64 {
        self.period_ms
    }
    fn observes_device_padding(&self) -> bool {
        true
    }
}
impl Drop for WasapiSink {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

fn format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 3,
        nChannels: 2,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}
fn flags() -> u32 {
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
}
struct MixFormat(*mut WAVEFORMATEX);
impl Drop for MixFormat {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const c_void)) };
    }
}

fn initialize_low_period(
    endpoint: &IMMDevice,
    use_mix: bool,
) -> Result<(IAudioClient, f64, NativeFormat)> {
    let client: IAudioClient = unsafe { endpoint.Activate(CLSCTX_ALL, None) }
        .context("activate low period render client")?;
    let client3: IAudioClient3 = client.cast().context("IAudioClient3 unavailable")?;
    let request = format();
    let mix = if use_mix {
        Some(MixFormat(
            unsafe { client.GetMixFormat() }.context("query endpoint mix format")?,
        ))
    } else {
        None
    };
    let raw: *const WAVEFORMATEX = mix
        .as_ref()
        .map_or(&request as *const _, |mix| mix.0 as *const _);
    let native = if use_mix {
        unsafe { parse_native_format(raw) }?
    } else {
        NativeFormat::stereo()
    };
    let (mut default, mut fundamental, mut minimum, mut maximum) = (0u32, 0u32, 0u32, 0u32);
    unsafe {
        client3.GetSharedModeEnginePeriod(
            raw,
            &mut default,
            &mut fundamental,
            &mut minimum,
            &mut maximum,
        )
    }
    .context("query shared engine periods")?;
    anyhow::ensure!(
        minimum > 0 && maximum >= minimum,
        "invalid shared engine periods"
    );
    let fundamental = fundamental.max(1);
    let period = minimum.div_ceil(fundamental) * fundamental;
    anyhow::ensure!(period <= maximum, "shared engine minimum not aligned");
    // Microsoft documents EVENTCALLBACK as this API's only stream flag;
    // AUTOCONVERTPCM belongs to IAudioClient::Initialize. The mix-format
    // branch performs channel/sample-rate conversion in our own adapter.
    unsafe {
        client3.InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, period, raw, None)
    }
    .context("initialize minimum period shared playback")?;
    Ok((
        client,
        period as f64 * 1000.0 / native.sample_rate as f64,
        native,
    ))
}
fn initialize_standard(endpoint: &IMMDevice) -> Result<(IAudioClient, f64, NativeFormat)> {
    let client: IAudioClient =
        unsafe { endpoint.Activate(CLSCTX_ALL, None) }.context("activate shared render client")?;
    let mut period = 0i64;
    unsafe { client.GetDevicePeriod(Some(&mut period), None) }
        .context("query default audio period")?;
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags(), 0, 0, &format(), None) }
        .context("initialize converted 48 kHz stereo playback")?;
    Ok((client, period as f64 / 10_000.0, NativeFormat::stereo()))
}

/// Windows owns the complete WAVEFORMATEX/EXTENSIBLE allocation.
unsafe fn parse_native_format(raw: *const WAVEFORMATEX) -> Result<NativeFormat> {
    anyhow::ensure!(!raw.is_null(), "null endpoint mix format");
    let base = ptr::read_unaligned(raw);
    if base.wFormatTag != 0xfffe {
        anyhow::ensure!(
            base.nChannels <= 2,
            "multichannel base format has no speaker layout"
        );
    }
    let float = match base.wFormatTag {
        3 => true,
        1 => false,
        0xfffe => {
            anyhow::ensure!(base.cbSize >= 22, "truncated endpoint mix extension");
            let ext = ptr::read_unaligned(raw.cast::<WAVEFORMATEXTENSIBLE>());
            let valid_bits = ext.Samples.wValidBitsPerSample;
            anyhow::ensure!(
                valid_bits == base.wBitsPerSample,
                "endpoint valid bits differ from sample container; use engine conversion"
            );
            let mask = ext.dwChannelMask;
            if base.nChannels == 1 {
                anyhow::ensure!(
                    mask == 0 || mask == 4,
                    "unsupported mono endpoint speaker layout"
                );
            } else {
                anyhow::ensure!(
                    mask & 3 == 3
                        && mask.count_ones() == base.nChannels as u32
                        && mask & !0x3ffff == 0,
                    "endpoint mix layout must begin with front left/right"
                );
            }
            let sub = ext.SubFormat;
            if sub == GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71) {
                true
            } else if sub == GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71) {
                false
            } else {
                anyhow::bail!("unsupported endpoint mix subformat");
            }
        }
        _ => anyhow::bail!("unsupported endpoint mix format"),
    };
    let native = NativeFormat {
        sample_rate: base.nSamplesPerSec,
        channels: base.nChannels,
        bits: base.wBitsPerSample,
        float,
    }
    .validate()?;
    anyhow::ensure!(
        native.frame_bytes() == base.nBlockAlign as usize,
        "invalid endpoint mix block alignment"
    );
    Ok(native)
}
fn endpoint_id(endpoint: &IMMDevice) -> Result<String> {
    let raw = unsafe { endpoint.GetId() }.context("query playback endpoint id")?;
    let result = unsafe { raw.to_string() };
    unsafe { CoTaskMemFree(Some(raw.0 as *const c_void)) };
    result.context("decode playback endpoint id")
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE_0;

    fn extended(
        channels: u16,
        mask: u32,
        bits: u16,
        valid: u16,
        float: bool,
    ) -> WAVEFORMATEXTENSIBLE {
        WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: 0xfffe,
                nChannels: channels,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: SAMPLE_RATE * channels as u32 * bits as u32 / 8,
                nBlockAlign: channels * bits / 8,
                wBitsPerSample: bits,
                cbSize: 22,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: valid,
            },
            dwChannelMask: mask,
            SubFormat: GUID::from_u128(if float {
                0x00000003_0000_0010_8000_00aa00389b71
            } else {
                0x00000001_0000_0010_8000_00aa00389b71
            }),
        }
    }
    fn parse(ext: &WAVEFORMATEXTENSIBLE) -> Result<NativeFormat> {
        unsafe { parse_native_format((ext as *const WAVEFORMATEXTENSIBLE).cast()) }
    }
    #[test]
    fn endpoint_mix_parser_accepts_supported_stereo_mono_and_surround_layouts() {
        for (channels, mask) in [(1, 0), (1, 4), (2, 3), (6, 0x3f), (8, 0x63f)] {
            let result = parse(&extended(channels, mask, 32, 32, true)).unwrap();
            assert_eq!(result.channels, channels);
        }
        assert!(parse(&extended(1, 4, 16, 16, false)).is_ok());
        assert!(unsafe { parse_native_format(&format()) }.is_ok());
    }
    #[test]
    fn endpoint_mix_parser_rejects_ambiguous_layouts_and_padded_valid_bits() {
        for (channels, mask) in [
            (1, 8),
            (2, 0x0c),
            (2, 0),
            (2, 0x3f),
            (6, 3),
            (3, 0x80000003),
        ] {
            assert!(parse(&extended(channels, mask, 32, 32, true)).is_err());
        }
        for valid in [0, 16, 24, 33] {
            assert!(parse(&extended(2, 3, 32, valid, false)).is_err());
        }
        let mut truncated = extended(2, 3, 32, 32, true);
        truncated.Format.cbSize = 20;
        assert!(parse(&truncated).is_err());
    }
}
