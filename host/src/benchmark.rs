//! Expiring, local-only stream-lab override contract.
//!
//! The transport process polls this file while it is already awake. Invalid,
//! expired, or out-of-policy profiles are treated exactly like no profile so
//! production encoder policy resumes without an explicit cleanup message.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const MAX_LEASE_MS: u64 = 10 * 60 * 1_000;
const MAX_PROFILE_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BenchmarkTransport {
    Wss,
    Webrtc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BenchmarkStreamMode {
    #[default]
    Work,
    Game,
}

impl BenchmarkStreamMode {
    pub const fn max_fps(self) -> u16 {
        match self {
            Self::Work => 30,
            Self::Game => 60,
        }
    }
}

const fn default_gop_seconds() -> u8 {
    2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BenchmarkPreset {
    P1,
    P2,
    P3,
    P4,
    P5,
    P6,
    P7,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BenchmarkTuning {
    UltraLowLatency,
    LowLatency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BenchmarkMultipass {
    Disabled,
    Quarter,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BenchmarkImplementationVariant {
    #[default]
    Production,
    Candidate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BenchmarkAqMode {
    None,
    Spatial,
    Temporal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BenchmarkRoiMode {
    #[default]
    Off,
    Damage,
    TextEdge,
    Feedback,
    FeedbackHeadroom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BenchmarkResourceMode {
    #[default]
    PerFrame,
    Persistent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BenchmarkRateControlMode {
    #[default]
    Cbr,
    ExternalBudgetVbr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BenchmarkHybridMode {
    #[default]
    Off,
    PngStaticRoi,
    PngQuiescentDetail,
    HevcQuiescentTiles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BenchmarkHybridCacheMode {
    DdaGated,
    #[default]
    SuppressReactivate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BenchmarkEncoder {
    pub preset: BenchmarkPreset,
    pub tuning: BenchmarkTuning,
    #[serde(default)]
    pub rate_control_mode: BenchmarkRateControlMode,
    #[serde(default)]
    pub target_quality: Option<u8>,
    #[serde(default)]
    pub peak_bitrate_multiplier: Option<u8>,
    pub multipass: BenchmarkMultipass,
    pub aq_strength: u8,
    #[serde(default)]
    pub temporal_aq: bool,
    #[serde(default)]
    pub aq_mode: Option<BenchmarkAqMode>,
    pub vbv_frames: u8,
    #[serde(default)]
    pub lookahead_depth: u8,
    #[serde(default)]
    pub weighted_prediction: bool,
    #[serde(default)]
    pub dirty_qp_map: bool,
    #[serde(default)]
    pub main10: bool,
    #[serde(default)]
    pub roi_mode: BenchmarkRoiMode,
    #[serde(default)]
    pub resource_mode: BenchmarkResourceMode,
    #[serde(default)]
    pub async_depth: u8,
}

#[derive(Debug, Clone, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BenchmarkProfile {
    pub schema_version: u8,
    #[serde(default)]
    pub implementation_variant: BenchmarkImplementationVariant,
    #[serde(default)]
    pub experiment_id: Option<String>,
    pub run_id: String,
    pub expires_at_ms: u64,
    #[serde(default)]
    pub apply_revision: u64,
    pub transport: BenchmarkTransport,
    pub codec: String,
    #[serde(default)]
    pub stream_mode: BenchmarkStreamMode,
    pub adaptive: bool,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_bps: u32,
    #[serde(default)]
    pub requested_bitrate_bps: Option<u32>,
    #[serde(default)]
    pub total_media_cap_bps: Option<u32>,
    #[serde(default = "default_gop_seconds")]
    pub gop_seconds: u8,
    #[serde(default)]
    pub hybrid_mode: BenchmarkHybridMode,
    #[serde(default)]
    pub hybrid_cache_mode: BenchmarkHybridCacheMode,
    pub encoder: BenchmarkEncoder,
}

impl PartialEq for BenchmarkProfile {
    fn eq(&self, other: &Self) -> bool {
        // Lease renewal changes only expiry. Treat that as the same effective
        // encoder policy so the host refresh does not stage an encoder/IDR
        // transition during a long-running benchmark.
        self.schema_version == other.schema_version
            && self.run_id == other.run_id
            && self.implementation_variant == other.implementation_variant
            && self.experiment_id == other.experiment_id
            && self.apply_revision == other.apply_revision
            && self.transport == other.transport
            && self.codec == other.codec
            && self.stream_mode == other.stream_mode
            && self.adaptive == other.adaptive
            && self.width == other.width
            && self.height == other.height
            && self.fps == other.fps
            && self.bitrate_bps == other.bitrate_bps
            && self.requested_bitrate_bps == other.requested_bitrate_bps
            && self.total_media_cap_bps == other.total_media_cap_bps
            && self.gop_seconds == other.gop_seconds
            && self.hybrid_mode == other.hybrid_mode
            && self.hybrid_cache_mode == other.hybrid_cache_mode
            && self.encoder == other.encoder
    }
}

impl BenchmarkProfile {
    fn has_valid_rate_control(&self) -> bool {
        match self.encoder.rate_control_mode {
            BenchmarkRateControlMode::Cbr => {
                self.encoder.target_quality.is_none()
                    && self.encoder.peak_bitrate_multiplier.is_none()
            }
            BenchmarkRateControlMode::ExternalBudgetVbr => {
                self.encoder.target_quality == Some(23)
                    && self.encoder.peak_bitrate_multiplier == Some(4)
                    && self.encoder.vbv_frames == 4
            }
        }
    }

    pub const fn effective_aq_mode(&self) -> BenchmarkAqMode {
        match self.encoder.aq_mode {
            Some(mode) => mode,
            None if self.encoder.temporal_aq => BenchmarkAqMode::Temporal,
            None => BenchmarkAqMode::Spatial,
        }
    }

    pub const fn effective_roi_mode(&self) -> BenchmarkRoiMode {
        match self.encoder.roi_mode {
            BenchmarkRoiMode::Off if self.encoder.dirty_qp_map => BenchmarkRoiMode::Damage,
            mode => mode,
        }
    }

    fn uses_pipeline_experiment(&self) -> bool {
        self.effective_roi_mode() != BenchmarkRoiMode::Off
            || self.hybrid_mode != BenchmarkHybridMode::Off
            || self.encoder.resource_mode != BenchmarkResourceMode::PerFrame
            || self.encoder.async_depth != 0
            || self.encoder.rate_control_mode != BenchmarkRateControlMode::Cbr
    }

    fn has_valid_pipeline_experiment(&self) -> bool {
        if !self.uses_pipeline_experiment() {
            return true;
        }

        let roi_active = self.effective_roi_mode() != BenchmarkRoiMode::Off;
        let hybrid_active = self.hybrid_mode != BenchmarkHybridMode::Off;
        let resource_active = self.encoder.resource_mode != BenchmarkResourceMode::PerFrame
            || self.encoder.async_depth != 0;
        let rate_control_active = self.encoder.rate_control_mode != BenchmarkRateControlMode::Cbr;
        let active_axes = u8::from(roi_active)
            + u8::from(hybrid_active)
            + u8::from(resource_active)
            + u8::from(rate_control_active);
        self.schema_version == 2
            && self.implementation_variant == BenchmarkImplementationVariant::Candidate
            && self.transport == BenchmarkTransport::Wss
            && self.stream_mode == BenchmarkStreamMode::Work
            && self.width == 1920
            && self.height == 1080
            && self.fps == 30
            && self.requested_bitrate_bps.unwrap_or(self.bitrate_bps) == 6_000_000
            && self.total_media_cap_bps.unwrap_or(self.bitrate_bps) == 6_000_000
            && self.bitrate_bps <= self.requested_bitrate_bps.unwrap_or(self.bitrate_bps)
            && self.bitrate_bps <= self.total_media_cap_bps.unwrap_or(self.bitrate_bps)
            && !self.encoder.main10
            && active_axes == 1
            && self.encoder.async_depth <= 1
            && (self.encoder.async_depth == 0
                || self.encoder.resource_mode == BenchmarkResourceMode::Persistent)
            && (!self.encoder.dirty_qp_map
                || matches!(
                    self.encoder.roi_mode,
                    BenchmarkRoiMode::Off | BenchmarkRoiMode::Damage
                ))
            && self.has_valid_rate_control()
            && (self.hybrid_cache_mode == BenchmarkHybridCacheMode::SuppressReactivate
                || self.hybrid_mode == BenchmarkHybridMode::PngQuiescentDetail)
    }

    fn is_valid(&self, now_ms: u64) -> bool {
        matches!(self.schema_version, 1 | 2)
            && valid_uuid(&self.run_id)
            && self.expires_at_ms > now_ms
            && self.expires_at_ms.saturating_sub(now_ms) <= MAX_LEASE_MS
            && self.codec == "hevc"
            && !self.adaptive
            && (320..=3840).contains(&self.width)
            && (180..=2160).contains(&self.height)
            && (1..=self.stream_mode.max_fps()).contains(&self.fps)
            && if self.schema_version == 1 {
                self.implementation_variant == BenchmarkImplementationVariant::Production
                    && self.experiment_id.is_none()
                    && self.encoder.aq_mode.is_none()
                    && self.encoder.lookahead_depth == 0
                    && !self.encoder.weighted_prediction
                    && !self.encoder.dirty_qp_map
                    && !self.encoder.main10
                    && self.encoder.roi_mode == BenchmarkRoiMode::Off
                    && self.encoder.resource_mode == BenchmarkResourceMode::PerFrame
                    && self.encoder.async_depth == 0
                    && self.encoder.rate_control_mode == BenchmarkRateControlMode::Cbr
                    && self.encoder.target_quality.is_none()
                    && self.encoder.peak_bitrate_multiplier.is_none()
                    && self.hybrid_mode == BenchmarkHybridMode::Off
                    && self.hybrid_cache_mode == BenchmarkHybridCacheMode::SuppressReactivate
                    && self.requested_bitrate_bps.is_none()
                    && self.total_media_cap_bps.is_none()
                    && (500_000..=40_000_000).contains(&self.bitrate_bps)
            } else {
                self.experiment_id
                    .as_deref()
                    .is_some_and(valid_experiment_id)
                    && self.encoder.aq_mode.is_some()
                    && !self.encoder.temporal_aq
                    && (500_000..=12_000_000).contains(&self.bitrate_bps)
                    && self.requested_bitrate_bps.is_none_or(|value| {
                        (500_000..=12_000_000).contains(&value) && self.bitrate_bps <= value
                    })
                    && self.total_media_cap_bps.is_none_or(|value| {
                        (500_000..=12_000_000).contains(&value) && self.bitrate_bps <= value
                    })
                    && self.encoder.lookahead_depth
                        <= match self.stream_mode {
                            BenchmarkStreamMode::Work => 2,
                            BenchmarkStreamMode::Game => 1,
                        }
                    && (!self.encoder.main10
                        || (self.implementation_variant
                            == BenchmarkImplementationVariant::Candidate
                            && self.transport == BenchmarkTransport::Wss
                            && self.stream_mode == BenchmarkStreamMode::Work
                            && self.width == 1920
                            && self.height == 1080
                            && self.fps == 30))
            }
            && self.has_valid_pipeline_experiment()
            && self.has_valid_rate_control()
            && matches!(self.gop_seconds, 1 | 2 | 4)
            && (1..=15).contains(&self.encoder.aq_strength)
            && (1..=4).contains(&self.encoder.vbv_frames)
    }
}

fn valid_experiment_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub fn program_data_path() -> PathBuf {
    std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("BubblePlay")
        .join("benchmark-v1.json")
}

pub fn load_profile(path: &Path, now_ms: u64) -> Option<BenchmarkProfile> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_PROFILE_BYTES
    {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let profile: BenchmarkProfile = serde_json::from_slice(&bytes).ok()?;
    profile.is_valid(now_ms).then_some(profile)
}

#[derive(Debug, Default)]
pub struct BenchmarkLease {
    path: PathBuf,
    active: Option<BenchmarkProfile>,
}

impl BenchmarkLease {
    pub fn program_data() -> Self {
        Self {
            path: program_data_path(),
            active: None,
        }
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path, active: None }
    }

    /// Reload the lease. Returning `None` is the safe auto-revert signal.
    pub fn refresh(&mut self, now_ms: u64) -> Option<&BenchmarkProfile> {
        self.active = load_profile(&self.path, now_ms);
        self.active.as_ref()
    }

    pub fn active(&self) -> Option<&BenchmarkProfile> {
        self.active.as_ref()
    }
}

fn valid_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => byte.is_ascii_hexdigit(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_PROFILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_profile(json: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = TEMP_PROFILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bubbleplay-benchmark-{}-{suffix}-{sequence}.json",
            std::process::id()
        ));
        fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn accepts_a_strict_bounded_work_hevc_lease() {
        let path = temp_profile(
            r#"{
                "schemaVersion":1,
                "runId":"123e4567-e89b-42d3-a456-426614174000",
                "expiresAtMs":1300000,
                "transport":"wss",
                "codec":"hevc",
                "streamMode":"work",
                "adaptive":false,
                "width":1920,
                "height":1080,
                "fps":30,
                "bitrateBps":4000000,
                "encoder":{"preset":"p5","tuning":"low-latency","multipass":"quarter","aqStrength":10,"temporalAq":true,"vbvFrames":4}
            }"#,
        );
        let profile = load_profile(&path, 1_000_000).expect("valid profile");
        assert_eq!(profile.encoder.preset, BenchmarkPreset::P5);
        assert_eq!(profile.encoder.vbv_frames, 4);
        assert_eq!(profile.stream_mode, BenchmarkStreamMode::Work);
        assert_eq!(profile.gop_seconds, 2);
        assert_eq!(profile.apply_revision, 0);
        assert_eq!(
            profile.implementation_variant,
            BenchmarkImplementationVariant::Production
        );
        assert_eq!(profile.effective_aq_mode(), BenchmarkAqMode::Temporal);
        fs::remove_file(path).ok();
    }

    #[test]
    fn accepts_game_at_sixty_fps_and_allowlisted_one_second_gop() {
        let path = temp_profile(
            r#"{"schemaVersion":1,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"applyRevision":7,"transport":"wss","codec":"hevc","streamMode":"game","adaptive":false,"width":1920,"height":1080,"fps":60,"bitrateBps":8000000,"gopSeconds":1,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqStrength":8,"temporalAq":false,"vbvFrames":1}}"#,
        );
        let profile = load_profile(&path, 1_000_000).expect("revision-aware profile");
        assert_eq!(profile.apply_revision, 7);
        fs::remove_file(path).ok();
    }

    #[test]
    fn accepts_bounded_v2_candidate_profile_with_expanded_encoder_controls() {
        let path = temp_profile(
            r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"nvenc-aq_01","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":12000000,"gopSeconds":4,"encoder":{"preset":"p7","tuning":"low-latency","multipass":"full","aqMode":"spatial","aqStrength":15,"vbvFrames":4,"lookaheadDepth":2,"weightedPrediction":true}}"#,
        );
        load_profile(&path, 1_000_000).expect("valid candidate profile");
        fs::remove_file(path).ok();
    }

    #[test]
    fn accepts_dirty_qp_map_only_as_an_explicit_v2_candidate_control() {
        let path = temp_profile(
            r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"dirty-qp-map","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":6000000,"gopSeconds":2,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false,"dirtyQpMap":true}}"#,
        );
        let profile = load_profile(&path, 1_000_000).expect("valid dirty-QP candidate");
        assert_eq!(profile.experiment_id.as_deref(), Some("dirty-qp-map"));
        assert!(profile.encoder.dirty_qp_map);
        fs::remove_file(path).ok();
    }

    #[test]
    fn accepts_additive_v2_pipeline_candidate_controls() {
        for json in [
            r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"roi-text-edge","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":6000000,"gopSeconds":2,"hybridMode":"off","encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false,"roiMode":"text-edge","resourceMode":"per-frame","asyncDepth":0}}"#,
            r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"roi-feedback-headroom","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":6000000,"gopSeconds":2,"hybridMode":"off","encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false,"roiMode":"feedback-headroom","resourceMode":"per-frame","asyncDepth":0}}"#,
            r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"hybrid-png","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":6000000,"gopSeconds":2,"hybridMode":"png-static-roi","encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false}}"#,
            r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"hybrid-quiescent","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":6000000,"gopSeconds":2,"hybridMode":"png-quiescent-detail","encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false}}"#,
            r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"nvenc-persistent","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":6000000,"gopSeconds":2,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false,"resourceMode":"persistent","asyncDepth":0}}"#,
        ] {
            let path = temp_profile(json);
            assert!(
                load_profile(&path, 1_000_000).is_some(),
                "rejected additive candidate {json}"
            );
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn accepts_default_off_lab_only_hevc_quiescent_tiles_profile() {
        let path = temp_profile(
            r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"hevc-quiescent-tiles","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":6000000,"requestedBitrateBps":6000000,"totalMediaCapBps":6000000,"gopSeconds":2,"hybridMode":"hevc-quiescent-tiles","encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false,"roiMode":"off","resourceMode":"per-frame","asyncDepth":0}}"#,
        );

        load_profile(&path, 1_000_000)
            .expect("bounded codec-native tile candidate should be admitted");
        fs::remove_file(path).ok();
    }

    #[test]
    fn accepts_cq23_external_budget_vbr_as_one_bounded_pipeline_axis() {
        let valid = r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"external-budget-vbr-cq23","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":6000000,"requestedBitrateBps":6000000,"totalMediaCapBps":6000000,"gopSeconds":2,"hybridMode":"off","encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":4,"lookaheadDepth":0,"weightedPrediction":false,"roiMode":"off","resourceMode":"per-frame","asyncDepth":0,"rateControlMode":"external-budget-vbr","targetQuality":23,"peakBitrateMultiplier":4}}"#;
        let path = temp_profile(valid);
        assert!(
            load_profile(&path, 1_000_000).is_some(),
            "bounded external-budget VBR candidate should be admitted"
        );
        fs::remove_file(path).ok();

        for invalid in [
            valid.replace("\"targetQuality\":23", "\"targetQuality\":22"),
            valid.replace("\"roiMode\":\"off\"", "\"roiMode\":\"feedback\""),
        ] {
            let path = temp_profile(&invalid);
            assert!(
                load_profile(&path, 1_000_000).is_none(),
                "accepted {invalid}"
            );
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn accepts_explicit_quiescent_cache_policy_control() {
        let path = temp_profile(
            r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"hybrid-quiescent-dda-gated","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":6000000,"gopSeconds":2,"hybridMode":"png-quiescent-detail","hybridCacheMode":"dda-gated","encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false}}"#,
        );

        assert!(
            load_profile(&path, 1_000_000).is_some(),
            "same-binary DDA-gated quiescent control must be admitted"
        );
        fs::remove_file(path).ok();
    }

    #[test]
    fn accepts_a_calibrated_pipeline_target_only_under_the_six_megabit_control_cap() {
        let valid = r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"hybrid-cache-calibrated","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":5400000,"requestedBitrateBps":6000000,"totalMediaCapBps":6000000,"gopSeconds":2,"hybridMode":"png-quiescent-detail","hybridCacheMode":"suppress-reactivate","encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false}}"#;
        let path = temp_profile(valid);
        let profile = load_profile(&path, 1_000_000).expect("calibrated six-megabit candidate");
        assert_eq!(profile.bitrate_bps, 5_400_000);
        assert_eq!(profile.requested_bitrate_bps, Some(6_000_000));
        assert_eq!(profile.total_media_cap_bps, Some(6_000_000));
        fs::remove_file(path).ok();

        for invalid in [
            valid.replace(
                "\"requestedBitrateBps\":6000000",
                "\"requestedBitrateBps\":5400000",
            ),
            valid.replace(
                "\"totalMediaCapBps\":6000000",
                "\"totalMediaCapBps\":4000000",
            ),
            valid.replace("\"bitrateBps\":5400000", "\"bitrateBps\":6100000"),
        ] {
            let path = temp_profile(&invalid);
            assert!(
                load_profile(&path, 1_000_000).is_none(),
                "accepted {invalid}"
            );
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn rejects_candidates_that_mix_independent_pipeline_axes() {
        let path = temp_profile(
            r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"mixed","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":6000000,"gopSeconds":2,"hybridMode":"png-static-roi","encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false,"roiMode":"damage","resourceMode":"persistent","asyncDepth":1}}"#,
        );
        assert!(load_profile(&path, 1_000_000).is_none());
        fs::remove_file(path).ok();
    }

    #[test]
    fn accepts_main10_only_for_an_explicit_1080p_v2_candidate() {
        let valid = r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"hevc-main10","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":6000000,"gopSeconds":2,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false,"dirtyQpMap":false,"main10":true}}"#;
        let path = temp_profile(valid);
        let profile = load_profile(&path, 1_000_000).expect("valid Main10 candidate");
        assert_eq!(profile.experiment_id.as_deref(), Some("hevc-main10"));
        fs::remove_file(path).ok();

        for invalid in [
            valid.replace("\"candidate\"", "\"production\""),
            valid.replace("\"width\":1920", "\"width\":1280"),
            valid.replace("\"height\":1080", "\"height\":720"),
        ] {
            let path = temp_profile(&invalid);
            assert!(
                load_profile(&path, 1_000_000).is_none(),
                "accepted {invalid}"
            );
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn rejects_unknown_fields_wrong_policy_and_out_of_range_encoder_settings() {
        let templates = [
            r#"{"schemaVersion":1,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"transport":"wss","codec":"h264","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":4000000,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqStrength":8,"temporalAq":false,"vbvFrames":1}}"#,
            r#"{"schemaVersion":1,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"transport":"wss","codec":"hevc","streamMode":"game","adaptive":false,"width":1920,"height":1080,"fps":61,"bitrateBps":4000000,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqStrength":8,"temporalAq":false,"vbvFrames":1}}"#,
            r#"{"schemaVersion":1,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":4000000,"extra":true,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqStrength":8,"temporalAq":false,"vbvFrames":1}}"#,
            r#"{"schemaVersion":1,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":4000000,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqStrength":16,"temporalAq":false,"vbvFrames":1}}"#,
        ];
        for json in templates {
            let path = temp_profile(json);
            assert!(load_profile(&path, 1_000_000).is_none(), "accepted {json}");
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn missing_mode_and_gop_use_safe_work_defaults() {
        let path = temp_profile(
            r#"{"schemaVersion":1,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"transport":"wss","codec":"hevc","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":4000000,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqStrength":8,"temporalAq":false,"vbvFrames":1}}"#,
        );
        let profile = load_profile(&path, 1_000_000).expect("safe legacy defaults");
        assert_eq!(profile.stream_mode, BenchmarkStreamMode::Work);
        assert_eq!(profile.gop_seconds, 2);
        assert_eq!(profile.apply_revision, 0);
        fs::remove_file(path).ok();
    }

    #[test]
    fn rejects_work_above_thirty_fps_and_non_allowlisted_gop() {
        for json in [
            r#"{"schemaVersion":1,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":31,"bitrateBps":4000000,"gopSeconds":2,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqStrength":8,"temporalAq":false,"vbvFrames":1}}"#,
            r#"{"schemaVersion":1,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"transport":"wss","codec":"hevc","streamMode":"game","adaptive":false,"width":1920,"height":1080,"fps":60,"bitrateBps":4000000,"gopSeconds":3,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqStrength":8,"temporalAq":false,"vbvFrames":1}}"#,
        ] {
            let path = temp_profile(json);
            assert!(load_profile(&path, 1_000_000).is_none(), "accepted {json}");
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn rejects_v2_profiles_outside_identifier_bitrate_and_mode_lookahead_bounds() {
        let valid = r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"candidate-01","transport":"wss","codec":"hevc","streamMode":"game","adaptive":false,"width":1920,"height":1080,"fps":60,"bitrateBps":8000000,"gopSeconds":1,"encoder":{"preset":"p2","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"none","aqStrength":1,"vbvFrames":1,"lookaheadDepth":1,"weightedPrediction":false}}"#;
        for invalid in [
            valid.replace("candidate-01", "bad experiment"),
            valid.replace("8000000", "12000001"),
            valid.replace("\"lookaheadDepth\":1", "\"lookaheadDepth\":2"),
            valid.replace("\"aqMode\":\"none\",", ""),
        ] {
            let path = temp_profile(&invalid);
            assert!(
                load_profile(&path, 1_000_000).is_none(),
                "accepted {invalid}"
            );
            fs::remove_file(path).ok();
        }
    }

    #[test]
    fn v2_experiment_floor_stays_two_mbps_but_effective_shaped_profiles_allow_half_mbps() {
        let effective = r#"{"schemaVersion":2,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"implementationVariant":"candidate","experimentId":"candidate-01","transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":500000,"gopSeconds":2,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqMode":"spatial","aqStrength":8,"vbvFrames":1,"lookaheadDepth":0,"weightedPrediction":false}}"#;
        let path = temp_profile(effective);
        assert!(load_profile(&path, 1_000_000).is_some());
        fs::remove_file(path).ok();

        let below_floor = effective.replace("500000", "499999");
        let path = temp_profile(&below_floor);
        assert!(load_profile(&path, 1_000_000).is_none());
        fs::remove_file(path).ok();
    }

    #[test]
    fn expiry_only_renewal_is_the_same_effective_profile() {
        let path = temp_profile(
            r#"{"schemaVersion":1,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":4000000,"gopSeconds":2,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqStrength":8,"temporalAq":false,"vbvFrames":1}}"#,
        );
        let first = load_profile(&path, 1_000_000).unwrap();
        let mut renewed = first.clone();
        renewed.expires_at_ms += 300_000;
        assert_eq!(first, renewed);
        renewed.apply_revision += 1;
        assert_ne!(first, renewed);
        renewed.apply_revision = first.apply_revision;
        renewed.gop_seconds = 4;
        assert_ne!(first, renewed);
        fs::remove_file(path).ok();
    }

    #[test]
    fn expiration_or_disappearance_clears_the_active_lease() {
        let path = temp_profile(
            r#"{"schemaVersion":1,"runId":"123e4567-e89b-42d3-a456-426614174000","expiresAtMs":1300000,"transport":"wss","codec":"hevc","streamMode":"work","adaptive":false,"width":1920,"height":1080,"fps":30,"bitrateBps":4000000,"encoder":{"preset":"p3","tuning":"ultra-low-latency","multipass":"disabled","aqStrength":8,"temporalAq":false,"vbvFrames":1}}"#,
        );
        let mut lease = BenchmarkLease::at(path.clone());
        assert!(lease.refresh(1_000_000).is_some());
        assert!(lease.refresh(1_300_000).is_none());
        assert!(lease.active().is_none());
        fs::remove_file(&path).ok();
        assert!(lease.refresh(1_000_000).is_none());
    }
}
