//! Binary-owned encode boundary; preserves existing streaming policy.
use super::{
    bail, benchmark, ensure, frame_damage, hybrid, info, quality, warn, AqMode, BenchmarkAqMode,
    BenchmarkHybridCacheMode, BenchmarkMultipass, BenchmarkPreset, BenchmarkResourceMode,
    BenchmarkRoiMode, BenchmarkTuning, CachedIdr, CachedIdrReplay, CapturedFrame, Context,
    Controller, Decision, EncodedFrame, EncoderBackend, EncoderCapabilities, EncoderOptions,
    EncoderPreference, FrameDamage, Instant, Interface, Multipass, Nvenc, Preset, Profile,
    ProfileTransition, QuiescentTilePlanner, RateControlMode, ResourceMode, Result, RoiMode,
    ScaleOutput, Scaler, Signaling, Tuning, VideoCodec, VideoEncoder, VideoMode, ViewportCrop,
    ViewportGeometry, WssAdjustment, RECT, STATIC_REPLAY_DELAY,
};

#[allow(clippy::too_many_arguments)]
pub(super) fn create_encoder_with_fallback(
    device: *mut core::ffi::c_void,
    width: u32,
    height: u32,
    native_width: u32,
    native_height: u32,
    fps: u32,
    bitrate_bps: u32,
    requested: Preset,
    codec: VideoCodec,
    infinite_gop: bool,
) -> Result<Nvenc> {
    let create = |options| {
        Nvenc::new_with_options_native_codec_gop(
            device,
            width,
            height,
            native_width,
            native_height,
            fps,
            bitrate_bps,
            options,
            codec,
            infinite_gop,
        )
    };
    // WSS v4 (infinite GOP) refines debt tiles through an HEVC delta-QP map.
    // Without it, refines fall back to whole frames.
    let primary = if infinite_gop && codec == VideoCodec::Hevc {
        let mut options = EncoderOptions::production(requested);
        options.roi_mode = RoiMode::Refine;
        create(options).or_else(|error| {
            warn!(
                ?error,
                "NVENC refused the refine QP map; v4 refines use whole frames"
            );
            create(EncoderOptions::production(requested))
        })
    } else {
        create(EncoderOptions::production(requested))
    };
    primary.or_else(|first| {
        if matches!(requested, Preset::P3) {
            warn!(
                ?first,
                width, height, fps, "P3 initialization failed; retrying P1"
            );
            Nvenc::new_with_options_native_codec_gop(
                device,
                width,
                height,
                native_width,
                native_height,
                fps,
                bitrate_bps,
                EncoderOptions::production(Preset::P1),
                codec,
                infinite_gop,
            )
        } else {
            Err(first)
        }
    })
}

