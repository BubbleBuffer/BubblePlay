//! NVENC HEVC/H.264 encoder on D3D11 textures via the raw `nvidia-video-codec-sdk`
//! sys bindings (`ENCODE_API` function-pointer table).
//!
//! Zero-copy path: register the captured desktop texture (same D3D11 device)
//! -> map -> encode_picture -> lock_bitstream -> memcpy Annex B out.
//! The input never leaves VRAM; only the bitstream is copied to host memory.
//!
//! SDK version audit (2026-08-07, driver 591.86 = SDK 13.0):
//! The `nvidia-video-codec-sdk` crate ships SDK 12.1 headers, and that is
//! fine: a C probe compiled against the genuine 12.1 header runs the full
//! session->preset->init->register->map->encode->lock cycle successfully on
//! this driver (30 frames, H.264 High 1080p, ffprobe-verified). The driver
//! accepts the old version words. Two call-site details matter:
//!   1. `NV_ENC_PRESET_CONFIG::presetCfg.version` must be set to
//!      `NV_ENC_CONFIG_VER` before `GetEncodePresetConfigEx` (else 15).
//!   2. `NV_ENC_MAP_INPUT_RESOURCE::inputResource` is *deprecated* ("Do not
//!      use" in the SDK header); the registered handle goes in
//!      `registeredResource` (else generic error 20 at Map).
//!
//! Production defaults remain HEVC Main, CBR, P3 + ultra-low-latency,
//! one-frame VBV, spatial AQ, no lookahead, and a two-second GOP. Expiring
//! Stream Lab v2 leases may select the bounded experimental controls below.

use std::ffi::{c_void, CStr};
use std::ptr;

use anyhow::{bail, ensure, Context, Result};
use nvidia_video_codec_sdk::safe::ENCODE_API;
use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    GUID, NVENCAPI_VERSION, NV_ENC_BUFFER_FORMAT, NV_ENC_BUFFER_USAGE, NV_ENC_CAPS,
    NV_ENC_CAPS_PARAM, NV_ENC_CAPS_PARAM_VER, NV_ENC_CONFIG, NV_ENC_CONFIG_VER,
    NV_ENC_CREATE_BITSTREAM_BUFFER, NV_ENC_CREATE_BITSTREAM_BUFFER_VER, NV_ENC_DEVICE_TYPE,
    NV_ENC_INITIALIZE_PARAMS, NV_ENC_INITIALIZE_PARAMS_VER, NV_ENC_INPUT_RESOURCE_TYPE,
    NV_ENC_LOCK_BITSTREAM, NV_ENC_LOCK_BITSTREAM_VER, NV_ENC_MAP_INPUT_RESOURCE,
    NV_ENC_MAP_INPUT_RESOURCE_VER, NV_ENC_MULTI_PASS, NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER, NV_ENC_PARAMS_RC_MODE, NV_ENC_PIC_FLAGS,
    NV_ENC_PIC_PARAMS, NV_ENC_PIC_PARAMS_VER, NV_ENC_PIC_STRUCT, NV_ENC_PIC_TYPE,
    NV_ENC_PRESET_CONFIG, NV_ENC_PRESET_CONFIG_VER, NV_ENC_QP_MAP_MODE, NV_ENC_RECONFIGURE_PARAMS,
    NV_ENC_RECONFIGURE_PARAMS_VER, NV_ENC_REGISTER_RESOURCE, NV_ENC_REGISTER_RESOURCE_VER,
    NV_ENC_SEQUENCE_PARAM_PAYLOAD, NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER, NV_ENC_TUNING_INFO,
};
use tracing::{debug, info, warn};

use crate::frame_damage::{
    analyze_hevc_qp_delta_map, analyze_hevc_text_edge_qp_delta_map, DamageRect, HevcQpDeltaMap,
    HevcQpMapDecision, QpMapRejectReason,
};

#[cfg(test)]
mod codec_policy_tests {
    use super::{
        annex_b_is_random_access, apply_initialization_options, evaluate_roi_map,
        resource_registration_action, AqMode, EncoderOptions, FrameDamage, Multipass, Preset,
        RateControlMode, ResourceMode, ResourceRegistrationAction, RoiMapReason, RoiMode, Tuning,
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
        assert_eq!(super::gop_frames(30, 2, true), super::NVENC_INFINITE_GOPLENGTH);
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
            super::NV_ENC_QP_MAP_MODE::NV_ENC_QP_MAP_DISABLED
        );
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
        let first = 1usize as *mut std::ffi::c_void;
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
}

