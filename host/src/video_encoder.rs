//! Static dispatch at the local host's video encode boundary.
//!
//! NVENC keeps its native configuration and fast path. Media Foundation
//! supplies H.264 when NVENC is unavailable or CPU mode is selected.

use std::ffi::c_void;
use std::str::FromStr;

use anyhow::{bail, ensure, Context, Result};
use tracing::info;
use windows::core::Interface;
use windows::core::{s, w};
use windows::Win32::Foundation::FreeLibrary;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::encode::{EncodedFrame, FrameDamage, Nvenc, VideoCodec};
use crate::mf_encode::MfEncoder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderCapabilities {
    pub h264: bool,
    pub hevc: bool,
    pub main10: bool,
    pub roi_qp_map: bool,
    pub refine_feedback: bool,
    pub hevc_tiles: bool,
}

impl EncoderCapabilities {
    pub const NVENC: Self = Self {
        h264: true,
        hevc: true,
        main10: true,
        roi_qp_map: true,
        refine_feedback: true,
        hevc_tiles: true,
    };

    pub const MF: Self = Self {
        h264: true,
        hevc: false,
        main10: false,
        roi_qp_map: false,
        refine_feedback: false,
        hevc_tiles: false,
    };

    pub const fn supports(self, codec: VideoCodec) -> bool {
        match codec {
            VideoCodec::Hevc => self.hevc,
            VideoCodec::H264 => self.h264,
        }
    }