pub(super) fn benchmark_encoder_options(profile: &benchmark::BenchmarkProfile) -> EncoderOptions {
    EncoderOptions {
        preset: match profile.encoder.preset {
            BenchmarkPreset::P1 => Preset::P1,
            BenchmarkPreset::P2 => Preset::P2,
            BenchmarkPreset::P3 => Preset::P3,
            BenchmarkPreset::P4 => Preset::P4,
            BenchmarkPreset::P5 => Preset::P5,
            BenchmarkPreset::P6 => Preset::P6,
            BenchmarkPreset::P7 => Preset::P7,
        },
        tuning: match profile.encoder.tuning {
            BenchmarkTuning::UltraLowLatency => Tuning::UltraLowLatency,
            BenchmarkTuning::LowLatency => Tuning::LowLatency,
        },
        rate_control: match profile.encoder.rate_control_mode {
            benchmark::BenchmarkRateControlMode::Cbr => RateControlMode::Cbr,
            benchmark::BenchmarkRateControlMode::ExternalBudgetVbr => {
                RateControlMode::ExternalBudgetVbr {
                    target_quality: profile.encoder.target_quality.unwrap_or(23),
                    peak_bitrate_multiplier: profile.encoder.peak_bitrate_multiplier.unwrap_or(4),
                }
            }
        },
        multipass: match profile.encoder.multipass {
            BenchmarkMultipass::Disabled => Multipass::Disabled,
            BenchmarkMultipass::Quarter => Multipass::Quarter,
            BenchmarkMultipass::Full => Multipass::Full,
        },
        aq_mode: match profile.effective_aq_mode() {
            BenchmarkAqMode::None => AqMode::None,
            BenchmarkAqMode::Spatial => AqMode::Spatial,
            BenchmarkAqMode::Temporal => AqMode::Temporal,
        },
        aq_strength: profile.encoder.aq_strength,
        vbv_frames: profile.encoder.vbv_frames,
        lookahead_depth: profile.encoder.lookahead_depth,
        weighted_prediction: profile.encoder.weighted_prediction,
        gop_seconds: profile.gop_seconds,
        roi_mode: match profile.effective_roi_mode() {
            BenchmarkRoiMode::Off => RoiMode::Off,
            BenchmarkRoiMode::Damage => RoiMode::Damage,
            BenchmarkRoiMode::TextEdge => RoiMode::TextEdge,
            BenchmarkRoiMode::Feedback | BenchmarkRoiMode::FeedbackHeadroom => RoiMode::Feedback,
        },
        resource_mode: match profile.encoder.resource_mode {
            BenchmarkResourceMode::PerFrame => ResourceMode::PerFrame,
            BenchmarkResourceMode::Persistent => ResourceMode::Persistent,
        },
        async_depth: profile.encoder.async_depth,
        main10: profile.encoder.main10,
    }
}

/// Source rectangle the encoder scales to its output for `mode`.
pub(super) fn encode_crop(
    source_width: u32,
    source_height: u32,
    viewport_crop: Option<ViewportCrop>,
    mode: VideoMode,
) -> frame_damage::DamageRect {
    let full = frame_damage::DamageRect {
        left: 0,
        top: 0,
        right: source_width,
        bottom: source_height,
    };
    if mode.label == "native" {
        return full;
    }
    viewport_crop.map_or(full, |crop| frame_damage::DamageRect {
        left: crop.x,
        top: crop.y,
        right: crop.x + crop.width,
        bottom: crop.y + crop.height,
    })
}

/// Capture damage for the encoder, plus a prepared v4 refine QP map. A refine
/// frame carries its map even without capture damage (idle follow-up).
pub(super) fn encode_frame_damage<'a>(
    frame: &'a CapturedFrame<'_>,
    viewport_crop: Option<ViewportCrop>,
    mode: VideoMode,
    refine_map: Option<&'a frame_damage::HevcQpDeltaMap>,
) -> Option<FrameDamage<'a>> {
    let rects = match (frame.damage(), refine_map) {
        (Some(rects), _) => rects,
        (None, Some(_)) => &[],
        (None, None) => return None,
    };
    Some(FrameDamage {
        source_width: frame.width(),
        source_height: frame.height(),
        crop: encode_crop(frame.width(), frame.height(), viewport_crop, mode),
        rects,
        edge_scores: None,
        prepared_qp_map: refine_map,
    })
}

pub(super) fn quiescent_damage_rects<'a>(
    _capture_damage: Option<&'a [frame_damage::DamageRect]>,
    temporal_damage: &'a [frame_damage::DamageRect],
    is_idle_snapshot: bool,
) -> &'a [frame_damage::DamageRect] {
    if is_idle_snapshot {
        &[]
    } else {
        temporal_damage
    }
}

pub(super) fn quiescent_validation_requests(
    planner: &QuiescentTilePlanner,
    viewport: ViewportGeometry,
    validation_sequence: u32,
    capture_damage: Option<&[frame_damage::DamageRect]>,
    is_idle_snapshot: bool,
) -> Vec<hybrid::QuiescentValidationRequest> {
    planner.validation_requests(
        viewport,
        validation_sequence,
        capture_damage,
        is_idle_snapshot,
    )
}