// The crate keeps its GUID table in a private `sys::guid` module; the values
// below are copied verbatim from there (they mirror the C SDK headers).
const NV_ENC_CODEC_H264_GUID: GUID = GUID {
    Data1: 0x6bc8_2762,
    Data2: 0x4e63,
    Data3: 0x4ca4,
    Data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
};
const NV_ENC_CODEC_HEVC_GUID: GUID = GUID {
    Data1: 0x790c_dc88,
    Data2: 0x4522,
    Data3: 0x4d7b,
    Data4: [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
};
const NV_ENC_PRESET_P1_GUID: GUID = GUID {
    Data1: 0xfc0a_8d3e,
    Data2: 0x45f8,
    Data3: 0x4cf8,
    Data4: [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
};
const NV_ENC_PRESET_P2_GUID: GUID = GUID {
    Data1: 0xf581_cfb8,
    Data2: 0x88d6,
    Data3: 0x4381,
    Data4: [0x93, 0xf0, 0xdf, 0x13, 0xf9, 0xc2, 0x7d, 0xab],
};
const NV_ENC_PRESET_P3_GUID: GUID = GUID {
    Data1: 0x3685_0110,
    Data2: 0x3a07,
    Data3: 0x441f,
    Data4: [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6],
};
const NV_ENC_PRESET_P4_GUID: GUID = GUID {
    Data1: 0x90a7_b826,
    Data2: 0xdf06,
    Data3: 0x4862,
    Data4: [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
};
const NV_ENC_PRESET_P5_GUID: GUID = GUID {
    Data1: 0x21c6_e6b4,
    Data2: 0x297a,
    Data3: 0x4cba,
    Data4: [0x99, 0x8f, 0xb6, 0xcb, 0xde, 0x72, 0xad, 0xe3],
};
const NV_ENC_PRESET_P6_GUID: GUID = GUID {
    Data1: 0x8e75_c279,
    Data2: 0x6299,
    Data3: 0x4ab6,
    Data4: [0x83, 0x02, 0x0b, 0x21, 0x5a, 0x33, 0x5c, 0xf5],
};
const NV_ENC_PRESET_P7_GUID: GUID = GUID {
    Data1: 0x8484_8c12,
    Data2: 0x6f71,
    Data3: 0x4c13,
    Data4: [0x93, 0x1b, 0x53, 0xe2, 0x83, 0xf5, 0x79, 0x74],
};
const NV_ENC_H264_PROFILE_HIGH_GUID: GUID = GUID {
    Data1: 0xe7cb_c309,
    Data2: 0x4f7a,
    Data3: 0x4b89,
    Data4: [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
};
const NV_ENC_HEVC_PROFILE_MAIN_GUID: GUID = GUID {
    Data1: 0xb514_c39a,
    Data2: 0xb55b,
    Data3: 0x40fa,
    Data4: [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec],
};
const NV_ENC_HEVC_PROFILE_MAIN10_GUID: GUID = GUID {
    Data1: 0xfa4d_2b6c,
    Data2: 0x3a5b,
    Data3: 0x411a,
    Data4: [0x80, 0x18, 0x0a, 0x3f, 0x5e, 0x3c, 0x9b, 0xe5],
};

/// Codec policy for one browser session. A fresh session always starts with
/// HEVC; H.264 is only a one-way compatibility fallback for that session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    Hevc,
    H264,
}

impl VideoCodec {
    pub const fn preferred() -> Self {
        Self::Hevc
    }
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hevc => "hevc",
            Self::H264 => "h264",
        }
    }
    pub const fn fallback(self) -> Option<Self> {
        match self {
            Self::Hevc => Some(Self::H264),
            Self::H264 => None,
        }
    }
    fn codec_guid(self) -> GUID {
        match self {
            Self::Hevc => NV_ENC_CODEC_HEVC_GUID,
            Self::H264 => NV_ENC_CODEC_H264_GUID,
        }
    }
    fn profile_guid(self, main10: bool) -> GUID {
        match self {
            Self::Hevc if main10 => NV_ENC_HEVC_PROFILE_MAIN10_GUID,
            Self::Hevc => NV_ENC_HEVC_PROFILE_MAIN_GUID,
            Self::H264 => NV_ENC_H264_PROFILE_HIGH_GUID,
        }
    }
}

/// Detect a random-access access unit from the Annex-B payload. NVENC's
/// lock metadata is diagnostic; the wire flag must describe the bytes the
/// browser will actually decode.
pub fn annex_b_is_random_access(codec: VideoCodec, bytes: &[u8]) -> bool {
    let mut index = 0usize;
    while index + 3 < bytes.len() {
        let (prefix, nal_start) = if bytes[index..].starts_with(&[0, 0, 0, 1]) {
            (4usize, index + 4)
        } else if bytes[index..].starts_with(&[0, 0, 1]) {
            (3usize, index + 3)
        } else {
            index += 1;
            continue;
        };
        if nal_start >= bytes.len() {
            break;
        }
        let random_access = match codec {
            VideoCodec::H264 => bytes[nal_start] & 0x1f == 5,
            VideoCodec::Hevc => {
                let nal_type = (bytes[nal_start] >> 1) & 0x3f;
                (16..=21).contains(&nal_type)
            }
        };
        if random_access {
            return true;
        }
        index = nal_start.max(index + prefix);
    }
    false
}

/// Zero-initialize a repr(C) NVENC params struct. This mirrors the C SDK usage
/// pattern (`memset(&p, 0, sizeof(p)); p.version = ...;`): every field we do
/// not set explicitly must be zero for the API.
#[inline]
unsafe fn zeroed<T>() -> T {
    std::mem::MaybeUninit::zeroed().assume_init()
}

pub struct EncodedFrame {
    /// Annex B access unit (SPS/PPS included on IDR when repeatSPSPPS is on).
    pub data: Vec<u8>,
    pub is_idr: bool,
    /// Encoder-side latency: register+map+encode+lock combined, in microseconds.
    pub encode_us: u64,
    /// Present only when a per-CTB HEVC delta-QP map was submitted for this picture.
    pub qp_map: Option<QpMapStats>,
    /// Present for every frame evaluated by an active ROI experiment.
    pub roi_activity: Option<RoiMapTelemetry>,
    /// Synchronous NVENC stage timings for default-off resource experiments.
    pub stage_timings: EncodeStageTimings,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncodeStageTimings {
    pub register_calls: u32,
    pub register_us: u64,
    pub map_us: u64,
    pub submit_us: u64,
    pub lock_us: u64,
    pub copy_us: u64,
    pub unmap_us: u64,
    pub unregister_calls: u32,
    pub unregister_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QpMapStats {
    pub changed_ctbs: u32,
    pub total_ctbs: u32,
    pub min_delta: i8,
    pub max_delta: i8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoiMapReason {
    Applied,
    MissingDamage,
    EmptyDamage,
    Idr,
    CoverageAtLeastHalf,
    UnsupportedEncoderState,
}

impl RoiMapReason {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::MissingDamage => "missing-damage",
            Self::EmptyDamage => "empty-damage",
            Self::Idr => "idr",
            Self::CoverageAtLeastHalf => "coverage-at-least-half",
            Self::UnsupportedEncoderState => "unsupported-encoder-state",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoiMapTelemetry {
    pub reason: RoiMapReason,
    pub changed_ctbs: u32,
    pub total_ctbs: u32,
    pub min_delta: Option<i8>,
    pub max_delta: Option<i8>,
}

struct RoiMapEvaluation {
    map: Option<HevcQpDeltaMap>,
    telemetry: Option<RoiMapTelemetry>,
}

#[derive(Debug, Clone, Copy)]
pub struct FrameDamage<'a> {
    pub source_width: u32,
    pub source_height: u32,
    pub crop: DamageRect,
    pub rects: &'a [DamageRect],
    pub edge_scores: Option<&'a [u32]>,
    pub prepared_qp_map: Option<&'a HevcQpDeltaMap>,
}

fn evaluate_roi_map(
    options: EncoderOptions,
    codec: VideoCodec,
    force_idr: bool,
    output_width: u32,
    output_height: u32,
    damage: Option<FrameDamage<'_>>,
) -> RoiMapEvaluation {
    let rejected = |reason, changed_ctbs, total_ctbs| RoiMapEvaluation {
        map: None,
        telemetry: Some(RoiMapTelemetry {
            reason,
            changed_ctbs,
            total_ctbs,
            min_delta: None,
            max_delta: None,
        }),
    };
    if options.roi_mode == RoiMode::Off {
        return RoiMapEvaluation {
            map: None,
            telemetry: None,
        };
    }
    if codec != VideoCodec::Hevc {
        return rejected(RoiMapReason::UnsupportedEncoderState, 0, 0);
    }
    if force_idr {
        return rejected(RoiMapReason::Idr, 0, 0);
    }
    let Some(damage) = damage else {
        return rejected(RoiMapReason::MissingDamage, 0, 0);
    };
    let decision = match options.roi_mode {
        RoiMode::Damage => analyze_hevc_qp_delta_map(
            output_width,
            output_height,
            damage.source_width,
            damage.source_height,
            damage.crop,
            damage.rects,
        ),
        RoiMode::TextEdge => {
            let Some(edge_scores) = damage.edge_scores else {
                return rejected(RoiMapReason::UnsupportedEncoderState, 0, 0);
            };
            analyze_hevc_text_edge_qp_delta_map(
                output_width,
                output_height,
                damage.source_width,
                damage.source_height,
                damage.crop,
                damage.rects,
                edge_scores,
            )
        }
        RoiMode::Feedback => {
            let Some(map) = damage.prepared_qp_map else {
                return rejected(RoiMapReason::MissingDamage, 0, 0);
            };
            let expected_columns = output_width.div_ceil(32);
            let expected_rows = output_height.div_ceil(32);
            if map.columns != expected_columns
                || map.rows != expected_rows
                || map.values.len() != (expected_columns * expected_rows) as usize
                || map.values.iter().any(|delta| !(-5..=2).contains(delta))
            {
                return rejected(
                    RoiMapReason::UnsupportedEncoderState,
                    0,
                    expected_columns * expected_rows,
                );
            }
            HevcQpMapDecision::Applied(map.clone())
        }
        RoiMode::Off => {
            return rejected(RoiMapReason::UnsupportedEncoderState, 0, 0);
        }
    };
    match decision {
        HevcQpMapDecision::Applied(map) => {
            let changed_ctbs = map.values.iter().filter(|value| **value < 0).count() as u32;
            let total_ctbs = map.values.len() as u32;
            let min_delta = map.values.iter().copied().min();
            let max_delta = map.values.iter().copied().max();
            RoiMapEvaluation {
                map: Some(map),
                telemetry: Some(RoiMapTelemetry {
                    reason: RoiMapReason::Applied,
                    changed_ctbs,
                    total_ctbs,
                    min_delta,
                    max_delta,
                }),
            }
        }
        HevcQpMapDecision::Rejected {
            reason,
            changed_ctbs,
            total_ctbs,
        } => rejected(
            match reason {
                QpMapRejectReason::InvalidGeometry => RoiMapReason::UnsupportedEncoderState,
                QpMapRejectReason::EmptyDamage => RoiMapReason::EmptyDamage,
                QpMapRejectReason::CoverageAtLeastHalf => RoiMapReason::CoverageAtLeastHalf,
            },
            changed_ctbs,
            total_ctbs,
        ),
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    P1,
    P2,
    P3,
    P4,
    P5,
    P6,
    P7,
}
impl Preset {
    fn guid(self) -> GUID {
        match self {
            Self::P1 => NV_ENC_PRESET_P1_GUID,
            Self::P2 => NV_ENC_PRESET_P2_GUID,
            Self::P3 => NV_ENC_PRESET_P3_GUID,
            Self::P4 => NV_ENC_PRESET_P4_GUID,
            Self::P5 => NV_ENC_PRESET_P5_GUID,
            Self::P6 => NV_ENC_PRESET_P6_GUID,
            Self::P7 => NV_ENC_PRESET_P7_GUID,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tuning {
    UltraLowLatency,
    LowLatency,
}

impl Tuning {
    fn native(self) -> NV_ENC_TUNING_INFO {
        match self {
            Self::UltraLowLatency => NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            Self::LowLatency => NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_LOW_LATENCY,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Multipass {
    Disabled,
    Quarter,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AqMode {
    None,
    Spatial,
    Temporal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoiMode {
    Off,
    Damage,
    TextEdge,
    Feedback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceMode {
    PerFrame,
    Persistent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControlMode {
    Cbr,
    /// NVENC targets the bounded average media rate while allowing a larger
    /// instantaneous ceiling. The WSS wire cap is verified separately so
    /// parity and framing overhead remain visible to the experiment.
    ExternalBudgetVbr {
        target_quality: u8,
        peak_bitrate_multiplier: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceRegistrationAction {
    PerFrame,
    RegisterPersistent,
    ReusePersistent,
    ReplacePersistent,
}

fn resource_registration_action(
    mode: ResourceMode,
    persistent_texture: Option<*mut c_void>,
    texture: *mut c_void,
) -> ResourceRegistrationAction {
    match (mode, persistent_texture) {
        (ResourceMode::PerFrame, _) => ResourceRegistrationAction::PerFrame,
        (ResourceMode::Persistent, None) => ResourceRegistrationAction::RegisterPersistent,
        (ResourceMode::Persistent, Some(current)) if current == texture => {
            ResourceRegistrationAction::ReusePersistent
        }
        (ResourceMode::Persistent, Some(_)) => ResourceRegistrationAction::ReplacePersistent,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderOptions {
    pub preset: Preset,
    pub tuning: Tuning,
    pub rate_control: RateControlMode,
    pub multipass: Multipass,
    pub aq_mode: AqMode,
    pub aq_strength: u8,
    pub vbv_frames: u8,
    pub lookahead_depth: u8,
    pub weighted_prediction: bool,
    pub gop_seconds: u8,
    pub roi_mode: RoiMode,
    pub resource_mode: ResourceMode,
    pub async_depth: u8,
    pub main10: bool,
}

impl EncoderOptions {
    pub const fn production(preset: Preset) -> Self {
        Self {
            preset,
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
    }
}

fn input_buffer_format(options: EncoderOptions) -> NV_ENC_BUFFER_FORMAT {
    if options.main10 {
        NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT
    } else {
        NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB
    }
}

fn qp_map_mode(options: EncoderOptions, codec: VideoCodec) -> NV_ENC_QP_MAP_MODE {
    if options.roi_mode != RoiMode::Off && codec == VideoCodec::Hevc {
        NV_ENC_QP_MAP_MODE::NV_ENC_QP_MAP_DELTA
    } else {
        NV_ENC_QP_MAP_MODE::NV_ENC_QP_MAP_DISABLED
    }
}

fn encoder_rate_control_bitrates(bitrate_bps: u32, mode: RateControlMode) -> (u32, u32) {
    match mode {
        RateControlMode::Cbr => (bitrate_bps, bitrate_bps),
        RateControlMode::ExternalBudgetVbr {
            peak_bitrate_multiplier,
            ..
        } => (
            bitrate_bps,
            bitrate_bps.saturating_mul(u32::from(peak_bitrate_multiplier)),
        ),
    }
}

fn rate_control_label(mode: RateControlMode) -> &'static str {
    match mode {
        RateControlMode::Cbr => "cbr",
        RateControlMode::ExternalBudgetVbr { .. } => "external-budget-vbr",
    }
}

/// NVENC's "no periodic IDR" GOP length (`NVENC_INFINITE_GOPLENGTH`).
pub const NVENC_INFINITE_GOPLENGTH: u32 = 0xffff_ffff;

/// GOP / IDR period in frames. WSS v4 owns keyframes itself (start, request,
/// 30 s safety), so its encoder runs with an infinite GOP.
fn gop_frames(fps: u32, gop_seconds: u8, infinite: bool) -> u32 {
    if infinite {
        NVENC_INFINITE_GOPLENGTH
    } else {
        fps.saturating_mul(u32::from(gop_seconds))
    }
}

/// Apply the GOP length to the common config and the codec IDR period.
fn apply_gop(cfg: &mut NV_ENC_CONFIG, codec: VideoCodec, frames: u32) {
    cfg.gopLength = frames;
    // Writing a union field is safe; the member matches the session codec.
    match codec {
        VideoCodec::Hevc => cfg.encodeCodecConfig.hevcConfig.idrPeriod = frames,
        VideoCodec::H264 => cfg.encodeCodecConfig.h264Config.idrPeriod = frames,
    }
}

fn apply_encoder_options(
    cfg: &mut NV_ENC_CONFIG,
    bitrate_bps: u32,
    fps: u32,
    options: EncoderOptions,
) {
    cfg.gopLength = gop_frames(fps, options.gop_seconds, false);
    cfg.frameIntervalP = 1;
    let (average_bitrate_bps, peak_bitrate_bps) =
        encoder_rate_control_bitrates(bitrate_bps, options.rate_control);
    match options.rate_control {
        RateControlMode::Cbr => {
            cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
            cfg.rcParams.targetQuality = 0;
            cfg.rcParams.targetQualityLSB = 0;
        }
        RateControlMode::ExternalBudgetVbr { target_quality, .. } => {
            cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR;
            cfg.rcParams.targetQuality = target_quality;
            cfg.rcParams.targetQualityLSB = 0;
        }
    }
    cfg.rcParams.averageBitRate = average_bitrate_bps;
    cfg.rcParams.maxBitRate = peak_bitrate_bps;
    cfg.rcParams.vbvBufferSize = average_bitrate_bps / fps.max(1) * u32::from(options.vbv_frames);
    cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;
    cfg.rcParams
        .set_enableAQ(u32::from(options.aq_mode == AqMode::Spatial));
    cfg.rcParams.set_aqStrength(u32::from(options.aq_strength));
    cfg.rcParams
        .set_enableTemporalAQ(u32::from(options.aq_mode == AqMode::Temporal));
    cfg.rcParams
        .set_enableLookahead(u32::from(options.lookahead_depth > 0));
    cfg.rcParams.lookaheadDepth = u16::from(options.lookahead_depth);
    cfg.rcParams.multiPass = match options.multipass {
        Multipass::Disabled => NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED,
        Multipass::Quarter => NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION,
        Multipass::Full => NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_FULL_RESOLUTION,
    };
}

fn apply_initialization_options(init: &mut NV_ENC_INITIALIZE_PARAMS, options: EncoderOptions) {
    init.set_enableWeightedPrediction(u32::from(options.weighted_prediction));
    init.set_enableReconFrameOutput(u32::from(options.roi_mode == RoiMode::Feedback));
}

pub struct Nvenc {
    session: *mut c_void,
    device: *mut c_void,
    bitstream: *mut c_void,
    width: u32,
    height: u32,
    fps: u32,
    max_width: u32,
    max_height: u32,
    bitrate_bps: u32,
    codec: VideoCodec,
    options: EncoderOptions,
    frame_idx: u64,
    force_idr_next: bool,
    infinite_gop: bool,
    lab_idr_telemetry: bool,
    last_mapped: Option<*mut c_void>,
    last_recon_mapped: Option<*mut c_void>,
    persistent_input: Option<PersistentInput>,
    persistent_recon_outputs: Vec<PersistentReconOutput>,
    /// Config snapshot kept alive for reconfigure; `init_params.encodeConfig`
    /// points at `cfg`. Both are moved together on reconfigure.
    cfg: Box<NV_ENC_CONFIG>,
    init_params: Box<NV_ENC_INITIALIZE_PARAMS>,
}

struct PersistentInput {
    texture: *mut c_void,
    registered: *mut c_void,
}

struct PersistentReconOutput {
    texture: *mut c_void,
    registered: *mut c_void,
}

// The session is bound to one thread; we never share it across threads.
unsafe impl Send for Nvenc {}

impl Nvenc {
    /// Open an encoder session on the given D3D11 device. For zero-copy
    /// register/map this must be the same device the capture texture lives on.
    pub fn new(
        device: *mut c_void,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self> {
        Self::new_with_preset_native(
            device,
            width,
            height,
            width,
            height,
            fps,
            bitrate_bps,
            Preset::P3,
        )
    }
    pub fn new_with_preset(
        device: *mut c_void,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        preset: Preset,
    ) -> Result<Self> {
        Self::new_with_preset_native(
            device,
            width,
            height,
            width,
            height,
            fps,
            bitrate_bps,
            preset,
        )
    }
    pub fn new_with_preset_native(
        device: *mut c_void,
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
        fps: u32,
        bitrate_bps: u32,
        preset: Preset,
    ) -> Result<Self> {
        Self::new_with_preset_native_codec(
            device,
            width,
            height,
            max_width,
            max_height,
            fps,
            bitrate_bps,
            preset,
            VideoCodec::preferred(),
        )
    }

    pub fn new_with_preset_native_codec(
        device: *mut c_void,
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
        fps: u32,
        bitrate_bps: u32,
        preset: Preset,
        codec: VideoCodec,
    ) -> Result<Self> {
        Self::new_with_options_native_codec(
            device,
            width,
            height,
            max_width,
            max_height,
            fps,
            bitrate_bps,
            EncoderOptions::production(preset),
            codec,
        )
    }

    pub fn new_with_options_native_codec(
        device: *mut c_void,
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
        fps: u32,
        bitrate_bps: u32,
        options: EncoderOptions,
        codec: VideoCodec,
    ) -> Result<Self> {
        ensure!(!device.is_null(), "null D3D11 device");
        ensure!(
            (1..=15).contains(&options.aq_strength),
            "AQ strength must be 1..=15"
        );
        ensure!(
            (1..=4).contains(&options.vbv_frames),
            "VBV frames must be 1..=4"
        );
        ensure!(
            options.lookahead_depth <= 2,
            "lookahead depth must be 0..=2"
        );
        ensure!(
            matches!(options.gop_seconds, 1 | 2 | 4),
            "GOP seconds must be 1, 2, or 4"
        );
        if let RateControlMode::ExternalBudgetVbr {
            target_quality,
            peak_bitrate_multiplier,
        } = options.rate_control
        {
            ensure!(
                (1..=51).contains(&target_quality),
                "VBR target quality must be 1..=51"
            );
            ensure!(
                (1..=4).contains(&peak_bitrate_multiplier),
                "VBR peak bitrate multiplier must be 1..=4"
            );
        }
        ensure!(
            width <= max_width && height <= max_height,
            "active dimensions exceed native maximum"
        );
        ensure!(
            !options.main10 || codec == VideoCodec::Hevc,
            "Main10 requires HEVC"
        );
        ensure!(
            options.roi_mode != RoiMode::Feedback || (codec == VideoCodec::Hevc && !options.main10),
            "reconstruction feedback requires HEVC Main"
        );

        // 1. Open session on the D3D11 device.
        let mut session_params: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = unsafe { zeroed() };
        session_params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        session_params.deviceType = NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX;
        session_params.device = device;
        session_params.apiVersion = NVENCAPI_VERSION;
        let mut session: *mut c_void = ptr::null_mut();
        let st = unsafe { (ENCODE_API.open_encode_session_ex)(&mut session_params, &mut session) };
        st.result_without_string()
            .context("NvEncOpenEncodeSessionEx")?;
        info!("NVENC session opened on capture D3D11 device");

        if options.roi_mode == RoiMode::Feedback {
            let mut caps: NV_ENC_CAPS_PARAM = unsafe { zeroed() };
            caps.version = NV_ENC_CAPS_PARAM_VER;
            caps.capsToQuery = NV_ENC_CAPS::NV_ENC_CAPS_OUTPUT_RECON_SURFACE;
            let mut supported = 0;
            let st = unsafe {
                (ENCODE_API.get_encode_caps)(session, codec.codec_guid(), &mut caps, &mut supported)
            };
            st.result_without_string()
                .context("NvEncGetEncodeCaps(OUTPUT_RECON_SURFACE)")?;
            ensure!(
                supported > 0,
                "NVENC HEVC reconstructed-output surfaces are unsupported"
            );
        }

        // 2. Query the selected preset/tuning pair; unsupported pairs fail
        // closed through the SDK status returned below.
        let mut preset_cfg: NV_ENC_PRESET_CONFIG = unsafe { zeroed() };
        preset_cfg.version = NV_ENC_PRESET_CONFIG_VER;
        // The driver validates the *inner* config version too; leaving it
        // zero yields NV_ENC_ERR_INVALID_VERSION (15) on SDK 13.0 drivers.
        preset_cfg.presetCfg.version = NV_ENC_CONFIG_VER;
        let st = unsafe {
            (ENCODE_API.get_encode_preset_config_ex)(
                session,
                codec.codec_guid(),
                options.preset.guid(),
                options.tuning.native(),
                &mut preset_cfg,
            )
        };
        st.result_without_string()
            .context("GetEncodePresetConfigEx")?;

        let mut cfg = Box::new(preset_cfg.presetCfg);
        cfg.profileGUID = codec.profile_guid(options.main10);
        apply_encoder_options(&mut cfg, bitrate_bps, fps, options);
        cfg.rcParams.qpMapMode = qp_map_mode(options, codec);

        // Codec-specific random-access cadence. HEVC emits VPS/SPS/PPS on
        // every IDR so Annex-B WebCodecs clients can join without hvcC.
        unsafe {
            match codec {
                VideoCodec::Hevc => {
                    let hevc = &mut cfg.encodeCodecConfig.hevcConfig;
                    hevc.idrPeriod = gop_frames(fps, options.gop_seconds, false);
                    hevc.set_repeatSPSPPS(1);
                    let bit_depth_minus_8 = if options.main10 { 2 } else { 0 };
                    hevc.set_pixelBitDepthMinus8(bit_depth_minus_8);
                }
                VideoCodec::H264 => {
                    let h264 = &mut cfg.encodeCodecConfig.h264Config;
                    h264.idrPeriod = gop_frames(fps, options.gop_seconds, false);
                    h264.set_repeatSPSPPS(1);
                }
            }
        }

        // 3. Initialize the encoder.
        let mut init = Box::<NV_ENC_INITIALIZE_PARAMS>::new(unsafe { zeroed() });
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = codec.codec_guid();
        init.presetGUID = options.preset.guid();
        init.encodeWidth = width;
        init.encodeHeight = height;
        init.darWidth = width;
        init.darHeight = height;
        init.frameRateNum = fps;
        init.frameRateDen = 1;
        init.maxEncodeWidth = max_width;
        init.maxEncodeHeight = max_height;
        init.enableEncodeAsync = 0; // sync D3D11 path
        init.enablePTD = 1;
        apply_initialization_options(&mut init, options);
        init.encodeConfig = cfg.as_mut();
        init.tuningInfo = options.tuning.native();
        init.bufferFormat = input_buffer_format(options);
        let st = unsafe { (ENCODE_API.initialize_encoder)(session, init.as_mut()) };
        if let Err(error) = st.result_without_string() {
            let driver_detail = unsafe { (ENCODE_API.get_last_error_string)(session) };
            let driver_detail = (!driver_detail.is_null())
                .then(|| {
                    unsafe { CStr::from_ptr(driver_detail) }
                        .to_string_lossy()
                        .into_owned()
                })
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "no driver detail".to_string());
            bail!("NvEncInitializeEncoder: {error:?} ({driver_detail})");
        }
        info!(
            width, height, max_width, max_height, fps, bitrate_bps, options = ?options,
            codec = codec.name(), rate_control = rate_control_label(options.rate_control),
            "NVENC initialized (ULL, no B-frames)"
        );

        // 4. Bitstream output buffer.
        let mut bs_params: NV_ENC_CREATE_BITSTREAM_BUFFER = unsafe { zeroed() };
        bs_params.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        let st = unsafe { (ENCODE_API.create_bitstream_buffer)(session, &mut bs_params) };
        st.result_without_string()
            .context("NvEncCreateBitstreamBuffer")?;

        Ok(Self {
            session,
            device,
            bitstream: bs_params.bitstreamBuffer,
            width,
            height,
            fps,
            max_width,
            max_height,
            bitrate_bps,
            codec,
            options,
            frame_idx: 0,
            force_idr_next: true, // first frame must be IDR for the decoder
            infinite_gop: false,
            lab_idr_telemetry: std::env::var("BUBBLEPLAY_LAB_IDR_TELEMETRY")
                .is_ok_and(|value| value == "1"),
            last_mapped: None,
            last_recon_mapped: None,
            persistent_input: None,
            persistent_recon_outputs: Vec::new(),
            cfg,
            init_params: init,
        })
    }

    /// Force the next encoded picture to be an IDR (e.g. on PLI).
    pub fn force_idr(&mut self) {
        self.force_idr_next = true;
    }

    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    pub fn is_for_device(&self, device: *mut c_void) -> bool {
        self.device == device
    }

    /// Reconfigure the encoder bitrate (BWE clamp). NVENC applies it from the
    /// next picture onward.
    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        if bitrate_bps == self.bitrate_bps {
            return Ok(());
        }
        let mut cfg = *self.cfg;
        let (average_bitrate_bps, peak_bitrate_bps) =
            encoder_rate_control_bitrates(bitrate_bps, self.options.rate_control);
        cfg.rcParams.averageBitRate = average_bitrate_bps;
        cfg.rcParams.maxBitRate = peak_bitrate_bps;
        cfg.rcParams.vbvBufferSize =
            average_bitrate_bps / self.fps.max(1) * u32::from(self.options.vbv_frames);
        cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;
        let mut init = *self.init_params;
        init.encodeConfig = &mut cfg;
        let mut reconf: NV_ENC_RECONFIGURE_PARAMS = unsafe { zeroed() };
        reconf.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        reconf.reInitEncodeParams = init;
        let st = unsafe { (ENCODE_API.reconfigure_encoder)(self.session, &mut reconf) };
        match st.result_without_string() {
            Ok(()) => {
                *self.cfg = cfg;
                *self.init_params = init;
                self.init_params.encodeConfig = self.cfg.as_mut();
                self.bitrate_bps = bitrate_bps;
                info!(bitrate_bps, "NVENC bitrate reconfigured");
                Ok(())
            }
            Err(e) => {
                warn!(bitrate_bps, ?e, "NVENC reconfigure failed");
                Err(e.into())
            }
        }
    }

    /// Switch between the configured periodic-IDR GOP and an infinite GOP
    /// (WSS v4: keyframes only on start, request or safety interval). No-op
    /// when already in the requested mode; the next picture is an IDR.
    pub fn set_infinite_gop(&mut self, infinite: bool) -> Result<()> {
        if infinite == self.infinite_gop {
            return Ok(());
        }
        let mut cfg = *self.cfg;
        apply_gop(&mut cfg, self.codec, gop_frames(self.fps, self.options.gop_seconds, infinite));
        let mut init = *self.init_params;
        init.encodeConfig = &mut cfg;
        let mut reconf: NV_ENC_RECONFIGURE_PARAMS = unsafe { zeroed() };
        reconf.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        reconf.reInitEncodeParams = init;
        reconf.set_resetEncoder(1);
        reconf.set_forceIDR(1);
        unsafe { (ENCODE_API.reconfigure_encoder)(self.session, &mut reconf) }
            .result_without_string()
            .context("NvEncReconfigureEncoder GOP")?;
        *self.cfg = cfg;
        *self.init_params = init;
        self.init_params.encodeConfig = self.cfg.as_mut();
        self.infinite_gop = infinite;
        self.force_idr_next = true;
        info!(infinite, "NVENC GOP reconfigured");
        Ok(())
    }

    pub fn infinite_gop(&self) -> bool {
        self.infinite_gop
    }

    /// Reset NVENC's rate-control state without changing the active profile.
    /// Stream Lab uses this only for explicitly gated source-quality captures.
    pub fn reset_rate_control(&mut self) -> Result<()> {
        let mut cfg = *self.cfg;
        let mut init = *self.init_params;
        init.encodeConfig = &mut cfg;
        let mut reconf: NV_ENC_RECONFIGURE_PARAMS = unsafe { zeroed() };
        reconf.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        reconf.reInitEncodeParams = init;
        reconf.set_resetEncoder(1);
        reconf.set_forceIDR(1);
        unsafe { (ENCODE_API.reconfigure_encoder)(self.session, &mut reconf) }
            .result_without_string()
            .context("NvEncReconfigureEncoder rate-control reset")?;
        *self.cfg = cfg;
        *self.init_params = init;
        self.init_params.encodeConfig = self.cfg.as_mut();
        self.force_idr_next = true;
        info!("NVENC rate-control state reset for Stream Lab quality capture");
        Ok(())
    }

    /// Dimension/fps transition. The host requests its wall-clock IDR on the
    /// first frame after this succeeds; GOP cadence itself is not reconfigured.
    pub fn reconfigure_profile(&mut self, width: u32, height: u32, fps: u32) -> Result<()> {
        if width > self.max_width || height > self.max_height {
            bail!("profile exceeds native encoder dimensions");
        }
        if width == self.width && height == self.height && fps == self.fps {
            return Ok(());
        }
        let mut cfg = *self.cfg;
        apply_gop(&mut cfg, self.codec, gop_frames(fps, self.options.gop_seconds, self.infinite_gop));
        let (average_bitrate_bps, _) =
            encoder_rate_control_bitrates(self.bitrate_bps, self.options.rate_control);
        cfg.rcParams.vbvBufferSize =
            average_bitrate_bps / fps.max(1) * u32::from(self.options.vbv_frames);
        cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;
        let mut init = *self.init_params;
        init.encodeWidth = width;
        init.encodeHeight = height;
        init.darWidth = width;
        init.darHeight = height;
        init.frameRateNum = fps;
        init.encodeConfig = &mut cfg;
        let mut reconf: NV_ENC_RECONFIGURE_PARAMS = unsafe { zeroed() };
        reconf.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        reconf.reInitEncodeParams = init;
        reconf.set_resetEncoder(1);
        unsafe { (ENCODE_API.reconfigure_encoder)(self.session, &mut reconf) }
            .result_without_string()
            .context("NvEncReconfigureEncoder profile")?;
        *self.cfg = cfg;
        *self.init_params = init;
        self.init_params.encodeConfig = self.cfg.as_mut();
        self.width = width;
        self.height = height;
        self.fps = fps;
        self.force_idr_next = true;
        Ok(())
    }

    /// Encode one captured desktop texture (zero-copy).
    ///
    /// `texture` must be an `ID3D11Texture2D` from the same D3D11 device the
    /// session was opened on. Returns the Annex B access unit.
    pub fn encode_texture(&mut self, texture: *mut c_void) -> Result<EncodedFrame> {
        self.encode_texture_with_damage(texture, None)
    }

    pub fn encode_texture_with_damage(
        &mut self,
        texture: *mut c_void,
        damage: Option<FrameDamage<'_>>,
    ) -> Result<EncodedFrame> {
        self.encode_texture_with_damage_and_reconstruction(texture, damage, None)
    }

    pub fn encode_texture_with_feedback(
        &mut self,
        texture: *mut c_void,
        damage: Option<FrameDamage<'_>>,
        reconstruction_texture: *mut c_void,
    ) -> Result<EncodedFrame> {
        self.encode_texture_with_damage_and_reconstruction(
            texture,
            damage,
            Some(reconstruction_texture),
        )
    }

    fn encode_texture_with_damage_and_reconstruction(
        &mut self,
        texture: *mut c_void,
        damage: Option<FrameDamage<'_>>,
        reconstruction_texture: Option<*mut c_void>,
    ) -> Result<EncodedFrame> {
        let feedback_enabled = self.options.roi_mode == RoiMode::Feedback;
        ensure!(
            feedback_enabled == reconstruction_texture.is_some(),
            if feedback_enabled {
                "feedback encode requires a reconstructed-output texture"
            } else {
                "reconstructed-output texture supplied outside feedback mode"
            }
        );
        if let Some(texture) = reconstruction_texture {
            ensure!(!texture.is_null(), "null reconstructed-output texture");
        }
        let t0 = std::time::Instant::now();
        let mut stage_timings = EncodeStageTimings::default();

        // Register -> map the input texture.
        let registration_action = resource_registration_action(
            self.options.resource_mode,
            self.persistent_input.as_ref().map(|input| input.texture),
            texture,
        );
        let per_frame_registration = registration_action == ResourceRegistrationAction::PerFrame;
        let registered = match registration_action {
            ResourceRegistrationAction::PerFrame => {
                self.register_input(texture, &mut stage_timings)?
            }
            ResourceRegistrationAction::RegisterPersistent => {
                let registered = self.register_input(texture, &mut stage_timings)?;
                self.persistent_input = Some(PersistentInput {
                    texture,
                    registered,
                });
                registered
            }
            ResourceRegistrationAction::ReusePersistent => {
                self.persistent_input
                    .as_ref()
                    .expect("persistent input selected above")
                    .registered
            }
            ResourceRegistrationAction::ReplacePersistent => {
                self.unregister_persistent_input(&mut stage_timings);
                let registered = self.register_input(texture, &mut stage_timings)?;
                self.persistent_input = Some(PersistentInput {
                    texture,
                    registered,
                });
                registered
            }
        };

        let mut map: NV_ENC_MAP_INPUT_RESOURCE = unsafe { zeroed() };
        map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        // `inputResource` is a deprecated field ("Do not use" per the SDK
        // header); the registered handle belongs in `registeredResource`.
        map.registeredResource = registered;
        let map_started = std::time::Instant::now();
        let st = unsafe { (ENCODE_API.map_input_resource)(self.session, &mut map) };
        stage_timings.map_us = map_started.elapsed().as_micros() as u64;
        if let Err(e) = st.result_without_string().context("NvEncMapInputResource") {
            if per_frame_registration {
                self.unregister_input(registered, &mut stage_timings);
            } else {
                self.unregister_persistent_input(&mut stage_timings);
            }
            return Err(e);
        }
        let mapped = map.mappedResource;
        self.last_mapped = Some(mapped);

        let mapped_recon = match reconstruction_texture {
            Some(texture) => match self.map_recon_output(texture) {
                Ok(mapped) => Some(mapped),
                Err(error) => {
                    stage_timings.unmap_us = self.unmap_current();
                    if per_frame_registration {
                        self.unregister_input(registered, &mut stage_timings);
                    }
                    return Err(error);
                }
            },
            None => None,
        };

        // Encode.
        let force_idr = self.force_idr_next;
        let mut pic: NV_ENC_PIC_PARAMS = unsafe { zeroed() };
        pic.version = NV_ENC_PIC_PARAMS_VER;
        pic.inputWidth = self.width;
        pic.inputHeight = self.height;
        pic.inputPitch = self.width;
        pic.encodePicFlags = if force_idr {
            NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
        } else {
            0
        };
        if let Some(mapped_recon) = mapped_recon {
            pic.encodePicFlags |= NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_RECON_FRAME as u32;
            pic.outputReconBuffer = mapped_recon;
        }
        pic.frameIdx = self.frame_idx as u32;
        pic.inputTimeStamp = self.frame_idx;
        pic.inputBuffer = mapped;
        pic.outputBitstream = self.bitstream;
        pic.bufferFmt = input_buffer_format(self.options);
        pic.pictureStruct = NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
        pic.pictureType = NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_UNKNOWN;
        let mut roi_evaluation = evaluate_roi_map(
            self.options,
            self.codec,
            force_idr,
            self.width,
            self.height,
            damage,
        );
        let mut qp_map = roi_evaluation.map.take();
        let qp_map_stats = if let Some(map) = qp_map.as_mut() {
            let expected = self.width.div_ceil(32) * self.height.div_ceil(32);
            ensure!(
                map.columns * map.rows == expected && map.values.len() == expected as usize,
                "HEVC QP delta map dimensions do not match encoder CTB grid"
            );
            pic.qpDeltaMap = map.values.as_mut_ptr();
            pic.qpDeltaMapSize = map.values.len() as u32;
            Some(QpMapStats {
                changed_ctbs: map.values.iter().filter(|value| **value < 0).count() as u32,
                total_ctbs: map.values.len() as u32,
                min_delta: map.values.iter().copied().min().unwrap_or(0),
                max_delta: map.values.iter().copied().max().unwrap_or(0),
            })
        } else {
            None
        };
        let submit_started = std::time::Instant::now();
        let st = unsafe { (ENCODE_API.encode_picture)(self.session, &mut pic) };
        stage_timings.submit_us = submit_started.elapsed().as_micros() as u64;
        let res = st.result_without_string().context("NvEncEncodePicture");
        if let Err(e) = res {
            self.unmap_recon_output();
            stage_timings.unmap_us = self.unmap_current();
            if per_frame_registration {
                self.unregister_input(registered, &mut stage_timings);
            }
            return Err(e);
        }

        // Lock bitstream (sync path: with ULL tune the output is ready right
        // after encode_picture returns).
        let mut lock: NV_ENC_LOCK_BITSTREAM = unsafe { zeroed() };
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = self.bitstream as *mut c_void;
        let lock_started = std::time::Instant::now();
        let st = unsafe { (ENCODE_API.lock_bitstream)(self.session, &mut lock) };
        let locked = st.result_without_string().context("NvEncLockBitstream");
        stage_timings.lock_us = lock_started.elapsed().as_micros() as u64;

        let mut result = None;
        if locked.is_ok() {
            let len = lock.bitstreamSizeInBytes as usize;
            let copy_started = std::time::Instant::now();
            let mut out = vec![0u8; len];
            unsafe {
                ptr::copy_nonoverlapping(
                    lock.bitstreamBufferPtr as *const u8,
                    out.as_mut_ptr(),
                    len,
                );
            }
            stage_timings.copy_us = copy_started.elapsed().as_micros() as u64;
            let st = unsafe { (ENCODE_API.unlock_bitstream)(self.session, self.bitstream) };
            if let Err(e) = st.result_without_string() {
                warn!(?e, "unlock_bitstream");
            }
            result = Some(out);
        } else if let Err(e) = locked {
            warn!(?e, "lock_bitstream failed");
        }

        // Unmap the reconstructed output before the D3D11 analysis pass, then
        // unmap + unregister the input. Registrations for the bounded recon
        // texture ring remain live for the encoder session.
        self.unmap_recon_output();
        stage_timings.unmap_us = self.unmap_current();
        if per_frame_registration {
            self.unregister_input(registered, &mut stage_timings);
        }

        let Some(data) = result else {
            bail!("no bitstream output")
        };
        let sdk_random_access = lock.pictureType == NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR;
        let is_idr = annex_b_is_random_access(self.codec, &data);
        if sdk_random_access != is_idr {
            debug!(
                sdk_random_access,
                payload_random_access = is_idr,
                force_idr,
                "NVENC picture type differs from Annex-B random-access detection"
            );
        }
        if force_idr && !is_idr {
            warn!("forced NVENC picture did not contain a detected random-access NAL");
        }
        if self.lab_idr_telemetry && is_idr {
            info!(
                frame_idx = self.frame_idx,
                force_idr,
                bytes = data.len(),
                frame_avg_qp = lock.frameAvgQP,
                frame_satd = lock.frameSatd,
                picture_type = lock.pictureType as u32,
                "stream_lab_idr"
            );
        }
        self.frame_idx += 1;
        self.force_idr_next = false;

        Ok(EncodedFrame {
            is_idr,
            encode_us: t0.elapsed().as_micros() as u64,
            data,
            qp_map: qp_map_stats,
            roi_activity: roi_evaluation.telemetry,
            stage_timings,
        })
    }

    fn register_input(
        &self,
        texture: *mut c_void,
        timings: &mut EncodeStageTimings,
    ) -> Result<*mut c_void> {
        let started = std::time::Instant::now();
        let mut reg: NV_ENC_REGISTER_RESOURCE = unsafe { zeroed() };
        reg.version = NV_ENC_REGISTER_RESOURCE_VER;
        reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        reg.width = self.width;
        reg.height = self.height;
        reg.pitch = 0;
        reg.resourceToRegister = texture;
        reg.bufferFormat = input_buffer_format(self.options);
        let st = unsafe { (ENCODE_API.register_resource)(self.session, &mut reg) };
        let result = st.result_without_string().context("NvEncRegisterResource");
        timings.register_calls = timings.register_calls.saturating_add(1);
        timings.register_us = timings
            .register_us
            .saturating_add(started.elapsed().as_micros() as u64);
        result?;
        Ok(reg.registeredResource)
    }

    fn map_recon_output(&mut self, texture: *mut c_void) -> Result<*mut c_void> {
        let registered = if let Some(output) = self
            .persistent_recon_outputs
            .iter()
            .find(|output| output.texture == texture)
        {
            output.registered
        } else {
            ensure!(
                self.persistent_recon_outputs.len() < 2,
                "feedback reconstruction texture ring exceeds two surfaces"
            );
            let mut reg: NV_ENC_REGISTER_RESOURCE = unsafe { zeroed() };
            reg.version = NV_ENC_REGISTER_RESOURCE_VER;
            reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
            reg.width = self.width;
            reg.height = self.height;
            reg.pitch = 0;
            reg.resourceToRegister = texture;
            reg.bufferFormat = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12;
            reg.bufferUsage = NV_ENC_BUFFER_USAGE::NV_ENC_OUTPUT_RECON;
            let st = unsafe { (ENCODE_API.register_resource)(self.session, &mut reg) };
            st.result_without_string()
                .context("NvEncRegisterResource(reconstruction output)")?;
            self.persistent_recon_outputs.push(PersistentReconOutput {
                texture,
                registered: reg.registeredResource,
            });
            reg.registeredResource
        };

        let mut map: NV_ENC_MAP_INPUT_RESOURCE = unsafe { zeroed() };
        map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        map.registeredResource = registered;
        let st = unsafe { (ENCODE_API.map_input_resource)(self.session, &mut map) };
        st.result_without_string()
            .context("NvEncMapInputResource(reconstruction output)")?;
        self.last_recon_mapped = Some(map.mappedResource);
        Ok(map.mappedResource)
    }

    fn unregister_input(&self, registered: *mut c_void, timings: &mut EncodeStageTimings) {
        if registered.is_null() {
            return;
        }
        let started = std::time::Instant::now();
        let st = unsafe { (ENCODE_API.unregister_resource)(self.session, registered) };
        timings.unregister_calls = timings.unregister_calls.saturating_add(1);
        timings.unregister_us = timings
            .unregister_us
            .saturating_add(started.elapsed().as_micros() as u64);
        if let Err(e) = st.result_without_string() {
            debug!(?e, "unregister_resource");
        }
    }

    fn unregister_persistent_input(&mut self, timings: &mut EncodeStageTimings) {
        if let Some(input) = self.persistent_input.take() {
            self.unregister_input(input.registered, timings);
        }
    }

    fn unregister_recon_outputs(&mut self) {
        for output in self.persistent_recon_outputs.drain(..) {
            let st = unsafe { (ENCODE_API.unregister_resource)(self.session, output.registered) };
            if let Err(error) = st.result_without_string() {
                debug!(?error, "unregister reconstruction output");
            }
        }
    }

    fn unmap_recon_output(&mut self) {
        if let Some(mapped) = self.last_recon_mapped.take() {
            let st = unsafe { (ENCODE_API.unmap_input_resource)(self.session, mapped) };
            if let Err(error) = st.result_without_string() {
                debug!(?error, "unmap reconstruction output");
            }
        }
    }

    fn unmap_current(&mut self) -> u64 {
        let started = std::time::Instant::now();
        if let Some(mapped) = self.last_mapped.take() {
            let st = unsafe { (ENCODE_API.unmap_input_resource)(self.session, mapped) };
            if let Err(e) = st.result_without_string() {
                debug!(?e, "unmap_input_resource");
            }
        }
        started.elapsed().as_micros() as u64
    }

    /// Fetch SPS/PPS (Annex B) — for decoder config / sanity checks.
    pub fn sequence_params(&self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 512];
        let mut out_size: u32 = 0;
        let mut payload: NV_ENC_SEQUENCE_PARAM_PAYLOAD = unsafe { zeroed() };
        payload.version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
        payload.inBufferSize = buf.len() as u32;
        payload.spsppsBuffer = buf.as_mut_ptr() as *mut c_void;
        payload.outSPSPPSPayloadSize = &mut out_size;
        let st = unsafe { (ENCODE_API.get_sequence_params)(self.session, &mut payload) };
        st.result_without_string()
            .context("NvEncGetSequenceParams")?;
        buf.truncate(out_size as usize);
        Ok(buf)
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate_bps
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }
}

impl Drop for Nvenc {
    fn drop(&mut self) {
        self.unmap_recon_output();
        self.unmap_current();
        let mut timings = EncodeStageTimings::default();
        self.unregister_persistent_input(&mut timings);
        self.unregister_recon_outputs();
        if !self.bitstream.is_null() {
            let _ = unsafe { (ENCODE_API.destroy_bitstream_buffer)(self.session, self.bitstream) };
            self.bitstream = ptr::null_mut();
        }
        if !self.session.is_null() {
            let _ = unsafe { (ENCODE_API.destroy_encoder)(self.session) };
            self.session = ptr::null_mut();
        }
    }
}
