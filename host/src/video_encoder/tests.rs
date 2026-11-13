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
    assert!(software.contains("Media Foundation hardware H.264 unavailable: initialization failed"));
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

#[test]
fn an_out_of_range_mf_gop_fails_the_backend_and_records_why_instead_of_aborting_selection() {
    let mut reason = String::new();
    let result = VideoEncoder::select(
        std::ptr::null_mut(),
        1920,
        1080,
        30,
        5_000_000,
        false,
        u32::from(u8::MAX) + 1,
        EncoderPreference {
            cpu_mode: true,
            encoder: EncoderOverride::Auto,
        },
        None,
        &mut reason,
        || bail!("NVENC is not used in CPU mode"),
    );
    assert!(result.is_err());
    // Selection reached its end: the failed backend is recorded.
    assert!(!reason.is_empty());
}