pub(super) fn quiescent_immediate_damage_rects<'a>(
    cache_mode: BenchmarkHybridCacheMode,
    capture_damage: Option<&'a [frame_damage::DamageRect]>,
    verified_damage: &'a [frame_damage::DamageRect],
    is_idle_snapshot: bool,
) -> &'a [frame_damage::DamageRect] {
    if is_idle_snapshot {
        return &[];
    }
    match cache_mode {
        BenchmarkHybridCacheMode::DdaGated => capture_damage.unwrap_or(&[]),
        BenchmarkHybridCacheMode::SuppressReactivate => verified_damage,
    }
}

/// Whether frames take the P010 Main10 conversion. The created encoder's
/// capabilities gate unsupported Main10 before any P010 texture is prepared.
pub(super) fn main10_conversion(wanted: bool, encoder: EncoderCapabilities) -> bool {
    wanted && encoder.main10
}

/// The normal NVENC recovery path keeps its prepared input and does no extra
/// scaling. Only a Main10 input falling back to an 8-bit encoder needs BGRA.
pub(super) fn optional_bgra_recovery_input<T>(
    main10_input: bool,
    capabilities: EncoderCapabilities,
    prepare_bgra: impl FnOnce() -> Result<T>,
) -> Result<Option<T>> {
    if main10_input && !capabilities.main10 {
        prepare_bgra().map(Some)
    } else {
        Ok(None)
    }
}

/// The selected encoder's codec is what goes on the wire. Media Foundation
/// emits H.264 only, so the session codec follows the encoder that exists.
pub(super) fn adopt_encoder_codec(active: &mut VideoCodec, encoder: Option<&VideoEncoder>) {
    let Some(encoder) = encoder else {
        return;
    };
    let codec = encoder.codec();
    if codec != *active {
        info!(
            from = active.name(),
            to = codec.name(),
            backend = encoder.backend_name(),
            "encoder cannot emit the session codec; using its codec"
        );
        *active = codec;
    }
}

/// NVENC retains master's immediate legacy offer path. MF declares H.264
/// capabilities before capture/encoder activation, even on a static desktop.
pub(super) fn startup_offer_command(
    sdp: String,
    backend: EncoderBackend,
) -> super::TransportCommand {
    match backend {
        EncoderBackend::Nvenc => super::TransportCommand::Offer(sdp),
        EncoderBackend::MfHardware | EncoderBackend::MfSoftware => {
            super::TransportCommand::OfferWithCapabilities(sdp, EncoderCapabilities::MF)
        }
    }
}

