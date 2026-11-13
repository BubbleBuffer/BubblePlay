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
fn parse_pairs(pairs: &[(&str, &str)]) -> (SoftwareEnv, Vec<String>) {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    SoftwareEnv::parse(|name| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    })
}
#[test]
fn software_env_unset_and_empty_use_measured_defaults() {
    let (env, warnings) = parse_pairs(&[]);
    assert_eq!(env, SoftwareEnv::default());
    assert!(warnings.is_empty());
    assert_eq!(env.tuning.worker_threads, MftWorkerThreads::MftDefault);
    assert_eq!(env.tuning.quality_speed, 0);
    assert!(env.tuning.cabac && env.tuning.low_latency);
    assert!(env.warp_bypass && !env.force_cpu_conversion);
    let (empty, warnings) = parse_pairs(&[
        ("BP_CPU_MFT_THREADS", "  "),
        ("BP_CPU_MFT_CABAC", ""),
        ("BP_CPU_WARP_BYPASS", ""),
    ]);
    assert_eq!(empty, SoftwareEnv::default());
    assert!(warnings.is_empty());
}
#[test]
fn software_env_parses_each_documented_value() {
    let (env, warnings) = parse_pairs(&[
        ("BP_CPU_MFT_THREADS", "4"),
        ("BP_CPU_MFT_QUALITY_SPEED", "100"),
        ("BP_CPU_MFT_CABAC", "0"),
        ("BP_CPU_MFT_LOW_LATENCY", "0"),
        ("BP_CPU_WARP_BYPASS", "0"),
        ("BP_CPU_FORCE_CONVERSION", "1"),
    ]);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(env.tuning.worker_threads, MftWorkerThreads::Explicit(4));
    assert_eq!(env.tuning.quality_speed, 100);
    assert!(!env.tuning.cabac && !env.tuning.low_latency);
    assert!(!env.warp_bypass && env.force_cpu_conversion);
    for (value, threads) in [
        ("1", MftWorkerThreads::Explicit(1)),
        ("2", MftWorkerThreads::Explicit(2)),
        ("default", MftWorkerThreads::MftDefault),
        ("half", MftWorkerThreads::HalfCores),
    ] {
        let (env, warnings) = parse_pairs(&[("BP_CPU_MFT_THREADS", value)]);
        assert!(warnings.is_empty());
        assert_eq!(env.tuning.worker_threads, threads);
    }
}
#[test]
fn software_env_rejects_invalid_values_back_to_defaults_with_warnings() {
    for pairs in [
        vec![("BP_CPU_MFT_THREADS", "3")],
        vec![("BP_CPU_MFT_THREADS", "8")],
        vec![("BP_CPU_MFT_QUALITY_SPEED", "101")],
        vec![("BP_CPU_MFT_QUALITY_SPEED", "-1")],
        vec![("BP_CPU_MFT_CABAC", "true")],
        vec![("BP_CPU_MFT_LOW_LATENCY", "2")],
        vec![("BP_CPU_WARP_BYPASS", "no")],
        vec![("BP_CPU_FORCE_CONVERSION", "yes")],
    ] {
        let (env, warnings) = parse_pairs(&pairs);
        assert_eq!(env, SoftwareEnv::default(), "{pairs:?}");
        assert_eq!(warnings.len(), 1, "{pairs:?}");
    }
}
#[test]
fn worker_thread_count_follows_request_and_parallelism() {
    assert_eq!(MftWorkerThreads::HalfCores.count(16), Some(8));
    assert_eq!(MftWorkerThreads::HalfCores.count(8), Some(4));
    assert_eq!(MftWorkerThreads::HalfCores.count(2), Some(1));
    assert_eq!(MftWorkerThreads::HalfCores.count(1), Some(1));
    assert_eq!(MftWorkerThreads::Explicit(2).count(16), Some(2));
    assert_eq!(MftWorkerThreads::MftDefault.count(16), None);
}
#[test]
fn warp_is_identified_by_vendor_device_or_software_flag() {
    let warp = AdapterKind {
        vendor_id: 0x1414,
        device_id: 0x8c,
        software: false,
    };
    assert!(warp.is_warp());
    assert!(AdapterKind {
        software: true,
        vendor_id: 0x10de,
        device_id: 0x2684
    }
    .is_warp());
    assert!(!AdapterKind {
        software: false,
        vendor_id: 0x10de,
        device_id: 0x2684
    }
    .is_warp());
    assert!(!AdapterKind {
        software: false,
        vendor_id: 0x1414,
        device_id: 0x8d
    }
    .is_warp());
}
#[test]
fn software_video_processor_requires_matching_geometry_and_no_forced_cpu() {
    assert!(software_video_processor_eligible(
        true,
        false,
        (1920, 1080),
        (1920, 1080)
    ));
    // A size mismatch forces the CPU route rather than a hardware-style rejection.
    assert!(!software_video_processor_eligible(
        true,
        false,
        (2560, 1440),
        (1920, 1080)
    ));
    assert!(!software_video_processor_eligible(
        true,
        true,
        (1920, 1080),
        (1920, 1080)
    ));
    assert!(!software_video_processor_eligible(
        false,
        false,
        (1920, 1080),
        (1920, 1080)
    ));
}
#[test]
fn damage_is_reused_only_for_the_same_source_and_output_geometry() {
    let key = Some(((2560, 1440), (1920, 1080)));
    assert!(cpu_damage_reusable(key, (2560, 1440), (1920, 1080)));
    assert!(!cpu_damage_reusable(key, (1920, 1080), (1920, 1080)));
    assert!(!cpu_damage_reusable(None, (2560, 1440), (1920, 1080)));
}
#[test]
fn nv12_length_matches_packed_planes() {
    assert_eq!(nv12_len(640, 480), 640 * 480 * 3 / 2);
    assert_eq!(nv12_len(2, 2), 6);
}
