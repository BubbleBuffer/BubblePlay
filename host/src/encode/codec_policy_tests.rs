use super::{
    annex_b_is_random_access, apply_initialization_options, evaluate_roi_map, AqMode,
    EncoderOptions, FrameDamage, Multipass, Preset, RateControlMode, ResourceMode, RoiMode, Tuning,
    VideoCodec,
};
use crate::frame_damage::{DamageRect, HevcQpDeltaMap};
use nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_INITIALIZE_PARAMS;

#[test]
fn fresh_sessions_prefer_hevc_and_keep_h264_as_fallback() {
    assert_eq!(VideoCodec::preferred(), VideoCodec::Hevc);
    assert_eq!(VideoCodec::Hevc.name(), "hevc");
    assert_eq!(VideoCodec::H264.name(), "h264");
    assert_eq!(VideoCodec::Hevc.fallback(), Some(VideoCodec::H264));
    assert_eq!(VideoCodec::H264.fallback(), None);
}

#[test]
fn annex_b_random_access_detection_uses_codec_nal_types() {
    assert!(annex_b_is_random_access(VideoCodec::H264, &[0, 0, 1, 5, 0]));
    assert!(!annex_b_is_random_access(
        VideoCodec::H264,
        &[0, 0, 1, 1, 0]
    ));
    assert!(annex_b_is_random_access(
        VideoCodec::Hevc,
        &[0, 0, 0, 1, 38, 0]
    ));
    assert!(!annex_b_is_random_access(
        VideoCodec::Hevc,
        &[0, 0, 0, 1, 2, 0]
    ));
}

#[test]
fn v4_infinite_gop_disables_periodic_idr_for_both_codecs() {
    assert_eq!(super::gop_frames(30, 2, false), 60);
    assert_eq!(super::gop_frames(60, 4, false), 240);
    assert_eq!(
        super::gop_frames(30, 2, true),
        super::NVENC_INFINITE_GOPLENGTH
    );
    for codec in [VideoCodec::Hevc, VideoCodec::H264] {
        let mut cfg: super::NV_ENC_CONFIG = unsafe { super::zeroed() };
        super::apply_gop(&mut cfg, codec, super::gop_frames(30, 2, true));
        assert_eq!(cfg.gopLength, super::NVENC_INFINITE_GOPLENGTH);
        let idr = unsafe {
            match codec {
                VideoCodec::Hevc => cfg.encodeCodecConfig.hevcConfig.idrPeriod,
                VideoCodec::H264 => cfg.encodeCodecConfig.h264Config.idrPeriod,
            }
        };
        assert_eq!(idr, super::NVENC_INFINITE_GOPLENGTH);
        super::apply_gop(&mut cfg, codec, super::gop_frames(30, 2, false));
        assert_eq!(cfg.gopLength, 60);
    }
}

#[test]
fn production_encoder_options_remain_the_existing_low_latency_defaults() {
    assert_eq!(
        EncoderOptions::production(Preset::P3),
        EncoderOptions {
            preset: Preset::P3,
            tuning: Tuning::UltraLowLatency,
            rate_control: RateControlMode::Cbr,
            multipass: Multipass::Disabled,
            aq_mode: AqMode::Spatial,
            aq_strength: 8,
            vbv_frames: 1,
            lookahead_depth: 0,
            weighted_prediction: false,
            gop_seconds: 2,
            roi_mode: RoiMode::Off,
            resource_mode: ResourceMode::PerFrame,
            async_depth: 0,
            main10: false,
        }
    );
}

#[test]
fn feedback_mode_requests_reconstructed_frame_output_without_changing_other_modes() {
    let mut feedback = EncoderOptions::production(Preset::P3);
    feedback.roi_mode = RoiMode::Feedback;
    let mut feedback_init = NV_ENC_INITIALIZE_PARAMS::default();
    apply_initialization_options(&mut feedback_init, feedback);
    assert_eq!(feedback_init.enableReconFrameOutput(), 1);

    for roi_mode in [RoiMode::Off, RoiMode::Damage, RoiMode::TextEdge] {
        let mut options = EncoderOptions::production(Preset::P3);
        options.roi_mode = roi_mode;
        let mut init = NV_ENC_INITIALIZE_PARAMS::default();
        apply_initialization_options(&mut init, options);
        assert_eq!(init.enableReconFrameOutput(), 0, "mode {roi_mode:?}");
    }
}