pub(super) fn ensure_rtc_encoder_codec(
    backend: EncoderBackend,
    wss_active: bool,
    negotiated: Option<VideoCodec>,
    actual: VideoCodec,
) -> Result<()> {
    ensure!(
        // NVENC retains master's one-way downgrade and warm offer behavior.
        backend == EncoderBackend::Nvenc
            || wss_active
            || negotiated.is_none_or(|codec| codec == actual),
        "encoder codec changed to {}; fresh RTC negotiation required",
        actual.name()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn create_local_encoder(
    preference: EncoderPreference,
    discovered: EncoderBackend,
    discovery_reason: &mut String,
    device: *mut core::ffi::c_void,
    width: u32,
    height: u32,
    native_width: u32,
    native_height: u32,
    fps: u32,
    bitrate_bps: u32,
    requested: Preset,
    codec: VideoCodec,
    benchmark_profile: Option<&benchmark::BenchmarkProfile>,
    infinite_gop: bool,
) -> Result<VideoEncoder> {
    // Media Foundation takes only the bitrate and GOP from a benchmark profile.
    let mf_bitrate_bps = benchmark_profile.map_or(bitrate_bps, |profile| profile.bitrate_bps);
    let gop_seconds = benchmark_profile.map_or(
        EncoderOptions::production(requested).gop_seconds,
        |profile| profile.gop_seconds,
    );
    let encoder = VideoEncoder::select(
        device,
        width,
        height,
        fps,
        mf_bitrate_bps,
        infinite_gop,
        u32::from(gop_seconds),
        preference,
        Some(discovered),
        discovery_reason,
        || {
            if let Some(profile) = benchmark_profile {
                return Nvenc::new_with_options_native_codec(
                    device,
                    width,
                    height,
                    native_width,
                    native_height,
                    fps,
                    profile.bitrate_bps,
                    benchmark_encoder_options(profile),
                    codec,
                );
            }
            create_encoder_with_fallback(
                device,
                width,
                height,
                native_width,
                native_height,
                fps,
                bitrate_bps,
                requested,
                codec,
                infinite_gop,
            )
        },
    )?;
    if benchmark_profile.is_some() && !encoder.capabilities().roi_qp_map {
        info!(
            backend = encoder.backend_name(),
            "benchmark encoder options skipped: preset, rate control, multipass, AQ, ROI, Main10 and resource mode are NVENC-only"
        );
    }
    Ok(encoder)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn configure_encoder_profile(
    encoder: &mut Option<VideoEncoder>,
    preference: EncoderPreference,
    discovered: &mut EncoderBackend,
    discovery_reason: &mut String,
    device: *mut core::ffi::c_void,
    mode: VideoMode,
    native_width: u32,
    native_height: u32,
    bitrate_bps: u32,
    requested: Preset,
    codec: VideoCodec,
    benchmark_profile: Option<&benchmark::BenchmarkProfile>,
) -> Result<()> {
    let infinite_gop = encoder.as_ref().is_some_and(VideoEncoder::infinite_gop);
    let reconfigure = encoder
        .as_mut()
        .context("encoder unavailable")?
        .reconfigure_profile(mode.width, mode.height, mode.fps);
    if let Err(e) = reconfigure {
        warn!(
            ?e,
            profile = mode.label,
            "encoder profile reconfigure failed; recreating once"
        );
        *encoder = None;
        *encoder = Some(
            create_local_encoder(
                preference,
                *discovered,
                discovery_reason,
                device,
                mode.width,
                mode.height,
                native_width,
                native_height,
                mode.fps,
                bitrate_bps,
                requested,
                codec,
                benchmark_profile,
                infinite_gop,
            )
            .with_context(|| format!("recreate encoder for {}", mode.label))?,
        );
        *discovered = encoder.as_ref().expect("replacement selected").backend();
    }
    encoder.as_mut().expect("configured above").force_idr();
    Ok(())
}

/// Recreate a known-good encoder contract and force its recovery IDR. This is
/// intentionally separate from a target transition: callers only commit the
/// target after its IDR has been admitted by the transport pump.
#[allow(clippy::too_many_arguments)]
pub(super) fn recreate_and_encode_profile(
    encoder: &mut Option<VideoEncoder>,
    preference: EncoderPreference,
    discovered: &mut EncoderBackend,
    discovery_reason: &mut String,
    device: *mut core::ffi::c_void,
    mode: VideoMode,
    native_width: u32,
    native_height: u32,
    bitrate_bps: u32,
    preset: Preset,
    codec: &mut VideoCodec,
    benchmark_profile: Option<&benchmark::BenchmarkProfile>,
    texture: *mut core::ffi::c_void,
    reconstruction_texture: Option<*mut core::ffi::c_void>,
    main10_input: bool,
    prepare_bgra: impl FnOnce() -> Result<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>,
) -> Result<EncodedFrame> {
    let infinite_gop = encoder.as_ref().is_some_and(VideoEncoder::infinite_gop);
    *encoder = None;
    let mut replacement = create_local_encoder(
        preference,
        *discovered,
        discovery_reason,
        device,
        mode.width,
        mode.height,
        native_width,
        native_height,
        mode.fps,
        bitrate_bps,
        preset,
        *codec,
        benchmark_profile,
        infinite_gop,
    )?;
    *discovered = replacement.backend();
    // Record the actual codec before any input preparation/encode can fail.
    // The next producer pass must not retry an unsupported HEVC contract.
    adopt_encoder_codec(codec, Some(&replacement));
    let bgra =
        optional_bgra_recovery_input(main10_input, replacement.capabilities(), prepare_bgra)?;
    // `bgra` owns a COM reference until the synchronous encode completes.
    let texture = bgra.as_ref().map_or(texture, Interface::as_raw);
    replacement.force_idr();
    let encoded = if let Some(reconstruction_texture) = reconstruction_texture {
        replacement.encode_texture_with_feedback(texture, None, reconstruction_texture)?
    } else {
        replacement.encode_texture(texture)?
    };
    ensure!(
        encoded.is_idr,
        "recovery encoder did not produce a forced IDR"
    );
    *encoder = Some(replacement);
    Ok(encoded)
}

pub(super) fn prepare_profile_texture(
    scaler: &mut Option<Scaler>,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    input_width: u32,
    input_height: u32,
    profile: Profile,
    main10: bool,
) -> Result<ScaleOutput> {
    prepare_profile_texture_with_resource(
        scaler,
        device,
        texture,
        input_width,
        input_height,
        profile,
        main10,
        ResourceMode::PerFrame,
    )
}

pub(super) fn full_profile_uses_native_texture(
    profile: Profile,
    width: u32,
    height: u32,
    resource_mode: ResourceMode,
) -> bool {
    profile == Profile::Full
        && width == 1920
        && height == 1080
        && resource_mode == ResourceMode::PerFrame
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_profile_texture_with_resource(
    scaler: &mut Option<Scaler>,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    input_width: u32,
    input_height: u32,
    profile: Profile,
    main10: bool,
    resource_mode: ResourceMode,
) -> Result<ScaleOutput> {
    if main10 {
        ensure!(
            profile == Profile::Full,
            "Main10 is limited to the Full 1920x1080 profile"
        );
        if scaler
            .as_ref()
            .is_none_or(|current| !current.is_for_device(device))
        {
            *scaler = Some(
                Scaler::new(device, input_width, input_height)
                    .context("initialize D3D11 video scaler")?,
            );
        }
        return scaler
            .as_mut()
            .expect("initialized above")
            .scale_main10(texture)
            .context("D3D11 BGRA to P010 conversion");
    }
    if profile == Profile::Full {
        let texture_device = unsafe { texture.GetDevice() }.context("Full texture GetDevice")?;
        if texture_device.as_raw() != device.as_raw() {
            bail!("Full texture is not on the capture D3D11 device");
        }
        let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        ensure!(
            desc.Width == input_width && desc.Height == input_height,
            "Full texture dimensions {}x{} do not match native capture {}x{}",
            desc.Width,
            desc.Height,
            input_width,
            input_height
        );
        if full_profile_uses_native_texture(profile, desc.Width, desc.Height, resource_mode) {
            return Ok(ScaleOutput {
                texture: texture.as_raw(),
                scale_us: 0,
                width: desc.Width,
                height: desc.Height,
            });
        }
    }
    if scaler
        .as_ref()
        .is_none_or(|current| !current.is_for_device(device))
    {
        *scaler = Some(
            Scaler::new(device, input_width, input_height)
                .context("initialize D3D11 video scaler")?,
        );
    }
    scaler
        .as_mut()
        .expect("initialized above")
        .scale(texture, profile)
        .context("D3D11 video scale")
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_video_mode_texture(
    scaler: &mut Option<Scaler>,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    input_width: u32,
    input_height: u32,
    mode: VideoMode,
    main10: bool,
    resource_mode: ResourceMode,
) -> Result<ScaleOutput> {
    if main10 {
        ensure!(
            mode.width == 1920 && mode.height == 1080,
            "Main10 mode must remain 1920x1080"
        );
        return prepare_profile_texture_with_resource(
            scaler,
            device,
            texture,
            input_width,
            input_height,
            Profile::Full,
            true,
            resource_mode,
        );
    }
    if mode.label == "native" {
        let texture_device = unsafe { texture.GetDevice() }.context("native texture GetDevice")?;
        ensure!(
            texture_device.as_raw() == device.as_raw(),
            "native texture is not on the capture D3D11 device"
        );
        let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        ensure!(
            desc.Width == input_width
                && desc.Height == input_height
                && desc.Width == mode.width
                && desc.Height == mode.height,
            "native texture {}x{} does not match capture {}x{} or encoder {}x{}",
            desc.Width,
            desc.Height,
            input_width,
            input_height,
            mode.width,
            mode.height
        );
        return Ok(ScaleOutput {
            texture: texture.as_raw(),
            scale_us: 0,
            width: desc.Width,
            height: desc.Height,
        });
    }
    let output = prepare_profile_texture_with_resource(
        scaler,
        device,
        texture,
        input_width,
        input_height,
        mode.profile,
        false,
        resource_mode,
    )?;
    ensure!(
        output.width == mode.width && output.height == mode.height,
        "scaled {} output {}x{} does not match encoder {}x{}",
        mode.label,
        output.width,
        output.height,
        mode.width,
        mode.height
    );
    Ok(output)
}

/// Recreate the same visible frame in BGRA after the replacement backend is
/// known to need it. Retain the surface even when the scaler owns its target.
#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_recovery_bgra_texture(
    scaler: &mut Option<Scaler>,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    input_width: u32,
    input_height: u32,
    mode: VideoMode,
    crop: Option<ViewportCrop>,
) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
    let output = if crop.is_some() && mode.label != "native" {
        prepare_profile_texture_with_crop(
            scaler,
            device,
            texture,
            input_width,
            input_height,
            mode.profile,
            crop,
            false,
        )?
    } else {
        prepare_video_mode_texture(
            scaler,
            device,
            texture,
            input_width,
            input_height,
            mode,
            false,
            ResourceMode::PerFrame,
        )?
    };
    ensure!(
        output.width == mode.width && output.height == mode.height,
        "BGRA recovery texture does not match the replacement encoder"
    );
    unsafe {
        windows::Win32::Graphics::Direct3D11::ID3D11Texture2D::from_raw_borrowed(&output.texture)
    }
    .context("BGRA recovery texture is null")
    .cloned()
}

pub(super) fn apply_quality_decision(
    quality: &mut Controller,
    decision: Decision,
    profiles: &mut ProfileTransition,
    latest_idr: &mut Option<CachedIdr>,
    encoder: &mut Option<VideoEncoder>,
    signaling: &mut Signaling,
) {
    let now = Instant::now();
    let mut applied = true;
    if let Some(enc) = encoder.as_mut() {
        if let Err(e) = enc.set_bitrate(decision.target_bps) {
            warn!(
                ?e,
                bitrate_bps = decision.target_bps,
                "encoder bitrate update failed"
            );
            applied = false;
        }
    }
    if !applied {
        return;
    }
    quality.apply(now, decision);
    let reason = decision.reason.label();
    let applied_bitrate = encoder
        .as_ref()
        .map(VideoEncoder::bitrate)
        .unwrap_or(decision.target_bps);
    let staged = profiles.stage(decision.profile, reason);
    if staged {
        // Never replay an IDR whose SPS/PPS describes dimensions the
        // controller has already asked us to leave.
        *latest_idr = None;
        return;
    }
    // A pinned fallback still follows bitrate decisions, but reports the real
    // encoder dimensions rather than the controller's unavailable profile.
    if profiles.announced == Some(profiles.active) {
        let active = profiles.active;
        signaling.send_quality(
            active.label,
            active.width,
            active.height,
            active.fps,
            applied_bitrate,
            quality.bwe(),
            reason,
            profiles.stream_mode().label(),
        );
    }
}

pub(super) fn apply_wss_adjustment(
    quality: &mut Controller,
    adjustment: WssAdjustment,
    encoder_bitrate_bps: u32,
    profiles: &mut ProfileTransition,
    latest_idr: &mut Option<CachedIdr>,
    encoder: &mut Option<VideoEncoder>,
    signaling: &mut Signaling,
) -> bool {
    let adjustment = WssAdjustment {
        mode: if profiles.pinned {
            profiles.active
        } else {
            adjustment.mode
        },
        ..adjustment
    };
    if let Some(enc) = encoder.as_mut() {
        if let Err(error) = enc.set_bitrate(encoder_bitrate_bps) {
            warn!(
                ?error,
                bitrate_bps = encoder_bitrate_bps,
                "WSS encoder bitrate update failed"
            );
            return false;
        }
    }
    quality.apply(
        Instant::now(),
        Decision {
            profile: adjustment.mode.profile,
            target_bps: adjustment.target_bps,
            reason: quality::Reason::Bitrate,
        },
    );
    if profiles.stage_mode(adjustment.mode, adjustment.reason) {
        *latest_idr = None;
        if let Some(enc) = encoder.as_mut() {
            enc.force_idr();
        }
        return true;
    }
    if profiles.announced == Some(profiles.active) {
        let active = profiles.active;
        signaling.send_quality(
            active.label,
            active.width,
            active.height,
            active.fps,
            encoder
                .as_ref()
                .map(VideoEncoder::bitrate)
                .unwrap_or(adjustment.target_bps),
            quality.bwe(),
            adjustment.reason,
            profiles.stream_mode().label(),
        );
    }
    false
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_profile_texture_with_crop(
    scaler: &mut Option<Scaler>,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    input_width: u32,
    input_height: u32,
    profile: Profile,
    crop: Option<ViewportCrop>,
    main10: bool,
) -> Result<ScaleOutput> {
    let Some(crop) = crop else {
        return prepare_profile_texture(
            scaler,
            device,
            texture,
            input_width,
            input_height,
            profile,
            main10,
        );
    };
    if crop.x == 0 && crop.y == 0 && crop.width == input_width && crop.height == input_height {
        return prepare_profile_texture(
            scaler,
            device,
            texture,
            input_width,
            input_height,
            profile,
            main10,
        );
    }
    if scaler
        .as_ref()
        .is_none_or(|current| !current.is_for_device(device))
    {
        *scaler = Some(
            Scaler::new(device, input_width, input_height)
                .context("initialize D3D11 video scaler")?,
        );
    }
    let source = RECT {
        left: crop.x as i32,
        top: crop.y as i32,
        right: (crop.x + crop.width) as i32,
        bottom: (crop.y + crop.height) as i32,
    };
    if main10 {
        ensure!(
            profile == Profile::Full,
            "Main10 crop is limited to the Full 1920x1080 profile"
        );
        scaler
            .as_mut()
            .expect("initialized above")
            .scale_main10_with_source(texture, source)
            .context("D3D11 cropped BGRA to P010 conversion")
    } else {
        scaler
            .as_mut()
            .expect("initialized above")
            .scale_with_source(texture, profile, source)
            .context("D3D11 cropped video scale")
    }
}

impl CachedIdrReplay {
    pub(super) fn on_ice_connected(&mut self) {
        self.initial_frame_pending = true;
    }

    pub(super) fn on_keyframe_requested(&mut self, now: Instant) {
        self.recovery_requested_at.get_or_insert(now);
    }

    pub(super) fn on_fresh_idr_admitted(&mut self) {
        self.initial_frame_pending = false;
        self.recovery_requested_at = None;
    }

    pub(super) fn is_due(&self, now: Instant, cached_idr_available: bool) -> bool {
        cached_idr_available
            && (self.initial_frame_pending
                || self
                    .recovery_requested_at
                    .is_some_and(|requested| now.duration_since(requested) >= STATIC_REPLAY_DELAY))
    }

    pub(super) fn on_replayed(&mut self) {
        self.initial_frame_pending = false;
        self.recovery_requested_at = None;
    }

    pub(super) fn on_not_admitted(&mut self, now: Instant) {
        self.initial_frame_pending = false;
        self.recovery_requested_at = Some(now);
    }
}
