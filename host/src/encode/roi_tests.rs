use super::{
    apply_initialization_options, evaluate_roi_map, resource_registration_action, EncoderOptions,
    FrameDamage, Preset, ResourceMode, ResourceRegistrationAction, RoiMapReason, RoiMode,
    VideoCodec,
};
use crate::frame_damage::{DamageRect, HevcQpDeltaMap};
use nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_INITIALIZE_PARAMS;

#[test]
fn refine_mode_applies_only_a_valid_prepared_map_and_never_on_keyframes() {
    let mut options = EncoderOptions::production(Preset::P3);
    options.roi_mode = RoiMode::Refine;
    assert_eq!(
        super::qp_map_mode(options, VideoCodec::Hevc),
        super::NV_ENC_QP_MAP_MODE::NV_ENC_QP_MAP_DELTA
    );
    let mut init = NV_ENC_INITIALIZE_PARAMS::default();
    apply_initialization_options(&mut init, options);
    assert_eq!(init.enableReconFrameOutput(), 0);
    let full = DamageRect {
        left: 0,
        top: 0,
        right: 128,
        bottom: 64,
    };
    let map = crate::frame_damage::HevcQpDeltaMap {
        columns: 4,
        rows: 2,
        values: vec![-6, 2, 2, 2, 2, 2, 2, 0],
    };
    let damage = |map| FrameDamage {
        source_width: 128,
        source_height: 64,
        crop: full,
        rects: &[],
        edge_scores: None,
        prepared_qp_map: map,
    };
    let applied = evaluate_roi_map(
        options,
        VideoCodec::Hevc,
        false,
        128,
        64,
        Some(damage(Some(&map))),
    );
    assert_eq!(applied.map.as_ref(), Some(&map));
    assert!(applied.telemetry.is_none());
    for (codec, idr) in [(VideoCodec::Hevc, true), (VideoCodec::H264, true)] {
        let skipped = evaluate_roi_map(options, codec, idr, 128, 64, Some(damage(Some(&map))));
        assert!(skipped.map.is_none() && skipped.telemetry.is_none());
    }
    assert!(evaluate_roi_map(
        options,
        VideoCodec::Hevc,
        false,
        128,
        64,
        Some(damage(None))
    )
    .map
    .is_none());
    assert!(
        evaluate_roi_map(options, VideoCodec::Hevc, false, 128, 64, None)
            .map
            .is_none()
    );
    let wrong_size = evaluate_roi_map(
        options,
        VideoCodec::Hevc,
        false,
        160,
        64,
        Some(damage(Some(&map))),
    );
    assert!(wrong_size.map.is_none());
    let mut out_of_range = map.clone();
    out_of_range.values[0] = -20;
    assert!(evaluate_roi_map(
        options,
        VideoCodec::Hevc,
        false,
        128,
        64,
        Some(damage(Some(&out_of_range)))
    )
    .map
    .is_none());
}

#[test]
fn damage_roi_reports_each_activity_gate_without_touching_nvenc() {
    let mut options = EncoderOptions::production(Preset::P3);
    options.roi_mode = RoiMode::Damage;
    let full = DamageRect {
        left: 0,
        top: 0,
        right: 128,
        bottom: 64,
    };
    let localized_rects = [DamageRect {
        left: 32,
        top: 0,
        right: 64,
        bottom: 32,
    }];
    let localized = FrameDamage {
        source_width: 128,
        source_height: 64,
        crop: full,
        rects: &localized_rects,
        edge_scores: None,
        prepared_qp_map: None,
    };
    assert_eq!(
        evaluate_roi_map(options, VideoCodec::Hevc, true, 128, 64, Some(localized))
            .telemetry
            .unwrap()
            .reason,
        RoiMapReason::Idr
    );
    assert_eq!(
        evaluate_roi_map(options, VideoCodec::Hevc, false, 128, 64, None)
            .telemetry
            .unwrap()
            .reason,
        RoiMapReason::MissingDamage
    );
    let applied = evaluate_roi_map(options, VideoCodec::Hevc, false, 128, 64, Some(localized));
    assert!(applied.map.is_some());
    assert_eq!(applied.telemetry.unwrap().reason, RoiMapReason::Applied);

    let full_rects = [full];
    let full_damage = FrameDamage {
        source_width: 128,
        source_height: 64,
        crop: full,
        rects: &full_rects,
        edge_scores: None,
        prepared_qp_map: None,
    };
    assert_eq!(
        evaluate_roi_map(options, VideoCodec::Hevc, false, 128, 64, Some(full_damage))
            .telemetry
            .unwrap()
            .reason,
        RoiMapReason::CoverageAtLeastHalf
    );
}