#[test]
fn expanded_controls_map_exactly_to_nvenc_configuration() {
    let options = EncoderOptions {
        preset: Preset::P7,
        tuning: Tuning::LowLatency,
        rate_control: RateControlMode::Cbr,
        multipass: Multipass::Full,
        aq_mode: AqMode::Temporal,
        aq_strength: 15,
        vbv_frames: 4,
        lookahead_depth: 2,
        weighted_prediction: true,
        gop_seconds: 4,
        roi_mode: RoiMode::Damage,
        resource_mode: ResourceMode::Persistent,
        async_depth: 1,
        main10: true,
    };
    let mut cfg = unsafe { super::zeroed() };
    super::apply_encoder_options(&mut cfg, 12_000_000, 30, options);
    assert_eq!(cfg.gopLength, 120);
    assert_eq!(cfg.rcParams.enableAQ(), 0);
    assert_eq!(cfg.rcParams.enableTemporalAQ(), 1);
    assert_eq!(cfg.rcParams.enableLookahead(), 1);
    assert_eq!(cfg.rcParams.lookaheadDepth, 2);
    assert_eq!(cfg.rcParams.aqStrength(), 15);
    assert_eq!(
        cfg.rcParams.multiPass,
        super::NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_FULL_RESOLUTION
    );
    assert_eq!(cfg.rcParams.vbvBufferSize, 1_600_000);

    let mut init = unsafe { super::zeroed() };
    super::apply_initialization_options(&mut init, options);
    assert_eq!(init.enableWeightedPrediction(), 1);
    assert_eq!(
        super::qp_map_mode(options, VideoCodec::Hevc),
        super::NV_ENC_QP_MAP_MODE::NV_ENC_QP_MAP_DELTA
    );
    assert_eq!(
        super::qp_map_mode(options, VideoCodec::H264),
        super::NV_ENC_QP_MAP_MODE::NV_ENC_QP_MAP_DELTA
    );
    let mut off = options;
    off.roi_mode = RoiMode::Off;
    for codec in [VideoCodec::Hevc, VideoCodec::H264] {
        assert_eq!(
            super::qp_map_mode(off, codec),
            super::NV_ENC_QP_MAP_MODE::NV_ENC_QP_MAP_DISABLED
        );
    }
    assert_eq!(
        super::input_buffer_format(options),
        super::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT
    );
}

#[test]
fn external_budget_vbr_keeps_the_average_target_and_bounds_only_the_peak() {
    let mut options = EncoderOptions::production(Preset::P3);
    options.rate_control = RateControlMode::ExternalBudgetVbr {
        target_quality: 28,
        peak_bitrate_multiplier: 4,
    };
    options.vbv_frames = 4;
    let mut cfg = unsafe { super::zeroed() };
    super::apply_encoder_options(&mut cfg, 6_000_000, 30, options);
    assert_eq!(
        cfg.rcParams.rateControlMode,
        super::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR
    );
    assert_eq!(cfg.rcParams.averageBitRate, 6_000_000);
    assert_eq!(cfg.rcParams.maxBitRate, 24_000_000);
    assert_eq!(cfg.rcParams.targetQuality, 28);
    assert_eq!(cfg.rcParams.targetQualityLSB, 0);
    assert_eq!(cfg.rcParams.vbvBufferSize, 800_000);
}

#[test]
fn h264_refine_and_damage_maps_use_the_macroblock_grid() {
    let mut options = EncoderOptions::production(Preset::P3);
    options.roi_mode = RoiMode::Refine;
    assert_eq!(super::qp_map_block(VideoCodec::H264), 16);
    assert_eq!(super::qp_map_block(VideoCodec::Hevc), 32);
    let full = DamageRect {
        left: 0,
        top: 0,
        right: 100,
        bottom: 50,
    };
    let damage = |map| FrameDamage {
        source_width: 100,
        source_height: 50,
        crop: full,
        rects: &[],
        edge_scores: None,
        prepared_qp_map: map,
    };
    // 100x50 is not a multiple of 16: 7x4 macroblocks, 4x2 CTBs.
    let mb_map = crate::frame_damage::HevcQpDeltaMap {
        columns: 7,
        rows: 4,
        values: vec![-6; 28],
    };
    let applied = evaluate_roi_map(
        options,
        VideoCodec::H264,
        false,
        100,
        50,
        Some(damage(Some(&mb_map))),
    );
    assert_eq!(applied.map.as_ref(), Some(&mb_map));
    // A CTB-grid map is wrong for H.264 and is dropped.
    let ctb_map = crate::frame_damage::HevcQpDeltaMap {
        columns: 4,
        rows: 2,
        values: vec![-6; 8],
    };
    let wrong = evaluate_roi_map(
        options,
        VideoCodec::H264,
        false,
        100,
        50,
        Some(damage(Some(&ctb_map))),
    );
    assert!(wrong.map.is_none());

    // Damage mode: the CTB analysis is expanded to macroblocks.
    options.roi_mode = RoiMode::Damage;
    let rects = [DamageRect {
        left: 0,
        top: 0,
        right: 10,
        bottom: 10,
    }];
    let frame = FrameDamage {
        rects: &rects,
        ..damage(None)
    };
    let evaluation = evaluate_roi_map(options, VideoCodec::H264, false, 100, 50, Some(frame));
    let map = evaluation.map.expect("damage map");
    assert_eq!((map.columns, map.rows), (7, 4));
    assert_eq!(map.values.len(), 28);
    // Only the first CTB changed: macroblocks (0..2, 0..2) are lowered.
    let lowered: Vec<usize> = (0..28).filter(|i| map.values[*i] < 0).collect();
    assert_eq!(lowered, vec![0, 1, 7, 8]);
}