    /// The wire header must describe the codec actually emitted by the encoder.
    pub fn effective_codec(self, requested: VideoCodec) -> VideoCodec {
        if self.supports(requested) {
            requested
        } else {
            VideoCodec::H264
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum EncoderOverride {
    #[default]
    Auto,
    Nvenc,
    MfHardware,
    MfSoftware,
}

impl FromStr for EncoderOverride {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "nvenc" => Ok(Self::Nvenc),
            "mf-hw" => Ok(Self::MfHardware),
            "mf-sw" => Ok(Self::MfSoftware),
            _ => bail!("--encoder must be auto|nvenc|mf-hw|mf-sw"),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EncoderPreference {
    pub cpu_mode: bool,
    pub encoder: EncoderOverride,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    Nvenc,
    MfHardware,
    MfSoftware,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderAdapter {
    pub luid: u64,
    pub vendor_id: u32,
}

pub fn encoder_adapter(adapter: &IDXGIAdapter) -> Result<EncoderAdapter> {
    let desc = unsafe { adapter.GetDesc() }?;
    Ok(EncoderAdapter {
        luid: (u64::from(desc.AdapterLuid.HighPart as u32) << 32)
            | u64::from(desc.AdapterLuid.LowPart),
        vendor_id: desc.VendorId,
    })
}

pub fn device_encoder_adapter(device: *mut c_void) -> Result<EncoderAdapter> {
    let device =
        unsafe { ID3D11Device::from_raw_borrowed(&device) }.context("null capture D3D11 device")?;
    let dxgi: IDXGIDevice = device.cast()?;
    encoder_adapter(&unsafe { dxgi.GetAdapter() }?)
}

/// Discovery belongs to an adapter, not to a frame or encoder session. An
/// unchanged adapter reuses the result across parked capture and recovery.
#[derive(Default)]
pub struct EncoderDiscoveryCache {
    entries: Vec<(EncoderAdapter, EncoderBackend, String)>,
}

impl EncoderDiscoveryCache {
    pub fn resolve(
        &mut self,
        adapter: EncoderAdapter,
        preference: EncoderPreference,
    ) -> Result<EncoderBackend> {
        self.resolve_with(adapter, || discover_encoder_backend(adapter, preference))
    }

    fn resolve_with(
        &mut self,
        adapter: EncoderAdapter,
        probe: impl FnOnce() -> Result<(EncoderBackend, String)>,
    ) -> Result<EncoderBackend> {
        if let Some((_, backend, _)) = self
            .entries
            .iter()
            .find(|(key, _, _)| key.luid == adapter.luid)
        {
            return Ok(*backend);
        }
        let (backend, reason) = probe()?;
        self.entries.push((adapter, backend, reason));
        Ok(backend)
    }

    pub fn reason(&self, adapter: EncoderAdapter) -> Option<&str> {
        self.entries
            .iter()
            .find(|(key, _, _)| key.luid == adapter.luid)
            .map(|(_, _, reason)| reason.as_str())
    }

    pub fn record_actual(
        &mut self,
        adapter: EncoderAdapter,
        backend: EncoderBackend,
        reason: &str,
    ) {
        if let Some((_, selected, stored_reason)) = self
            .entries
            .iter_mut()
            .find(|(key, _, _)| key.luid == adapter.luid)
        {
            *selected = backend;
            if stored_reason.as_str() != reason {
                reason.clone_into(stored_reason);
            }
        }
    }
}

fn discover_encoder_backend(
    adapter: EncoderAdapter,
    preference: EncoderPreference,
) -> Result<(EncoderBackend, String)> {
    let order = selection_order(preference);
    let mut failures = Vec::new();
    if order.contains(&EncoderBackend::Nvenc) {
        // This proves driver API availability, not that every NVIDIA SKU can
        // initialize every requested encode profile. The factory still checks.
        if adapter.vendor_id == 0x10de {
            match probe_nvenc_api() {
                Ok(()) => return Ok((EncoderBackend::Nvenc, String::new())),
                Err(error) => failures.push(format!("NVENC unavailable: {error:#}")),
            }
        } else {
            failures.push("NVENC unavailable: capture adapter is not NVIDIA".to_owned());
        }
    }
    if order
        .iter()
        .any(|backend| *backend != EncoderBackend::Nvenc)
    {
        match crate::mf_encode::probe_availability(Some(adapter.luid)) {
            Ok(available) => {
                if let Some(error) = &available.hardware_error {
                    info!(%error, "Media Foundation hardware discovery unavailable; checking software registration");
                }
                if order.contains(&EncoderBackend::MfHardware) && !available.hardware {
                    failures.push(format!(
                        "Media Foundation hardware H.264 unavailable: {}",
                        available
                            .hardware_error
                            .as_deref()
                            .unwrap_or("no encoder registered for the capture adapter")
                    ));
                }
                for backend in order {
                    if (*backend == EncoderBackend::MfHardware && available.hardware)
                        || (*backend == EncoderBackend::MfSoftware && available.software)
                    {
                        return Ok((*backend, failures.join("; ")));
                    }
                }
                failures.push("no requested Media Foundation H.264 encoder registered".to_owned());
            }
            Err(error) => failures.push(format!("Media Foundation unavailable: {error:#}")),
        }
    }
    bail!("no video encoder: {}", failures.join("; "))
}

/// CPU mode always forces software, including when a diagnostic override is set.
pub fn selection_order(preference: EncoderPreference) -> &'static [EncoderBackend] {
    use EncoderBackend::*;
    if preference.cpu_mode {
        return &[MfSoftware];
    }
    match preference.encoder {
        EncoderOverride::Auto => &[Nvenc, MfHardware, MfSoftware],
        EncoderOverride::Nvenc => &[Nvenc],
        EncoderOverride::MfHardware => &[MfHardware],
        EncoderOverride::MfSoftware => &[MfSoftware],
    }
}

/// Pure policy, independent of driver discovery and native initialization.
pub fn select_backend(
    available: &[EncoderBackend],
    preference: EncoderPreference,
) -> Option<EncoderBackend> {
    selection_order(preference)
        .iter()
        .copied()
        .find(|candidate| available.contains(candidate))
}

// Keep NVENC inline: its existing encode path must not gain an allocation or
// indirect dispatch merely to accommodate the smaller MF backend.
#[allow(clippy::large_enum_variant)]
pub enum VideoEncoder {
    Nvenc(Nvenc),
    Mf(MfEncoder),
}

fn encoder_selection_reason(
    preference: EncoderPreference,
    backend: EncoderBackend,
    failures: &str,
) -> String {
    if preference.cpu_mode {
        " (CPU mode forced by tray setting)".to_owned()
    } else if preference.encoder != EncoderOverride::Auto {
        " (diagnostic override)".to_owned()
    } else if backend == EncoderBackend::MfSoftware {
        if failures.is_empty() {
            "; no hardware encoder".to_owned()
        } else {
            format!("; no hardware encoder: {failures}")
        }
    } else if failures.is_empty() {
        String::new()
    } else {
        format!("; {failures}")
    }
}

fn append_selection_failures(reason: &mut String, unavailable: &[String]) {
    if !unavailable.is_empty() {
        if !reason.is_empty() {
            reason.push_str("; ");
        }
        reason.push_str(&unavailable.join("; "));
    }
}

impl VideoEncoder {
    /// The caller owns NVENC's existing preset and benchmark fallback policy.
    #[allow(clippy::too_many_arguments)]
    pub fn select(
        device: *mut c_void,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        infinite_gop: bool,
        gop_seconds: u32,
        preference: EncoderPreference,
        discovered: Option<EncoderBackend>,
        discovery_reason: &mut String,
        mut create_nvenc: impl FnMut() -> Result<Nvenc>,
    ) -> Result<Self> {
        let mut unavailable = Vec::new();
        for backend in selection_order(preference)
            .iter()
            .skip_while(|backend| discovered.is_some_and(|selected| selected != **backend))
        {
            let result = match backend {
                EncoderBackend::Nvenc => {
                    let available = if discovered.is_none() {
                        probe_nvenc_api()
                    } else {
                        Ok(())
                    };
                    available.and_then(|()| create_nvenc()).map(Self::Nvenc)
                }
                EncoderBackend::MfHardware | EncoderBackend::MfSoftware => MfEncoder::new_with_gop(
                    device,
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    infinite_gop,
                    *backend == EncoderBackend::MfHardware,
                    u8::try_from(gop_seconds).context("MF GOP seconds exceed u8")?,
                )
                .map(Self::Mf),
            };
            match result {
                Ok(encoder) => {
                    append_selection_failures(discovery_reason, &unavailable);
                    let reason = encoder_selection_reason(preference, *backend, discovery_reason);
                    match &encoder {
                        Self::Nvenc(_) => {
                            let adapter = adapter_name(device)
                                .unwrap_or_else(|_| "NVIDIA adapter".to_owned());
                            info!("encoder: NVENC ({adapter}){reason}");
                        }
                        Self::Mf(mf) => info!("encoder: {}{reason}", mf.backend_name()),
                    }
                    return Ok(encoder);
                }
                Err(error) => unavailable.push(format!(
                    "{} unavailable: {error:#}",
                    match backend {
                        EncoderBackend::Nvenc => "NVENC",
                        EncoderBackend::MfHardware => "Media Foundation hardware H.264",
                        EncoderBackend::MfSoftware => "Microsoft software H.264",
                    }
                )),
            }
        }
        append_selection_failures(discovery_reason, &unavailable);
        bail!("no video encoder: {}", discovery_reason)
    }

    pub fn capabilities(&self) -> EncoderCapabilities {
        match self {
            Self::Nvenc(_) => EncoderCapabilities::NVENC,
            Self::Mf(_) => EncoderCapabilities::MF,
        }
    }

    pub fn backend(&self) -> EncoderBackend {
        match self {
            Self::Nvenc(_) => EncoderBackend::Nvenc,
            Self::Mf(encoder) if encoder.hardware() => EncoderBackend::MfHardware,
            Self::Mf(_) => EncoderBackend::MfSoftware,
        }
    }

    pub fn backend_name(&self) -> &str {
        match self {
            Self::Nvenc(_) => "NVENC",
            Self::Mf(encoder) => encoder.backend_name(),
        }
    }

    pub fn encode_texture(&mut self, texture: *mut c_void) -> Result<EncodedFrame> {
        match self {
            Self::Nvenc(encoder) => encoder.encode_texture(texture),
            Self::Mf(encoder) => encoder.encode_texture(texture),
        }
    }

    pub fn encode_texture_with_damage(
        &mut self,
        texture: *mut c_void,
        damage: Option<FrameDamage<'_>>,
    ) -> Result<EncodedFrame> {
        match self {
            Self::Nvenc(encoder) => encoder.encode_texture_with_damage(texture, damage),
            Self::Mf(encoder) => encoder.encode_texture(texture),
        }
    }

    pub fn cpu_conversion(&self) -> bool {
        matches!(self, Self::Mf(e) if !e.hardware() && e.converter_name().starts_with("BGRA"))
    }

    /// CPU-only source geometry/damage contract. The texture remains capture's
    /// native BGRA surface; software conversion owns the output dimensions.
    pub fn encode_cpu_texture(
        &mut self,
        texture: *mut c_void,
        damage: Option<&[crate::frame_damage::DamageRect]>,
    ) -> Result<EncodedFrame> {
        match self {
            Self::Mf(e) if !e.hardware() => e.encode_texture_with_damage(texture, damage),
            _ => bail!("CPU texture submission requires Microsoft software H.264"),
        }
    }

    pub fn encode_texture_with_feedback(
        &mut self,
        texture: *mut c_void,
        damage: Option<FrameDamage<'_>>,
        reconstruction_texture: *mut c_void,
    ) -> Result<EncodedFrame> {
        match self {
            Self::Nvenc(encoder) => {
                encoder.encode_texture_with_feedback(texture, damage, reconstruction_texture)
            }
            Self::Mf(encoder) => encoder.encode_texture(texture),
        }
    }

    pub fn force_idr(&mut self) {
        match self {
            Self::Nvenc(encoder) => encoder.force_idr(),
            Self::Mf(encoder) => encoder.force_idr(),
        }
    }

    pub fn idr_pending(&self) -> bool {
        match self {
            Self::Nvenc(encoder) => encoder.idr_pending(),
            Self::Mf(encoder) => encoder.idr_pending(),
        }
    }

    pub fn refine_qp_map_enabled(&self) -> bool {
        match self {
            Self::Nvenc(encoder) => encoder.refine_qp_map_enabled(),
            Self::Mf(_) => false,
        }
    }

    pub fn codec(&self) -> VideoCodec {
        match self {
            Self::Nvenc(encoder) => encoder.codec(),
            Self::Mf(_) => VideoCodec::H264,
        }
    }

    pub fn is_for_device(&self, device: *mut c_void) -> bool {
        match self {
            Self::Nvenc(encoder) => encoder.is_for_device(device),
            Self::Mf(encoder) => encoder.is_for_device(device),
        }
    }

    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        match self {
            Self::Nvenc(encoder) => encoder.set_bitrate(bitrate_bps),
            Self::Mf(encoder) => encoder.set_bitrate(bitrate_bps),
        }
    }

    pub fn set_max_qp(&mut self, max_qp: Option<u32>) -> Result<()> {
        match self {
            Self::Nvenc(encoder) => encoder.set_max_qp(max_qp),
            Self::Mf(encoder) => encoder.set_max_qp(max_qp),
        }
    }

    pub fn max_qp(&self) -> Option<u32> {
        match self {
            Self::Nvenc(encoder) => encoder.max_qp(),
            Self::Mf(encoder) => encoder.max_qp(),
        }
    }

    pub fn bitrate(&self) -> u32 {
        match self {
            Self::Nvenc(encoder) => encoder.bitrate(),
            Self::Mf(encoder) => encoder.bitrate(),
        }
    }

    pub fn fps(&self) -> u32 {
        match self {
            Self::Nvenc(encoder) => encoder.fps(),
            Self::Mf(encoder) => encoder.fps(),
        }
    }

    pub fn infinite_gop(&self) -> bool {
        match self {
            Self::Nvenc(encoder) => encoder.infinite_gop(),
            Self::Mf(encoder) => encoder.infinite_gop(),
        }
    }

    pub fn reset_rate_control(&mut self) -> Result<()> {
        match self {
            Self::Nvenc(encoder) => encoder.reset_rate_control(),
            Self::Mf(encoder) => encoder.reset_rate_control(),
        }
    }

    pub fn reconfigure_profile(&mut self, width: u32, height: u32, fps: u32) -> Result<()> {
        match self {
            Self::Nvenc(encoder) => encoder.reconfigure_profile(width, height, fps),
            Self::Mf(encoder) => encoder.reconfigure_profile(width, height, fps),
        }
    }

    pub fn sequence_params(&self) -> Result<Vec<u8>> {
        match self {
            Self::Nvenc(encoder) => encoder.sequence_params(),
            Self::Mf(encoder) => encoder.sequence_params(),
        }
    }
}

/// Probe the DLL and API table before entering the SDK's infallible lazy
/// initializer. Missing drivers and an older API are ordinary selection errors.
pub fn probe_nvenc_api() -> Result<()> {
    use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
        NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION, NVENCSTATUS, NV_ENCODE_API_FUNCTION_LIST,
        NV_ENCODE_API_FUNCTION_LIST_VER,
    };
    type GetVersion = unsafe extern "system" fn(*mut u32) -> NVENCSTATUS;
    type CreateInstance =
        unsafe extern "system" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
    let module = unsafe { LoadLibraryW(w!("nvEncodeAPI64.dll")) }
        .context("LoadLibrary(nvEncodeAPI64.dll)")?;
    struct Library(windows::Win32::Foundation::HMODULE);
    impl Drop for Library {
        fn drop(&mut self) {
            let _ = unsafe { FreeLibrary(self.0) };
        }
    }
    let module = Library(module);
    let version = unsafe { GetProcAddress(module.0, s!("NvEncodeAPIGetMaxSupportedVersion")) }
        .context("NvEncodeAPIGetMaxSupportedVersion export missing")?;
    let create = unsafe { GetProcAddress(module.0, s!("NvEncodeAPICreateInstance")) }
        .context("NvEncodeAPICreateInstance export missing")?;
    let version: GetVersion = unsafe { std::mem::transmute(version) };
    let create: CreateInstance = unsafe { std::mem::transmute(create) };
    let mut supported = 0;
    unsafe { version(&mut supported) }
        .result_without_string()
        .context("NvEncodeAPIGetMaxSupportedVersion")?;
    ensure!(
        (supported >> 4, supported & 15) >= (NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION),
        "NVIDIA driver API {}.{} is older than required {}.{}",
        supported >> 4,
        supported & 15,
        NVENCAPI_MAJOR_VERSION,
        NVENCAPI_MINOR_VERSION
    );
    let mut functions = NV_ENCODE_API_FUNCTION_LIST {
        version: NV_ENCODE_API_FUNCTION_LIST_VER,
        ..Default::default()
    };
    unsafe { create(&mut functions) }
        .result_without_string()
        .context("NvEncodeAPICreateInstance")?;
    ensure!(
        functions.nvEncOpenEncodeSessionEx.is_some()
            && functions.nvEncInitializeEncoder.is_some()
            && functions.nvEncEncodePicture.is_some(),
        "NVIDIA encoder API table is incomplete"
    );
    Ok(())
}

fn adapter_name(device: *mut c_void) -> Result<String> {
    // The encode factory receives a live D3D11 device; this borrow owns no ref.
    let device = unsafe { ID3D11Device::from_raw_borrowed(&device) }
        .ok_or_else(|| anyhow::anyhow!("null D3D11 device"))?;
    let dxgi: IDXGIDevice = device.cast()?;
    let desc = unsafe { dxgi.GetAdapter()?.GetDesc()? };
    let length = desc.Description.iter().position(|v| *v == 0).unwrap_or(128);
    Ok(String::from_utf16_lossy(&desc.Description[..length]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_selection_logs_keep_discovery_and_actual_fallback_reasons() {
        let auto = EncoderPreference::default();
        let nvenc = "NVENC unavailable: capture adapter is not NVIDIA";
        assert_eq!(
            encoder_selection_reason(auto, EncoderBackend::MfHardware, nvenc),
            format!("; {nvenc}")
        );
        let mut reasons = nvenc.to_owned();
        append_selection_failures(
            &mut reasons,
            &["Media Foundation hardware H.264 unavailable: initialization failed".to_owned()],
        );
        let software = encoder_selection_reason(auto, EncoderBackend::MfSoftware, &reasons);
        assert!(software.contains("no hardware encoder"));
        assert!(software.contains(nvenc));
        assert!(
            software.contains("Media Foundation hardware H.264 unavailable: initialization failed")
        );
        assert_eq!(
            encoder_selection_reason(
                EncoderPreference {
                    cpu_mode: true,
                    encoder: EncoderOverride::Nvenc
                },
                EncoderBackend::MfSoftware,
                &reasons
            ),
            " (CPU mode forced by tray setting)"
        );
        let adapter = EncoderAdapter {
            luid: 42,
            vendor_id: 0x8086,
        };
        let mut cache = EncoderDiscoveryCache::default();
        cache
            .resolve_with(adapter, || {
                Ok((EncoderBackend::MfHardware, nvenc.to_owned()))
            })
            .unwrap();
        cache.record_actual(adapter, EncoderBackend::MfSoftware, &reasons);
        assert_eq!(cache.reason(adapter), Some(reasons.as_str()));
        assert_eq!(
            cache
                .resolve_with(adapter, || panic!("cached fallback must not re-probe"))
                .unwrap(),
            EncoderBackend::MfSoftware
        );
    }

    #[test]
    fn discovery_is_cached_by_adapter_across_warm_reconnect_and_recovery() {
        let first = EncoderAdapter {
            luid: 10,
            vendor_id: 0x10de,
        };
        let second = EncoderAdapter {
            luid: 20,
            vendor_id: 0x8086,
        };
        let mut cache = EncoderDiscoveryCache::default();
        assert_eq!(
            cache
                .resolve_with(first, || Ok((EncoderBackend::Nvenc, String::new())))
                .unwrap(),
            EncoderBackend::Nvenc
        );
        assert_eq!(
            cache
                .resolve_with(first, || panic!("unchanged adapter must not be re-probed"))
                .unwrap(),
            EncoderBackend::Nvenc
        );
        assert_eq!(
            cache
                .resolve_with(second, || Ok((
                    EncoderBackend::MfHardware,
                    "NVENC unavailable".to_owned()
                )))
                .unwrap(),
            EncoderBackend::MfHardware
        );
        cache.record_actual(
            first,
            EncoderBackend::MfSoftware,
            "NVENC and hardware unavailable",
        );
        assert_eq!(cache.reason(first), Some("NVENC and hardware unavailable"));
        assert_eq!(
            cache
                .resolve_with(first, || panic!("actual fallback must remain cached"))
                .unwrap(),
            EncoderBackend::MfSoftware
        );
        assert_eq!(
            cache
                .resolve_with(second, || panic!(
                    "returning to an adapter must reuse discovery"
                ))
                .unwrap(),
            EncoderBackend::MfHardware
        );
    }

    #[test]
    fn auto_prefers_nvenc_then_hardware_then_software() {
        use EncoderBackend::*;
        let preference = EncoderPreference::default();
        assert_eq!(
            select_backend(&[MfSoftware, MfHardware, Nvenc], preference),
            Some(Nvenc)
        );
        assert_eq!(
            select_backend(&[MfSoftware, MfHardware], preference),
            Some(MfHardware)
        );
        assert_eq!(select_backend(&[MfSoftware], preference), Some(MfSoftware));
        assert_eq!(select_backend(&[], preference), None);
    }

    #[test]
    fn cpu_mode_skips_all_hardware_even_with_an_override() {
        for encoder in [
            EncoderOverride::Auto,
            EncoderOverride::Nvenc,
            EncoderOverride::MfHardware,
            EncoderOverride::MfSoftware,
        ] {
            let preference = EncoderPreference {
                cpu_mode: true,
                encoder,
            };
            assert_eq!(selection_order(preference), &[EncoderBackend::MfSoftware]);
            assert_eq!(
                select_backend(
                    &[EncoderBackend::Nvenc, EncoderBackend::MfHardware],
                    preference
                ),
                None
            );
        }
    }

    #[test]
    fn diagnostic_overrides_do_not_fall_back() {
        for (encoder, backend) in [
            (EncoderOverride::Nvenc, EncoderBackend::Nvenc),
            (EncoderOverride::MfHardware, EncoderBackend::MfHardware),
            (EncoderOverride::MfSoftware, EncoderBackend::MfSoftware),
        ] {
            let preference = EncoderPreference {
                cpu_mode: false,
                encoder,
            };
            assert_eq!(select_backend(&[backend], preference), Some(backend));
            assert_eq!(select_backend(&[], preference), None);
        }
    }

    #[test]
    fn mf_forces_h264_and_disables_nvenc_extensions() {
        assert_eq!(
            EncoderCapabilities::MF.effective_codec(VideoCodec::Hevc),
            VideoCodec::H264
        );
        assert_eq!(
            EncoderCapabilities::MF.effective_codec(VideoCodec::H264),
            VideoCodec::H264
        );
        assert_eq!(
            EncoderCapabilities::NVENC.effective_codec(VideoCodec::Hevc),
            VideoCodec::Hevc
        );
        const {
            assert!(!EncoderCapabilities::MF.main10);
            assert!(!EncoderCapabilities::MF.roi_qp_map);
            assert!(!EncoderCapabilities::MF.refine_feedback);
            assert!(!EncoderCapabilities::MF.hevc_tiles);
        }
    }
}
#[path = "cpu_policy.rs"]
pub mod cpu_policy;