#[test]
fn text_edge_roi_requires_features_and_reports_the_weighted_delta_range() {
    let mut options = EncoderOptions::production(Preset::P3);
    options.roi_mode = RoiMode::TextEdge;
    let full = DamageRect {
        left: 0,
        top: 0,
        right: 128,
        bottom: 64,
    };
    let changed_rects = [
        DamageRect {
            left: 0,
            top: 0,
            right: 32,
            bottom: 32,
        },
        DamageRect {
            left: 32,
            top: 0,
            right: 64,
            bottom: 32,
        },
    ];
    let missing_features = FrameDamage {
        source_width: 128,
        source_height: 64,
        crop: full,
        rects: &changed_rects,
        edge_scores: None,
        prepared_qp_map: None,
    };
    assert_eq!(
        evaluate_roi_map(
            options,
            VideoCodec::Hevc,
            false,
            128,
            64,
            Some(missing_features),
        )
        .telemetry
        .unwrap()
        .reason,
        RoiMapReason::UnsupportedEncoderState
    );

    let edge_scores = [10, 90, 0, 0, 0, 0, 0, 0];
    let with_features = FrameDamage {
        edge_scores: Some(&edge_scores),
        ..missing_features
    };
    let applied = evaluate_roi_map(
        options,
        VideoCodec::Hevc,
        false,
        128,
        64,
        Some(with_features),
    );
    assert_eq!(applied.map.unwrap().values, vec![-2, -4, 1, 1, 1, 1, 1, 1]);
    let telemetry = applied.telemetry.unwrap();
    assert_eq!(telemetry.reason, RoiMapReason::Applied);
    assert_eq!(
        (telemetry.min_delta, telemetry.max_delta),
        (Some(-4), Some(1))
    );
}

#[test]
fn feedback_roi_submits_the_exact_prepared_map() {
    let mut options = EncoderOptions::production(Preset::P3);
    options.roi_mode = RoiMode::Feedback;
    let full = DamageRect {
        left: 0,
        top: 0,
        right: 128,
        bottom: 64,
    };
    let map = HevcQpDeltaMap {
        columns: 4,
        rows: 2,
        values: vec![-2, 1, 1, 0, 0, 0, 0, 0],
    };
    let frame = FrameDamage {
        source_width: 128,
        source_height: 64,
        crop: full,
        rects: &[],
        edge_scores: None,
        prepared_qp_map: Some(&map),
    };

    let evaluation = evaluate_roi_map(options, VideoCodec::Hevc, false, 128, 64, Some(frame));
    assert_eq!(evaluation.map.unwrap(), map);
    let telemetry = evaluation.telemetry.unwrap();
    assert_eq!(telemetry.reason, RoiMapReason::Applied);
    assert_eq!((telemetry.changed_ctbs, telemetry.total_ctbs), (1, 8));
    assert_eq!(
        (telemetry.min_delta, telemetry.max_delta),
        (Some(-2), Some(1))
    );
}

#[test]
fn persistent_resources_reuse_only_the_exact_registered_texture() {
    let first = std::ptr::dangling_mut::<std::ffi::c_void>();
    let second = 2usize as *mut std::ffi::c_void;
    assert_eq!(
        resource_registration_action(ResourceMode::PerFrame, Some(first), first),
        ResourceRegistrationAction::PerFrame
    );
    assert_eq!(
        resource_registration_action(ResourceMode::Persistent, None, first),
        ResourceRegistrationAction::RegisterPersistent
    );
    assert_eq!(
        resource_registration_action(ResourceMode::Persistent, Some(first), first),
        ResourceRegistrationAction::ReusePersistent
    );
    assert_eq!(
        resource_registration_action(ResourceMode::Persistent, Some(first), second),
        ResourceRegistrationAction::ReplacePersistent
    );
}
