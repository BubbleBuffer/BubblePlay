//! Signaling client for the BubblePlay Cloudflare Worker relay.
//!
//! Protocol (JSON text frames over one WebSocket):
//! - host -> worker: {type:"hello", token, role:"host"}
//! - worker -> host: {type:"hello-ok"} | {type:"error", message}
//! - relayed: {type:"offer", sdp} (client -> host via worker),
//!   {type:"answer", sdp} (host -> client),
//!   {type:"ice", candidate} (both directions),
//!   {type:"peer-left"},
//!   {type:"ping"} (keepalive — consumed, never relayed)
//!
//! The socket runs non-blocking; call `poll()` once per loop iteration.

use crate::capture::MonitorInfo;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::windows::io::{AsRawSocket, RawSocket};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::{header::AUTHORIZATION, HeaderName, HeaderValue, Request};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{client_tls_with_config, Error as WsError, Message, WebSocket};

const SIGNALING_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(45);
const WORKER_VERSION_PROBE_INTERVAL: Duration = Duration::from_secs(15);
const BENCH_STATUS_MAX_BYTES: usize = 4_096;
const FILE_DRAG_PREVIEW_MAX_BYTES: usize = 8_192;

/// Client-side drag metadata for the native OLE target probe. No file bytes or
/// paths travel in this control; the host checks epoch and desktop readiness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDragPreviewControl {
    pub protocol: u8,
    pub epoch: String,
    pub drag_id: u32,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop: Option<crate::file_transfer::DropPoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<String>>,
}

impl FileDragPreviewControl {
    pub fn valid(&self) -> bool {
        if self.protocol != 1
            || self.drag_id == 0
            || self.epoch.len() != 32
            || !self
                .epoch
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return false;
        }
        match self.action.as_str() {
            "cancel" => self.drop.is_none() && self.types.is_none(),
            "update" => {
                self.drop.is_some_and(|point| point.generation != 0)
                    && self.types.as_ref().is_some_and(|types| {
                        (1..=64).contains(&types.len())
                            && types.iter().all(|mime| {
                                mime.len() <= 96 && mime.bytes().all(|b| b.is_ascii_graphic())
                            })
                    })
            }
            _ => false,
        }
    }
}

fn valid_file_drag_preview_wire(text: &str, control: &FileDragPreviewControl) -> bool {
    if text.len() > FILE_DRAG_PREVIEW_MAX_BYTES || !control.valid() {
        return false;
    }
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(text) else {
        return false;
    };
    if fields.keys().any(|key| {
        ![
            "type", "protocol", "epoch", "dragId", "action", "drop", "types",
        ]
        .contains(&key.as_str())
    }) {
        return false;
    }
    if control.action == "cancel" {
        return !fields.contains_key("drop") && !fields.contains_key("types");
    }
    fields
        .get("drop")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|point| {
            point.len() == 4
                && point
                    .keys()
                    .all(|key| ["x", "y", "generation", "viewportRevision"].contains(&key.as_str()))
        })
}

const V2_REDUNDANT_BENCH_FIELDS: [&str; 12] = [
    "transport",
    "preset",
    "tuning",
    "multipass",
    "aqStrength",
    "temporalAq",
    "aqMode",
    "vbvFrames",
    "lookaheadDepth",
    "weightedPrediction",
    "dirtyQpMap",
    "main10",
];

/// Every host build injects set-1 scan codes and syncs text clipboard (the
/// browser still disables clipboard on the secure desktop).
fn input_capabilities_message() -> SignalMsg {
    SignalMsg::InputCapabilities {
        protocol: 1,
        scancode: true,
        clipboard: true,
        clipboard_max_bytes: crate::clipboard::MAX_CLIPBOARD_BYTES as u32,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchStatusEncoder {
    pub preset: String,
    pub tuning: String,
    pub multipass: String,
    pub aq_strength: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporal_aq: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aq_mode: Option<String>,
    pub vbv_frames: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lookahead_depth: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weighted_prediction: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty_qp_map: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main10: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roi_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub async_depth: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_control_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_quality: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_bitrate_multiplier: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchStatusEffective {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implementation_variant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experiment_id: Option<String>,
    pub transport: String,
    pub codec: String,
    pub stream_mode: String,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_bps: u32,
    pub gop_seconds: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hybrid_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hybrid_cache_mode: Option<String>,
    pub encoder: BenchStatusEncoder,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BenchRoiActivity {
    pub mode: String,
    pub evaluated_frames: u64,
    pub mapped_frames: u64,
    pub missing_damage_frames: u64,
    pub empty_damage_frames: u64,
    pub idr_frames: u64,
    pub coverage_rejected_frames: u64,
    pub unsupported_frames: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub temporal_frames: u64,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub temporal_priming_frames: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis_p95_us: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis_max_us: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconstruction_active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_submitted_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_ready_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_late_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_skipped_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_adjusted_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_warmup_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_motion_bypasses: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_missing_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_eligible_ctbs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_refined_ctbs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_funded_ctbs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_spend_headroom_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_net_zero_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_wire_occupancy_p95_bps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_wire_occupancy_max_bps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_submit_p95_us: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_submit_max_us: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_readback_p95_us: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_readback_max_us: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_feedback_sequence: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_feedback_age_frames: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_feedback_budget_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_feedback_wire_occupancy_bps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sequence: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_changed_ctbs: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_total_ctbs: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_min_delta: Option<i8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_max_delta: Option<i8>,
}

impl BenchRoiActivity {
    pub fn feedback() -> Self {
        Self {
            mode: "feedback".into(),
            reconstruction_active: Some(false),
            feedback_submitted_frames: Some(0),
            feedback_ready_frames: Some(0),
            feedback_late_frames: Some(0),
            feedback_skipped_frames: Some(0),
            feedback_adjusted_frames: Some(0),
            feedback_warmup_frames: Some(0),
            feedback_motion_bypasses: Some(0),
            feedback_missing_frames: Some(0),
            feedback_eligible_ctbs: Some(0),
            feedback_refined_ctbs: Some(0),
            feedback_funded_ctbs: Some(0),
            feedback_submit_p95_us: Some(0),
            feedback_submit_max_us: Some(0),
            feedback_readback_p95_us: Some(0),
            feedback_readback_max_us: Some(0),
            ..Self::default()
        }
    }

    pub fn feedback_headroom() -> Self {
        let mut activity = Self::feedback();
        activity.mode = "feedback-headroom".into();
        activity.feedback_spend_headroom_frames = Some(0);
        activity.feedback_net_zero_frames = Some(0);
        activity.feedback_wire_occupancy_p95_bps = Some(0);
        activity.feedback_wire_occupancy_max_bps = Some(0);
        activity
    }
}

fn u64_is_zero(value: &u64) -> bool {
    *value == 0
}

fn u32_is_zero(value: &u32) -> bool {
    *value == 0
}

#[allow(clippy::too_many_arguments)]
fn host_capabilities_message(
    service_mode: bool,
    cold_boot: bool,
    secure_desktop: bool,
    secure_text: bool,
    file_transfer: bool,
    file_drop: bool,
    follow_window: Option<bool>,
    browser_approval_eligible: bool,
) -> SignalMsg {
    SignalMsg::HostCapabilities {
        browser_approval: !service_mode && browser_approval_eligible,
        service_mode,
        cold_boot,
        secure_desktop,
        secure_text,
        file_transfer: file_transfer.then_some(crate::file_transfer::PROTOCOL_VERSION),
        file_drop: (!service_mode && file_transfer && file_drop).then_some(1),
        file_drop_preview: (!service_mode && file_transfer && file_drop).then_some(1),
        follow_window: follow_window.map(|_| crate::follow_window::PROTOCOL_VERSION),
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn bench_status_wire(msg: &SignalMsg) -> Result<String> {
    let mut value = serde_json::to_value(msg).context("serialize benchmark status value")?;
    let protocol = value.get("protocol").and_then(serde_json::Value::as_u64);
    if protocol == Some(2) {
        let object = value
            .as_object_mut()
            .context("benchmark status must serialize as an object")?;
        for field in V2_REDUNDANT_BENCH_FIELDS {
            object.remove(field);
        }
        // Experiment profiles admit only one independent pipeline axis. Hybrid activity is the
        // evidence required for the quiescent-detail axis; the per-stage NVENC breakdown belongs
        // to the resource-mode axis and otherwise pushes realistic status windows over the fixed
        // signaling limit.
        let feedback_activity = object
            .get("roiActivity")
            .and_then(serde_json::Value::as_object)
            .and_then(|activity| activity.get("mode"))
            .and_then(serde_json::Value::as_str)
            == Some("feedback");
        if object.get("hybridActivity").is_some() || feedback_activity {
            object.remove("encodeStages");
        }
        if object.get("hybridActivity").is_some() {
            if let Some(hybrid) = object
                .get_mut("hybridActivity")
                .and_then(serde_json::Value::as_object_mut)
            {
                let mut cache = serde_json::Map::new();
                for (field, compact) in [
                    ("suppressions", "s"),
                    ("reactivations", "r"),
                    ("permanentInvalidations", "i"),
                    ("staleReactivationRejects", "x"),
                    ("cachedBytesRetained", "b"),
                    ("pngBytesAvoided", "a"),
                ] {
                    if let Some(field_value) = hybrid.remove(field) {
                        cache.insert(compact.into(), field_value);
                    }
                }
                hybrid.insert("cache".into(), serde_json::Value::Object(cache));
            }
        }
        let mut temporal_roi = serde_json::Map::new();
        if let Some(activity) = object
            .get_mut("roiActivity")
            .and_then(serde_json::Value::as_object_mut)
        {
            for field in [
                "source",
                "temporalFrames",
                "temporalPrimingFrames",
                "analysisP95Us",
                "analysisMaxUs",
            ] {
                if let Some(field_value) = activity.remove(field) {
                    temporal_roi.insert(field.into(), field_value);
                }
            }
        }
        if !temporal_roi.is_empty() {
            let metrics = object
                .entry("metrics")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("benchmark status metrics must serialize as an object")?;
            metrics.insert(
                "temporalRoi".into(),
                serde_json::Value::Object(temporal_roi),
            );
        }
    }
    let wire = serde_json::to_string(&value).context("serialize benchmark status wire")?;
    if wire.len() > BENCH_STATUS_MAX_BYTES {
        bail!(
            "benchmark status exceeds {BENCH_STATUS_MAX_BYTES}-byte signaling limit: {} bytes",
            wire.len()
        );
    }
    Ok(wire)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BenchStageTiming {
    pub count: u64,
    pub total_us: u64,
    pub max_us: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BenchEncodeStages {
    pub frames: u64,
    pub register: BenchStageTiming,
    pub map: BenchStageTiming,
    pub submit: BenchStageTiming,
    pub lock: BenchStageTiming,
    pub copy: BenchStageTiming,
    pub unmap: BenchStageTiming,
    pub unregister: BenchStageTiming,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BenchHybridActivity {
    pub mode: String,
    pub frames: u64,
    pub history_resets: u64,
    pub history_preserved_frames: u64,
    pub motion_bypasses: u64,
    pub crop_remaps: u64,
    pub cursor_neutral_frames: u64,
    pub candidates: u64,
    pub reissued_tiles: u64,
    pub upserts: u64,
    pub invalidations: u64,
    #[serde(default)]
    pub suppressions: u64,
    #[serde(default)]
    pub reactivations: u64,
    #[serde(default)]
    pub permanent_invalidations: u64,
    #[serde(default)]
    pub stale_reactivation_rejects: u64,
    #[serde(default)]
    pub cached_bytes_retained: u64,
    #[serde(default)]
    pub png_bytes_avoided: u64,
    pub budget_skipped_upserts: u64,
    pub png_bytes: u64,
    pub png_us_total: u64,
    pub png_us_max: u32,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub hevc_tile_bytes: u64,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub hevc_tile_copy_us_total: u64,
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub hevc_tile_copy_us_max: u32,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub hevc_tile_encode_us_total: u64,
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub hevc_tile_encode_us_max: u32,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub hevc_tile_encoder_opens: u64,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub hevc_tile_encoder_failures: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis_p95_us: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis_max_us: Option<u32>,
    pub validation_frames: u64,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub validation_unavailable_frames: u64,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub validation_failed_frames: u64,
    #[serde(default)]
    pub deferred_validation_ready_frames: u64,
    #[serde(default)]
    pub deferred_validation_pending_frames: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred_validation_age_p95_frames: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred_validation_age_max_frames: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred_validation_pending_max_age_frames: Option<u32>,
    #[serde(default)]
    pub damage_audit_frames: u64,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub damage_audit_metadata_unavailable_frames: u64,
    #[serde(default)]
    pub damage_audit_changed_ctbs: u64,
    #[serde(default)]
    pub damage_audit_covered_ctbs: u64,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub damage_audit_missed_ctbs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_submit_p95_us: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_submit_max_us: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_collect_p95_us: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_collect_max_us: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_validation_sequence: Option<u32>,
    pub sidecar_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[allow(clippy::large_enum_variant)]
pub enum SignalMsg {
    Hello {
        token: String,
        role: String,
    },
    #[serde(rename = "hello-ok")]
    HelloOk {
        #[serde(rename = "workerVersionId", default)]
        worker_version_id: Option<String>,
    },
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    Ice {
        candidate: String,
    },
    Diag {
        seq: u64,
        #[serde(rename = "atMs")]
        at_ms: u64,
        profile: serde_json::Value,
    },
    #[serde(rename = "client-log")]
    ClientLog {
        seq: u64,
        #[serde(rename = "atMs")]
        at_ms: u64,
        level: String,
        event: String,
        detail: serde_json::Value,
    },
    Qos {
        #[serde(rename = "sessionId")]
        session_id: String,
        seq: u64,
        #[serde(rename = "atMs")]
        at_ms: u64,
        #[serde(default)]
        rx: QosRx,
        #[serde(default)]
        presentation: QosPresentation,
    },
    FileDragPreview {
        #[serde(flatten)]
        control: FileDragPreviewControl,
    },
    Quality {
        profile: String,
        width: u32,
        height: u32,
        fps: u32,
        #[serde(rename = "targetBitrateBps")]
        target_bitrate_bps: u32,
        #[serde(rename = "bweBps")]
        bwe_bps: Option<u32>,
        reason: String,
        #[serde(rename = "streamMode", skip_serializing_if = "Option::is_none")]
        stream_mode: Option<String>,
    },
    #[serde(rename = "bench-status")]
    BenchStatus {
        protocol: u8,
        #[serde(rename = "profileRevision")]
        profile_revision: u64,
        #[serde(rename = "runId")]
        run_id: String,
        sequence: u64,
        phase: String,
        #[serde(
            rename = "implementationVariant",
            skip_serializing_if = "Option::is_none"
        )]
        implementation_variant: Option<String>,
        #[serde(rename = "experimentId", skip_serializing_if = "Option::is_none")]
        experiment_id: Option<String>,
        transport: String,
        preset: String,
        tuning: String,
        multipass: String,
        #[serde(rename = "aqStrength")]
        aq_strength: u8,
        #[serde(rename = "temporalAq", skip_serializing_if = "Option::is_none")]
        temporal_aq: Option<bool>,
        #[serde(rename = "aqMode", skip_serializing_if = "Option::is_none")]
        aq_mode: Option<String>,
        #[serde(rename = "vbvFrames")]
        vbv_frames: u8,
        #[serde(rename = "lookaheadDepth", skip_serializing_if = "Option::is_none")]
        lookahead_depth: Option<u8>,
        #[serde(rename = "weightedPrediction", skip_serializing_if = "Option::is_none")]
        weighted_prediction: Option<bool>,
        #[serde(rename = "dirtyQpMap", skip_serializing_if = "Option::is_none")]
        dirty_qp_map: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        main10: Option<bool>,
        effective: BenchStatusEffective,
        #[serde(rename = "windowDurationMs")]
        window_duration_ms: u64,
        #[serde(rename = "encodedBytes")]
        encoded_bytes: u64,
        #[serde(rename = "wssPayloadBytes")]
        wss_payload_bytes: u64,
        #[serde(rename = "encodeCount")]
        encode_count: u64,
        #[serde(rename = "encodeUsTotal")]
        encode_us_total: u64,
        #[serde(rename = "encodeUsMax")]
        encode_us_max: u32,
        #[serde(rename = "idrCount")]
        idr_count: u64,
        #[serde(rename = "deltaCount")]
        delta_count: u64,
        skips: u64,
        #[serde(rename = "qpMapFrames")]
        qp_map_frames: u64,
        #[serde(rename = "qpMapChangedCtbs")]
        qp_map_changed_ctbs: u64,
        #[serde(rename = "qpMapTotalCtbs")]
        qp_map_total_ctbs: u64,
        #[serde(rename = "roiActivity", skip_serializing_if = "Option::is_none")]
        roi_activity: Option<BenchRoiActivity>,
        #[serde(rename = "encodeStages", skip_serializing_if = "Option::is_none")]
        encode_stages: Option<BenchEncodeStages>,
        #[serde(rename = "hybridActivity", skip_serializing_if = "Option::is_none")]
        hybrid_activity: Option<BenchHybridActivity>,
    },
    #[serde(rename = "media-view")]
    MediaView {
        state: String,
        revision: u32,
        generation: u32,
        #[serde(rename = "desktopWidth")]
        desktop_width: u32,
        #[serde(rename = "desktopHeight")]
        desktop_height: u32,
        #[serde(rename = "sourceX")]
        source_x: u32,
        #[serde(rename = "sourceY")]
        source_y: u32,
        #[serde(rename = "sourceWidth")]
        source_width: u32,
        #[serde(rename = "sourceHeight")]
        source_height: u32,
        reason: String,
    },
    #[serde(rename = "desktop-state")]
    DesktopState {
        state: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        desktop: Option<String>,
        #[serde(rename = "secureKind", skip_serializing_if = "Option::is_none")]
        secure_kind: Option<String>,
        #[serde(rename = "secureCapabilities", skip_serializing_if = "Option::is_none")]
        secure_capabilities: Option<Vec<String>>,
        #[serde(rename = "input", skip_serializing_if = "Option::is_none")]
        input_state: Option<String>,
        generation: u32,
        reason: String,
    },
    #[serde(rename = "input-status")]
    InputStatus {
        protocol: u8,
        generation: u32,
        #[serde(rename = "receivedPackets")]
        received_packets: u64,
        #[serde(rename = "acceptedPackets")]
        accepted_packets: u64,
        #[serde(rename = "failedPackets")]
        failed_packets: u64,
        #[serde(rename = "droppedPackets")]
        dropped_packets: u64,
        #[serde(rename = "releasedInputs")]
        released_inputs: u64,
        #[serde(rename = "requestedEvents")]
        requested_events: u64,
        #[serde(rename = "appliedEvents")]
        applied_events: u64,
        #[serde(rename = "lastErrorCode", skip_serializing_if = "Option::is_none")]
        last_error_code: Option<i32>,
        #[serde(rename = "lastSequence", skip_serializing_if = "Option::is_none")]
        last_sequence: Option<u16>,
        /// Omitted on receipt-only messages. Routing announcements explicitly
        /// send null to clear an earlier warning.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<crate::input::InputStatusReason>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_input_route"
        )]
        route: Option<crate::input::InputRoute>,
    },
    #[serde(rename = "secure-action")]
    SecureAction {
        action: String,
    },
    #[serde(rename = "secure-action-result")]
    SecureActionResult {
        action: String,
        ok: bool,
        reason: String,
    },
    #[serde(rename = "host-capabilities")]
    HostCapabilities {
        #[serde(rename = "browserApproval", default, skip_serializing_if = "is_false")]
        browser_approval: bool,
        #[serde(rename = "serviceMode")]
        service_mode: bool,
        #[serde(rename = "coldBoot")]
        cold_boot: bool,
        #[serde(rename = "secureDesktop")]
        secure_desktop: bool,
        #[serde(rename = "secureText")]
        secure_text: bool,
        /// File transfer protocol version (`docs/architecture/file-transfer.md`);
        /// absent when the host cannot transfer files.
        #[serde(
            rename = "fileTransfer",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        file_transfer: Option<u8>,
        /// Native Windows virtual-file drops; absent on older/service hosts.
        #[serde(rename = "fileDrop", default, skip_serializing_if = "Option::is_none")]
        file_drop: Option<u8>,
        /// Native OLE pre-drop target preview; absent on older/service hosts.
        #[serde(
            rename = "fileDropPreview",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        file_drop_preview: Option<u8>,
        /// Follow active window version (`docs/architecture/follow-active-window.md`);
        /// absent when the host cannot follow the foreground window.
        #[serde(
            rename = "followWindow",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        follow_window: Option<u8>,
    },
    #[serde(rename = "monitor-list")]
    MonitorList {
        monitors: Vec<MonitorInfo>,
        /// The host's follow state; absent when follow is not advertised.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        follow: Option<bool>,
    },
    /// Input features beyond the original VK packet set. Sent separately from
    /// `host-capabilities` so older browsers simply ignore it.
    #[serde(rename = "input-capabilities")]
    InputCapabilities {
        protocol: u8,
        scancode: bool,
        clipboard: bool,
        #[serde(rename = "clipboardMaxBytes")]
        clipboard_max_bytes: u32,
    },
    /// One in-order chunk of host clipboard text for the browser.
    #[serde(rename = "clipboard")]
    Clipboard {
        protocol: u8,
        id: u32,
        index: u16,
        count: u16,
        text: String,
    },
    #[serde(rename = "clipboard-image")]
    ClipboardImage {
        protocol: u8,
        id: u32,
        index: u16,
        count: u16,
        text: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    #[serde(rename = "monitor-error")]
    MonitorError {
        selector: String,
        reason: String,
    },
    #[serde(rename = "first-frame")]
    FirstFrame {
        protocol: u8,
        phase: String,
        #[serde(rename = "requestId")]
        request_id: String,
        #[serde(default)]
        transport: Option<String>,
        #[serde(default)]
        epoch: Option<String>,
        #[serde(default)]
        generation: Option<u32>,
        #[serde(default)]
        sequence: Option<u32>,
        #[serde(default)]
        codec: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    },
    #[serde(rename = "media-control")]
    MediaControl {
        action: String,
        #[serde(rename = "requestId", default)]
        request_id: Option<String>,
        #[serde(rename = "streamMode", default)]
        stream_mode: Option<String>,
        #[serde(default)]
        codec: Option<String>,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        protocol: Option<u8>,
        #[serde(default)]
        epoch: Option<String>,
        #[serde(default)]
        quality: Option<String>,
        #[serde(default)]
        revision: Option<u32>,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(rename = "xQ16", default)]
        x_q16: Option<u16>,
        #[serde(rename = "yQ16", default)]
        y_q16: Option<u16>,
        #[serde(rename = "widthQ16", default)]
        width_q16: Option<u16>,
        #[serde(rename = "heightQ16", default)]
        height_q16: Option<u16>,
        #[serde(default)]
        monitor: Option<String>,
        /// `action: "follow"`: turn follow active window on or off.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        follow: Option<bool>,
    },
    #[serde(rename = "peer-left")]
    PeerLeft,
    Ping,
    Pong,
    Error {
        message: String,
    },
    Bye,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QosRx {
    pub mbps: Option<f64>,
    #[serde(rename = "lossPct")]
    pub loss_pct: Option<f64>,
    #[serde(rename = "rttMs")]
    pub rtt_ms: Option<f64>,
    #[serde(rename = "jitterMs")]
    pub jitter_ms: Option<f64>,
    #[serde(rename = "jitterBufferMs")]
    pub jitter_buffer_ms: Option<f64>,
    #[serde(rename = "nackDelta")]
    pub nack_delta: Option<u64>,
    #[serde(rename = "pliDelta")]
    pub pli_delta: Option<u64>,
    #[serde(rename = "packetsReceivedDelta")]
    pub packets_received_delta: Option<u64>,
    #[serde(rename = "framesReceivedDelta")]
    pub frames_received_delta: Option<u64>,
    #[serde(rename = "framesDecodedDelta")]
    pub frames_decoded_delta: Option<u64>,
    #[serde(rename = "framesDroppedDelta")]
    pub frames_dropped_delta: Option<u64>,
    #[serde(rename = "freezeDelta")]
    pub freeze_delta: Option<u64>,
    #[serde(rename = "qpPerFrame")]
    pub qp_per_frame: Option<f64>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QosPresentation {
    pub fps: Option<f64>,
    #[serde(rename = "recentGapMs")]
    pub recent_gap_ms: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

pub enum SignalEvent {
    SecureDeny,
    /// Worker accepted our hello.
    Registered,
    /// Browser sent an SDP offer.
    Offer(String),
    /// Browser trickled an ICE candidate (SDP string form).
    RemoteCandidate(String),
    FileDragPreview(FileDragPreviewControl),
    /// Authenticated browser profiling snapshot (enabled with ?profile=1).
    ClientDiagnostics {
        seq: u64,
        at_ms: u64,
        profile: serde_json::Value,
    },
    ClientLog {
        seq: u64,
        at_ms: u64,
        level: String,
        event: String,
        detail: serde_json::Value,
    },
    Qos {
        seq: u64,
        at_ms: u64,
        rx: QosRx,
        presentation: QosPresentation,
    },
    MediaControl {
        action: String,
        request_id: Option<String>,
        stream_mode: Option<String>,
        codec: Option<String>,
        reason: Option<String>,
        protocol: Option<u8>,
        epoch: Option<String>,
        quality: Option<String>,
        revision: Option<u32>,
        enabled: Option<bool>,
        x_q16: Option<u16>,
        y_q16: Option<u16>,
        width_q16: Option<u16>,
        height_q16: Option<u16>,
        monitor: Option<String>,
        follow: Option<bool>,
    },
    MediaView {
        state: String,
        revision: u32,
        generation: u32,
        desktop_width: u32,
        desktop_height: u32,
        source_x: u32,
        source_y: u32,
        source_width: u32,
        source_height: u32,
        reason: String,
    },
    FirstFrame {
        protocol: u8,
        phase: String,
        request_id: String,
        transport: Option<String>,
        epoch: Option<String>,
        generation: Option<u32>,
        sequence: Option<u32>,
        codec: Option<String>,
        reason: Option<String>,
    },
    InputData(Vec<u8>),
    /// The client left (or the relay told us so).
    PeerLeft,
}

#[derive(Default)]
struct PendingCursorState {
    latest: Option<[u8; 8]>,
    // Keep the acknowledgement watermark across slot drainage. External
    // cursor polls carry the latest position, not a new input watermark.
    // A new authenticated signaling session starts with no watermark.
    acknowledged_sequence: Option<u16>,
}

#[cfg(test)]
mod pending_cursor_state_tests {
    use super::PendingCursorState;
    use crate::input::CursorState;

    fn snapshot(acknowledged_sequence: Option<u16>, x: u16) -> [u8; 8] {
        CursorState {
            acknowledged_sequence,
            x,
            y: x + 1,
            in_bounds: true,
            visible: true,
        }
        .encode()
    }

    #[test]
    fn routine_cursor_feedback_keeps_only_the_latest_before_admission() {
        let mut pending = PendingCursorState::default();
        assert_eq!(pending.publish(snapshot(None, 1), false), None);
        assert_eq!(pending.publish(snapshot(None, 2), false), None);
        assert_eq!(pending.latest.take(), Some(snapshot(None, 2)));
        assert_eq!(pending.latest, None);
    }

    #[test]
    fn reliable_responses_are_distinct_and_supersede_older_routine_state() {
        let mut pending = PendingCursorState::default();
        pending.publish(snapshot(Some(1), 1), false);
        assert_eq!(
            pending.publish(snapshot(Some(2), 2), true),
            Some(snapshot(Some(2), 2))
        );
        assert_eq!(pending.latest, None);
        assert_eq!(
            pending.publish(snapshot(Some(3), 3), true),
            Some(snapshot(Some(3), 3))
        );
        pending.publish(snapshot(None, 4), false);
        assert_eq!(pending.latest, Some(snapshot(Some(3), 4)));
    }

    #[test]
    fn ackless_latest_snapshot_keeps_final_ack_and_its_own_visibility() {
        let mut pending = PendingCursorState::default();
        pending.publish(snapshot(Some(42), 1), false);
        let external = CursorState {
            acknowledged_sequence: None,
            x: 300,
            y: 400,
            in_bounds: false,
            visible: false,
        };
        pending.publish(external.encode(), false);
        assert_eq!(
            pending.latest.take(),
            Some(
                CursorState {
                    acknowledged_sequence: Some(42),
                    ..external
                }
                .encode()
            )
        );
    }

    #[test]
    fn stale_ack_does_not_lower_watermark_in_pending_or_after_slot_drainage() {
        let mut pending = PendingCursorState::default();
        pending.publish(snapshot(Some(100), 1), false);
        pending.publish(snapshot(Some(99), 2), false);
        assert_eq!(pending.latest.take(), Some(snapshot(Some(100), 2)));
        pending.publish(snapshot(None, 3), false);
        assert_eq!(pending.latest.take(), Some(snapshot(Some(100), 3)));
        pending.publish(snapshot(Some(90), 4), false);
        assert_eq!(pending.latest.take(), Some(snapshot(Some(100), 4)));
        // Explicit responses also preserve the watermark without being
        // coalesced together or resurrecting the drained position slot.
        assert_eq!(
            pending.publish(snapshot(Some(98), 5), true),
            Some(snapshot(Some(100), 5))
        );
        assert_eq!(pending.latest, None);
    }

    #[test]
    fn ack_watermark_wraps_and_rejects_duplicates_stale_and_half_range() {
        let mut pending = PendingCursorState::default();
        pending.publish(snapshot(Some(u16::MAX), 1), false);
        pending.publish(snapshot(Some(0), 2), false);
        pending.publish(snapshot(Some(u16::MAX), 3), false);
        pending.publish(snapshot(Some(0), 4), false);
        pending.publish(snapshot(Some(0x8000), 5), false);
        assert_eq!(pending.latest.take(), Some(snapshot(Some(0), 5)));
        pending.publish(snapshot(Some(1), 6), false);
        assert_eq!(pending.latest.take(), Some(snapshot(Some(1), 6)));
    }

    #[test]
    fn new_signaling_session_starts_without_previous_ack_watermark() {
        let mut previous = PendingCursorState::default();
        previous.publish(snapshot(Some(100), 1), false);
        let mut fresh = PendingCursorState::default();
        fresh.publish(snapshot(None, 2), false);
        assert_eq!(fresh.latest.take(), Some(snapshot(None, 2)));
        fresh.publish(snapshot(Some(1), 3), false);
        assert_eq!(fresh.latest.take(), Some(snapshot(Some(1), 3)));
    }

    #[test]
    fn warm_signaling_socket_resets_ack_and_unsent_position_for_new_viewer() {
        let mut pending = PendingCursorState::default();
        pending.publish(snapshot(Some(24_000), 1), false);
        pending.reset();
        assert_eq!(pending.latest, None);
        assert_eq!(pending.acknowledged_sequence, None);
        assert_eq!(
            pending.publish(snapshot(Some(0), 2), true),
            Some(snapshot(Some(0), 2))
        );
    }
}

fn valid_secure_action_wire(text: &str) -> bool {
    if text.len() > 512 {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    value.as_object().is_some_and(|value| {
        value.len() == 2
            && value.get("type") == Some(&serde_json::json!("secure-action"))
            && value.get("action") == Some(&serde_json::json!("deny"))
    })
}

impl PendingCursorState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn publish(&mut self, mut state: [u8; 8], reliable: bool) -> Option<[u8; 8]> {
        if state[7] & crate::input::CURSOR_ACK_VALID != 0 {
            let sequence = u16::from_le_bytes([state[1], state[2]]);
            if crate::input::newer_sequence(self.acknowledged_sequence, sequence) {
                self.acknowledged_sequence = Some(sequence);
            }
        }
        // Coordinates and visibility always use the newest snapshot, while
        // ACK-less, duplicate or stale snapshots cannot erase/lower its ACK.
        if let Some(sequence) = self.acknowledged_sequence {
            state[1..3].copy_from_slice(&sequence.to_le_bytes());
            state[7] |= crate::input::CURSOR_ACK_VALID;
        }
        if reliable {
            // This newer authoritative response supersedes an unsent routine
            // state, but each reliable response itself is sent distinctly.
            self.latest = None;
            Some(state)
        } else {
            self.latest = Some(state);
            None
        }
    }
}

pub struct Signaling {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    pending_cursor_state: PendingCursorState,
    registered: bool,
    peer_seen: bool,
    last_keepalive_ack: Instant,
    version_monitor: Option<WorkerVersionMonitor>,
    /// Advertised as `fileTransfer: 1` in host-capabilities.
    file_transfer: bool,
    file_drop: bool,
    /// `None` when follow active window is unsupported (not advertised);
    /// otherwise the session's follow state, sent in monitor-list.
    follow_window: Option<bool>,
    browser_approval: crate::browser_approval::BrowserApproval,
}

struct WorkerVersionMonitor {
    stop: Sender<()>,
    changes: Receiver<(String, String)>,
}

impl WorkerVersionMonitor {
    fn spawn(
        url: &str,
        token: &str,
        host_gate_token: Option<&str>,
        connected_version: Option<String>,
    ) -> Result<Option<Self>> {
        if worker_version_probe_url(url)?.is_none() {
            return Ok(None);
        }
        let url = url.to_string();
        let token = token.to_string();
        let host_gate_token = host_gate_token.map(str::to_string);
        let (stop_tx, stop_rx) = mpsc::channel();
        let (change_tx, change_rx) = mpsc::channel();
        thread::Builder::new()
            .name("bubbleplay-worker-version".into())
            .spawn(move || {
                let known = connected_version.filter(|value| canonical_worker_version_id(value));
                loop {
                    match stop_rx.recv_timeout(WORKER_VERSION_PROBE_INTERVAL) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let current =
                        match probe_worker_version(&url, &token, host_gate_token.as_deref()) {
                            Ok(Some(current)) => current,
                            Ok(None) | Err(_) => continue,
                        };
                    if worker_version_changed(known.as_deref(), Some(&current)) {
                        let connected = known.unwrap_or_else(|| "unversioned".to_string());
                        let _ = change_tx.send((connected, current));
                        break;
                    }
                }
            })
            .context("spawn Worker version monitor")?;
        Ok(Some(Self {
            stop: stop_tx,
            changes: change_rx,
        }))
    }

    fn changed(&self) -> Option<(String, String)> {
        self.changes.try_recv().ok()
    }
}

impl Drop for WorkerVersionMonitor {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

fn deserialize_input_route<'de, D>(
    deserializer: D,
) -> Result<Option<crate::input::InputRoute>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    crate::input::InputRoute::deserialize(deserializer).map(Some)
}

fn input_routing_status_message(
    generation: u32,
    status: crate::input::InputRoutingStatus,
) -> serde_json::Value {
    let mut message = serde_json::to_value(SignalMsg::InputStatus {
        protocol: 1,
        generation,
        received_packets: 0,
        accepted_packets: 0,
        failed_packets: 0,
        dropped_packets: 0,
        released_inputs: 0,
        requested_events: 0,
        applied_events: 0,
        last_error_code: None,
        last_sequence: None,
        reason: status.reason,
        route: Some(status.route),
    })
    .expect("input status serializes");
    if status.reason.is_none() {
        message["reason"] = serde_json::Value::Null;
    }
    message
}

impl Signaling {
    /// Connect to the worker (wss://...), upgrade to WS, register as host.
    pub fn connect(url: &str, token: &str, host_gate_token: Option<&str>) -> Result<Self> {
        info!(url, "connecting to signaling worker");
        let request = signaling_request(url, host_gate_token)?;
        let (mut ws, response) = connect_with_timeout(request)?;
        debug!(status = %response.status(), "ws handshake done");

        // Send hello, then wait (blocking, one-time) for hello-ok.
        let hello = SignalMsg::Hello {
            token: token.to_string(),
            role: "host".to_string(),
        };
        ws.send(Message::text(serde_json::to_string(&hello)?))
            .context("send hello")?;

        let worker_version_id = loop {
            let msg = ws.read().context("read hello-ack")?;
            let Message::Text(text) = msg else { continue };
            let parsed: SignalMsg =
                serde_json::from_str(&text).with_context(|| format!("bad hello ack: {text}"))?;
            match parsed {
                SignalMsg::HelloOk { worker_version_id } => break worker_version_id,
                SignalMsg::Error { message } => bail!("worker rejected hello: {message}"),
                other => bail!("unexpected message while registering: {other:?}"),
            }
        };

        let version_monitor =
            WorkerVersionMonitor::spawn(url, token, host_gate_token, worker_version_id.clone())?;

        // Flip the underlying TCP stream to non-blocking for the run loop.
        set_nonblocking(&mut ws)?;
        info!("signaling registered as host");
        Ok(Self {
            ws,
            pending_cursor_state: PendingCursorState::default(),
            registered: true,
            peer_seen: false,
            last_keepalive_ack: Instant::now(),
            version_monitor,
            file_transfer: false,
            file_drop: false,
            follow_window: None,
            browser_approval: Default::default(),
        })
    }

    /// Advertise file transfer in later host-capabilities messages.
    pub fn set_file_transfer(&mut self, enabled: bool) {
        self.file_transfer = enabled;
    }

    pub fn set_file_drop(&mut self, enabled: bool) {
        self.file_drop = enabled;
    }

    /// Advertise follow active window (`followWindow: 1`) in later
    /// host-capabilities messages; follow starts off.
    pub fn set_follow_window_supported(&mut self, supported: bool) {
        self.follow_window = supported.then_some(false);
    }

    /// Record the session's follow state for later monitor-list messages.
    /// Ignored when follow is not advertised.
    pub fn set_follow(&mut self, follow: bool) {
        if let Some(state) = self.follow_window.as_mut() {
            *state = follow;
        }
    }

    /// Send an already-serialized message (e.g. `file-offer`).
    pub fn send_text(&mut self, text: &str) {
        if self.ws.send(Message::text(text)).is_ok() {
            let _ = self.ws.flush();
        }
    }

    pub fn registered(&self) -> bool {
        self.registered
    }

    /// Readiness wakes the producer for signaling and WSS input immediately.
    pub fn read_socket(&self) -> RawSocket {
        match self.ws.get_ref() {
            MaybeTlsStream::Plain(stream) => stream.as_raw_socket(),
            MaybeTlsStream::NativeTls(stream) => stream.get_ref().as_raw_socket(),
            _ => unreachable!("unsupported signaling TLS stream"),
        }
    }

    /// Drain pending messages without blocking. Call once per main-loop tick.
    pub fn poll(&mut self) -> Result<Vec<SignalEvent>> {
        self.flush_cursor_state();
        if let Some((connected, current)) = self
            .version_monitor
            .as_ref()
            .and_then(WorkerVersionMonitor::changed)
        {
            self.registered = false;
            self.browser_approval.disconnect();
            bail!("signaling Worker version changed from {connected} to {current}");
        }
        let mut events = Vec::new();
        loop {
            let msg = match self.ws.read() {
                Ok(m) => m,
                Err(WsError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // TLS may surface "want read" as WouldBlock via native-tls too.
                Err(e) => {
                    self.registered = false;
                    self.browser_approval.disconnect();
                    return Err(e).context("ws read");
                }
            };
            match msg {
                Message::Text(text) => {
                    if let Some(decisions) = self
                        .browser_approval
                        .handle_wire(&text, crate::browser_approval::now_ms())
                    {
                        self.send_approval_decisions(decisions);
                        continue;
                    }
                    let parsed: SignalMsg = match serde_json::from_str(&text) {
                        Ok(m) => m,
                        Err(e) => {
                            warn!(%e, bytes = text.len(), "ignoring unparsable signal message");
                            continue;
                        }
                    };
                    match parsed {
                        SignalMsg::Offer { sdp } => {
                            self.peer_seen = true;
                            events.push(SignalEvent::Offer(sdp));
                        }
                        SignalMsg::Ice { candidate } => {
                            self.peer_seen = true;
                            events.push(SignalEvent::RemoteCandidate(candidate));
                        }
                        SignalMsg::Diag {
                            seq,
                            at_ms,
                            profile,
                        } => events.push(SignalEvent::ClientDiagnostics {
                            seq,
                            at_ms,
                            profile,
                        }),
                        SignalMsg::ClientLog {
                            seq,
                            at_ms,
                            level,
                            event,
                            detail,
                        } => {
                            events.push(SignalEvent::ClientLog {
                                seq,
                                at_ms,
                                level,
                                event,
                                detail,
                            });
                        }
                        SignalMsg::Qos {
                            seq,
                            at_ms,
                            rx,
                            presentation,
                            ..
                        } => events.push(SignalEvent::Qos {
                            seq,
                            at_ms,
                            rx,
                            presentation,
                        }),
                        SignalMsg::FileDragPreview { control } => {
                            if valid_file_drag_preview_wire(&text, &control) {
                                events.push(SignalEvent::FileDragPreview(control));
                            }
                        }
                        SignalMsg::SecureAction { action } => {
                            if action == "deny" && valid_secure_action_wire(&text) {
                                events.push(SignalEvent::SecureDeny);
                            }
                        }
                        SignalMsg::MediaControl {
                            action,
                            request_id,
                            stream_mode,
                            codec,
                            reason,
                            protocol,
                            epoch,
                            quality,
                            revision,
                            enabled,
                            x_q16,
                            y_q16,
                            width_q16,
                            height_q16,
                            monitor,
                            follow,
                        } => {
                            events.push(SignalEvent::MediaControl {
                                action,
                                request_id,
                                stream_mode,
                                codec,
                                reason,
                                protocol,
                                epoch,
                                quality,
                                revision,
                                enabled,
                                x_q16,
                                y_q16,
                                width_q16,
                                height_q16,
                                monitor,
                                follow,
                            });
                        }
                        SignalMsg::MediaView {
                            state,
                            revision,
                            generation,
                            desktop_width,
                            desktop_height,
                            source_x,
                            source_y,
                            source_width,
                            source_height,
                            reason,
                        } => {
                            events.push(SignalEvent::MediaView {
                                state,
                                revision,
                                generation,
                                desktop_width,
                                desktop_height,
                                source_x,
                                source_y,
                                source_width,
                                source_height,
                                reason,
                            });
                        }
                        SignalMsg::FirstFrame {
                            protocol,
                            phase,
                            request_id,
                            transport,
                            epoch,
                            generation,
                            sequence,
                            codec,
                            reason,
                        } => {
                            events.push(SignalEvent::FirstFrame {
                                protocol,
                                phase,
                                request_id,
                                transport,
                                epoch,
                                generation,
                                sequence,
                                codec,
                                reason,
                            });
                        }
                        SignalMsg::PeerLeft => events.push(SignalEvent::PeerLeft),
                        SignalMsg::Ping => {} // backward-compatible worker/client keepalive
                        SignalMsg::Pong => self.last_keepalive_ack = Instant::now(),
                        SignalMsg::HelloOk { .. } => {} // duplicate ack, ignore
                        SignalMsg::Error { message } => {
                            warn!(message, "worker error message");
                        }
                        other => {
                            debug!(kind = ?std::mem::discriminant(&other), "ignoring signal message")
                        }
                    }
                }
                Message::Ping(_) | Message::Pong(_) => {} // tungstenite auto-pongs
                Message::Close(_) => {
                    self.registered = false;
                    self.browser_approval.disconnect();
                    events.push(SignalEvent::PeerLeft);
                    break;
                }
                Message::Binary(data) if data.len() <= 16_384 => {
                    events.push(SignalEvent::InputData(data.to_vec()));
                }
                _ => {}
            }
        }
        if keepalive_expired(self.last_keepalive_ack, Instant::now()) {
            self.registered = false;
            self.browser_approval.disconnect();
            bail!("signaling keepalive acknowledgement timed out");
        }
        if self.registered {
            let decisions = self
                .browser_approval
                .poll(crate::browser_approval::now_ms());
            self.send_approval_decisions(decisions);
        }
        Ok(events)
    }

    fn send_approval_decisions(
        &mut self,
        decisions: Vec<crate::browser_approval::ApprovalDecision>,
    ) {
        for decision in decisions {
            if let Ok(text) = serde_json::to_string(&decision) {
                self.send_text(&text);
            }
        }
    }

    pub fn send_answer(&mut self, sdp: &str) -> Result<()> {
        let msg = SignalMsg::Answer {
            sdp: sdp.to_string(),
        };
        self.ws
            .send(Message::text(serde_json::to_string(&msg)?))
            .context("send answer")?;
        self.ws.flush().ok();
        Ok(())
    }

    /// Trickle a local candidate to the browser (SDP string form).
    pub fn send_candidate(&mut self, candidate: &str) -> Result<()> {
        let msg = SignalMsg::Ice {
            candidate: candidate.to_string(),
        };
        self.ws
            .send(Message::text(serde_json::to_string(&msg)?))
            .context("send ice")?;
        self.ws.flush().ok();
        Ok(())
    }

    pub fn send_bye(&mut self) {
        let _ = self.ws.send(Message::text(
            serde_json::to_string(&SignalMsg::Bye).unwrap_or_default(),
        ));
        let _ = self.ws.flush();
    }

    /// Send an application-level keepalive so the relay's WebSocket stays
    /// warm on both sides (the worker consumes pings, never relays them).
    pub fn send_ping(&mut self) {
        let msg = SignalMsg::Ping;
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            self.ws.flush().ok();
        }
    }

    pub fn send_binary(&mut self, data: &[u8]) {
        if data.len() > 16_384 {
            return;
        }
        if self.ws.send(Message::Binary(data.to_vec().into())).is_ok() {
            self.ws.flush().ok();
        }
    }

    /// Routine feedback has one replaceable slot before socket admission.
    /// Explicit cursor-sync/button responses retain the existing reliable path.
    pub fn send_cursor_state(&mut self, state: [u8; 8], reliable: bool) {
        if let Some(state) = self.pending_cursor_state.publish(state, reliable) {
            self.send_binary(&state);
        }
    }

    /// The signaling socket may outlive its viewer. Its retained ACK belongs
    /// to the old input producer and must not overwrite a fresh client's ACK.
    pub fn reset_viewer_cursor_state(&mut self) {
        self.pending_cursor_state.reset();
    }

    pub fn flush_cursor_state(&mut self) {
        // Tungstenite retains an already admitted frame on WouldBlock. Drain
        // it before admitting another routine state, bounding that backlog.
        if self.ws.flush().is_err() {
            return;
        }
        let Some(state) = self.pending_cursor_state.latest.take() else {
            return;
        };
        if let Err(WsError::WriteBufferFull(_)) =
            self.ws.send(Message::Binary(state.to_vec().into()))
        {
            // WriteBufferFull is the only send result that did not admit it.
            self.pending_cursor_state.latest = Some(state);
        }
    }

    /// Preserve feedback at an Agent input-status barrier. The Agent emits
    /// such a barrier immediately after each explicit button/sync response.
    pub fn flush_cursor_state_reliable(&mut self) {
        if let Some(state) = self.pending_cursor_state.latest.take() {
            self.send_binary(&state);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn send_quality(
        &mut self,
        profile: &str,
        width: u32,
        height: u32,
        fps: u32,
        target_bitrate_bps: u32,
        bwe_bps: Option<u32>,
        reason: &str,
        stream_mode: &str,
    ) {
        let msg = SignalMsg::Quality {
            profile: profile.into(),
            width,
            height,
            fps,
            target_bitrate_bps,
            bwe_bps,
            reason: reason.into(),
            stream_mode: Some(stream_mode.into()),
        };
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            self.ws.flush().ok();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn send_bench_status(
        &mut self,
        protocol: u8,
        run_id: &str,
        sequence: u64,
        profile_revision: u64,
        phase: &str,
        implementation_variant: Option<&str>,
        experiment_id: Option<&str>,
        transport: &str,
        codec: &str,
        stream_mode: &str,
        width: u16,
        height: u16,
        fps: u16,
        bitrate_bps: u32,
        gop_seconds: u8,
        preset: &str,
        tuning: &str,
        multipass: &str,
        aq_strength: u8,
        aq_mode: &str,
        vbv_frames: u8,
        rate_control_mode: &str,
        target_quality: Option<u8>,
        peak_bitrate_multiplier: Option<u8>,
        lookahead_depth: u8,
        weighted_prediction: bool,
        dirty_qp_map: bool,
        main10: bool,
        roi_mode: &str,
        resource_mode: &str,
        async_depth: u8,
        hybrid_mode: &str,
        hybrid_cache_mode: &str,
        window_duration_ms: u64,
        encoded_bytes: u64,
        wss_payload_bytes: u64,
        encode_count: u64,
        encode_us_total: u64,
        encode_us_max: u32,
        idr_count: u64,
        delta_count: u64,
        skips: u64,
        qp_map_frames: u64,
        qp_map_changed_ctbs: u64,
        qp_map_total_ctbs: u64,
        roi_activity: Option<BenchRoiActivity>,
        encode_stages: Option<BenchEncodeStages>,
        hybrid_activity: Option<BenchHybridActivity>,
    ) {
        let msg = SignalMsg::BenchStatus {
            protocol,
            profile_revision,
            run_id: run_id.into(),
            sequence,
            phase: phase.into(),
            implementation_variant: implementation_variant.map(str::to_owned),
            experiment_id: experiment_id.map(str::to_owned),
            transport: transport.into(),
            preset: preset.into(),
            tuning: tuning.into(),
            multipass: multipass.into(),
            aq_strength,
            temporal_aq: (protocol == 1).then_some(aq_mode == "temporal"),
            aq_mode: (protocol == 2).then(|| aq_mode.to_owned()),
            vbv_frames,
            lookahead_depth: (protocol == 2).then_some(lookahead_depth),
            weighted_prediction: (protocol == 2).then_some(weighted_prediction),
            dirty_qp_map: (protocol == 2).then_some(dirty_qp_map),
            main10: (protocol == 2).then_some(main10),
            effective: BenchStatusEffective {
                implementation_variant: implementation_variant.map(str::to_owned),
                experiment_id: experiment_id.map(str::to_owned),
                transport: transport.into(),
                codec: codec.into(),
                stream_mode: stream_mode.into(),
                width,
                height,
                fps,
                bitrate_bps,
                gop_seconds,
                hybrid_mode: (protocol == 2).then(|| hybrid_mode.to_owned()),
                hybrid_cache_mode: (protocol == 2).then(|| hybrid_cache_mode.to_owned()),
                encoder: BenchStatusEncoder {
                    preset: preset.into(),
                    tuning: tuning.into(),
                    multipass: multipass.into(),
                    aq_strength,
                    temporal_aq: (protocol == 1).then_some(aq_mode == "temporal"),
                    aq_mode: (protocol == 2).then(|| aq_mode.to_owned()),
                    vbv_frames,
                    rate_control_mode: (protocol == 2 && rate_control_mode != "cbr")
                        .then(|| rate_control_mode.to_owned()),
                    target_quality: (protocol == 2).then_some(target_quality).flatten(),
                    peak_bitrate_multiplier: (protocol == 2)
                        .then_some(peak_bitrate_multiplier)
                        .flatten(),
                    lookahead_depth: (protocol == 2).then_some(lookahead_depth),
                    weighted_prediction: (protocol == 2).then_some(weighted_prediction),
                    dirty_qp_map: (protocol == 2).then_some(dirty_qp_map),
                    main10: (protocol == 2).then_some(main10),
                    roi_mode: (protocol == 2).then(|| roi_mode.to_owned()),
                    resource_mode: (protocol == 2).then(|| resource_mode.to_owned()),
                    async_depth: (protocol == 2).then_some(async_depth),
                },
            },
            window_duration_ms,
            encoded_bytes,
            wss_payload_bytes,
            encode_count,
            encode_us_total,
            encode_us_max,
            idr_count,
            delta_count,
            skips,
            qp_map_frames,
            qp_map_changed_ctbs,
            qp_map_total_ctbs,
            roi_activity,
            encode_stages,
            hybrid_activity,
        };
        let wire = match bench_status_wire(&msg) {
            Ok(wire) => wire,
            Err(error) => {
                warn!(?error, "benchmark status serialization rejected");
                return;
            }
        };
        debug!(
            sequence,
            profile_revision,
            bytes = wire.len(),
            wire = %wire,
            "benchmark status wire"
        );
        if self.ws.send(Message::text(wire)).is_ok() {
            self.ws.flush().ok();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn send_media_view(
        &mut self,
        state: &str,
        revision: u32,
        generation: u32,
        desktop_width: u32,
        desktop_height: u32,
        source_x: u32,
        source_y: u32,
        source_width: u32,
        source_height: u32,
        reason: &str,
    ) {
        let msg = SignalMsg::MediaView {
            state: state.into(),
            revision,
            generation,
            desktop_width,
            desktop_height,
            source_x,
            source_y,
            source_width,
            source_height,
            reason: reason.into(),
        };
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            self.ws.flush().ok();
        }
    }

    pub fn send_desktop_state(&mut self, state: &str, generation: u32, reason: &str) {
        self.send_desktop_state_kind(state, None, None, generation, reason);
    }

    pub fn send_desktop_state_kind(
        &mut self,
        state: &str,
        desktop: Option<&str>,
        input_state: Option<&str>,
        generation: u32,
        reason: &str,
    ) {
        let msg = SignalMsg::DesktopState {
            state: state.into(),
            desktop: desktop.map(str::to_owned),
            secure_kind: None,
            secure_capabilities: None,
            input_state: input_state.map(str::to_owned),
            generation,
            reason: reason.into(),
        };
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            let _ = self.ws.flush();
        }
    }

    pub fn send_secure_desktop_state(
        &mut self,
        kind: &str,
        capabilities: &[&str],
        generation: u32,
    ) {
        self.send_secure_desktop_state_status(false, kind, capabilities, generation);
    }

    pub fn send_secure_desktop_state_status(
        &mut self,
        active: bool,
        kind: &str,
        capabilities: &[&str],
        generation: u32,
    ) {
        let msg = SignalMsg::DesktopState {
            state: if active { "active" } else { "unavailable" }.into(),
            desktop: Some("secure".into()),
            secure_kind: Some(kind.into()),
            secure_capabilities: Some(
                capabilities
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect(),
            ),
            input_state: Some(
                if active && capabilities.contains(&"input") {
                    "enabled"
                } else if active {
                    "read-only"
                } else {
                    "suspended"
                }
                .into(),
            ),
            generation,
            reason: if active {
                "desktop-switched"
            } else {
                "secure-desktop"
            }
            .into(),
        };
        if let Ok(text) = serde_json::to_string(&msg) {
            if self.ws.send(Message::text(text)).is_ok() {
                let _ = self.ws.flush();
            }
        }
    }

    pub fn send_secure_action_result(&mut self, ok: bool, reason: &str) {
        let message = SignalMsg::SecureActionResult {
            action: "deny".into(),
            ok,
            reason: reason.into(),
        };
        if let Ok(text) = serde_json::to_string(&message) {
            self.send_text(&text);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn send_input_status(
        &mut self,
        generation: u32,
        received_packets: u64,
        accepted_packets: u64,
        failed_packets: u64,
        dropped_packets: u64,
        released_inputs: u64,
        requested_events: u64,
        applied_events: u64,
        last_error_code: Option<i32>,
        last_sequence: Option<u16>,
    ) {
        let msg = SignalMsg::InputStatus {
            protocol: 1,
            generation,
            received_packets,
            accepted_packets,
            failed_packets,
            dropped_packets,
            released_inputs,
            requested_events,
            applied_events,
            last_error_code,
            last_sequence,
            reason: None,
            route: None,
        };
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            let _ = self.ws.flush();
        }
    }

    /// An additive input-status announcement. Existing receipt-only messages
    /// omit reason/route and cannot clear a foreground warning.
    pub fn send_input_routing_status(
        &mut self,
        generation: u32,
        status: crate::input::InputRoutingStatus,
    ) {
        self.send_text(&input_routing_status_message(generation, status).to_string());
    }

    pub fn send_host_capabilities(
        &mut self,
        service_mode: bool,
        cold_boot: bool,
        secure_desktop: bool,
        secure_text: bool,
    ) {
        let msg = host_capabilities_message(
            service_mode,
            cold_boot,
            secure_desktop,
            secure_text,
            self.file_transfer,
            self.file_drop,
            self.follow_window,
            crate::browser_approval::capability_available(service_mode),
        );
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            let _ = self.ws.flush();
        }
        self.send_input_capabilities();
    }

    fn send_input_capabilities(&mut self) {
        let msg = input_capabilities_message();
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            let _ = self.ws.flush();
        }
    }

    /// Send one host clipboard chunk (see `clipboard::chunk_text`).
    pub fn send_clipboard_chunk(&mut self, id: u32, index: u16, count: u16, text: &str) {
        self.send_clipboard_content_chunk(id, index, count, text, false);
    }

    pub fn send_clipboard_content_chunk(
        &mut self,
        id: u32,
        index: u16,
        count: u16,
        text: &str,
        png: bool,
    ) {
        let msg = if png {
            SignalMsg::ClipboardImage {
                protocol: 1,
                id,
                index,
                count,
                text: text.to_owned(),
                mime_type: "image/png".to_owned(),
            }
        } else {
            SignalMsg::Clipboard {
                protocol: 1,
                id,
                index,
                count,
                text: text.to_owned(),
            }
        };
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            let _ = self.ws.flush();
        }
    }

    pub fn send_monitor_list(&mut self, monitors: Vec<MonitorInfo>) {
        let msg = SignalMsg::MonitorList {
            monitors,
            follow: self.follow_window,
        };
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            let _ = self.ws.flush();
        }
    }

    pub fn send_monitor_error(&mut self, selector: &str, reason: &str) {
        let msg = SignalMsg::MonitorError {
            selector: selector.into(),
            reason: reason.into(),
        };
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            let _ = self.ws.flush();
        }
    }

    pub fn send_first_frame(
        &mut self,
        phase: &str,
        request_id: &str,
        generation: u32,
        transport: Option<&str>,
        epoch: Option<&str>,
        codec: Option<&str>,
    ) {
        let msg = SignalMsg::FirstFrame {
            protocol: 1,
            phase: phase.into(),
            request_id: request_id.into(),
            transport: transport.map(str::to_owned),
            epoch: epoch.map(str::to_owned),
            generation: Some(generation),
            sequence: None,
            codec: codec.map(str::to_owned),
            reason: None,
        };
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            let _ = self.ws.flush();
        }
    }

    pub fn send_first_frame_failed(
        &mut self,
        request_id: &str,
        generation: u32,
        transport: Option<&str>,
        epoch: Option<&str>,
        codec: Option<&str>,
        reason: &str,
    ) {
        let msg = SignalMsg::FirstFrame {
            protocol: 1,
            phase: "failed".into(),
            request_id: request_id.into(),
            transport: transport.map(str::to_owned),
            epoch: epoch.map(str::to_owned),
            generation: Some(generation),
            sequence: None,
            codec: codec.map(str::to_owned),
            reason: Some(reason.chars().take(96).collect()),
        };
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            let _ = self.ws.flush();
        }
    }

    pub fn send_lab_quality_keyframe_admitted(
        &mut self,
        request_id: &str,
        generation: u32,
        sequence: u32,
        codec: &str,
    ) {
        let msg = SignalMsg::FirstFrame {
            protocol: 1,
            phase: "quality-keyframe-admitted".into(),
            request_id: request_id.into(),
            transport: Some("wss".into()),
            epoch: None,
            generation: Some(generation),
            sequence: Some(sequence),
            codec: Some(codec.into()),
            reason: Some("stream-lab-quality-capture".into()),
        };
        if self
            .ws
            .send(Message::text(
                serde_json::to_string(&msg).unwrap_or_default(),
            ))
            .is_ok()
        {
            let _ = self.ws.flush();
        }
    }
}

fn signaling_request(url: &str, host_gate_token: Option<&str>) -> Result<Request<()>> {
    let worker_name = std::env::var("BUBBLEPLAY_LAB_WORKER_NAME")
        .ok()
        .filter(|value| !value.is_empty());
    let version_id = std::env::var("BUBBLEPLAY_LAB_WORKER_VERSION_ID")
        .ok()
        .filter(|value| !value.is_empty());
    let worker_version = match (worker_name.as_deref(), version_id.as_deref()) {
        (None, None) => None,
        (Some(worker_name), Some(version_id)) => Some((worker_name, version_id)),
        _ => bail!(
            "Stream Lab Worker version override requires both worker name and version identifier"
        ),
    };
    signaling_request_with_worker_version(url, host_gate_token, worker_version)
}

fn signaling_request_with_worker_version(
    url: &str,
    host_gate_token: Option<&str>,
    worker_version: Option<(&str, &str)>,
) -> Result<Request<()>> {
    crate::relay_url::require_secure_relay_url(url)?;
    let mut request = url
        .into_client_request()
        .context("build websocket request")?;
    if let Some(token) = host_gate_token.filter(|value| !value.is_empty()) {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("host gate token is not a valid HTTP header value")?;
        request.headers_mut().insert(AUTHORIZATION, value);
    }
    if request.uri().path() == "/lab-host-ws" {
        if let Some((worker_name, version_id)) = worker_version {
            if !canonical_worker_name(worker_name) || !canonical_worker_version_id(version_id) {
                bail!("Stream Lab Worker version override is not canonical");
            }
            let value = HeaderValue::from_str(&format!("{worker_name}=\"{version_id}\""))
                .context("Stream Lab Worker version override is not a valid HTTP header value")?;
            request.headers_mut().insert(
                HeaderName::from_static("cloudflare-workers-version-overrides"),
                value,
            );
        }
    }
    Ok(request)
}

fn canonical_worker_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn canonical_worker_version_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
        && matches!(bytes[14], b'1'..=b'5')
        && matches!(bytes[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b')
}

fn worker_version_changed(connected: Option<&str>, current: Option<&str>) -> bool {
    match (connected, current) {
        (Some(connected), Some(current)) => {
            canonical_worker_version_id(connected)
                && canonical_worker_version_id(current)
                && connected != current
        }
        (None, Some(current)) => canonical_worker_version_id(current),
        _ => false,
    }
}

fn worker_version_probe_url(url: &str) -> Result<Option<String>> {
    let request = url
        .into_client_request()
        .context("build Worker version probe URL")?;
    let uri = request.uri();
    if uri.path() != "/host-ws" {
        return Ok(None);
    }
    if uri.scheme_str() != Some("wss") {
        bail!("Worker version probe requires a wss signaling URL");
    }
    let authority = uri
        .authority()
        .context("Worker version probe URL has no authority")?;
    Ok(Some(format!("wss://{authority}/host-version-ws")))
}

fn probe_worker_version(
    url: &str,
    token: &str,
    host_gate_token: Option<&str>,
) -> Result<Option<String>> {
    let Some(probe_url) = worker_version_probe_url(url)? else {
        return Ok(None);
    };
    let request = signaling_request(&probe_url, host_gate_token)?;
    let (mut ws, _) = connect_with_timeout(request)?;
    ws.send(Message::text(serde_json::to_string(&SignalMsg::Hello {
        token: token.to_string(),
        role: "version-probe".to_string(),
    })?))
    .context("send Worker version probe hello")?;
    loop {
        let message = ws.read().context("read Worker version probe")?;
        match message {
            Message::Text(text) => match serde_json::from_str::<SignalMsg>(&text)
                .with_context(|| format!("bad Worker version probe response: {text}"))?
            {
                SignalMsg::HelloOk { worker_version_id } => {
                    let _ = ws.close(None);
                    return Ok(worker_version_id.filter(|value| canonical_worker_version_id(value)));
                }
                SignalMsg::Error { message } => {
                    bail!("Worker rejected version probe: {message}")
                }
                other => bail!("unexpected Worker version probe response: {other:?}"),
            },
            Message::Close(_) => bail!("Worker version probe closed before hello-ok"),
            _ => {}
        }
    }
}

fn connect_with_timeout(
    request: Request<()>,
) -> Result<(
    WebSocket<MaybeTlsStream<TcpStream>>,
    tungstenite::handshake::client::Response,
)> {
    let host = request
        .uri()
        .host()
        .context("signaling URL has no host")?
        .trim_matches(['[', ']'])
        .to_string();
    let port = request.uri().port_u16().unwrap_or_else(|| {
        if request.uri().scheme_str() == Some("ws") {
            80
        } else {
            443
        }
    });
    let addresses = (host.as_str(), port)
        .to_socket_addrs()
        .context("resolve signaling worker")?;
    let mut last_error = None;
    let mut tcp = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, SIGNALING_HANDSHAKE_TIMEOUT) {
            Ok(stream) => {
                tcp = Some(stream);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let tcp = tcp.ok_or_else(|| {
        anyhow::anyhow!(
            "connect signaling worker: {}",
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "DNS returned no addresses".to_string())
        )
    })?;
    tcp.set_nodelay(true).context("set signaling TCP_NODELAY")?;
    tcp.set_read_timeout(Some(SIGNALING_HANDSHAKE_TIMEOUT))
        .context("set signaling handshake read timeout")?;
    tcp.set_write_timeout(Some(SIGNALING_HANDSHAKE_TIMEOUT))
        .context("set signaling handshake write timeout")?;
    client_tls_with_config(request, tcp, None, None)
        .map_err(|error| anyhow::anyhow!("websocket handshake failed: {error:?}"))
}

fn keepalive_expired(last_ack: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_ack) > KEEPALIVE_TIMEOUT
}

fn set_nonblocking(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> Result<()> {
    let stream = ws.get_mut();
    let tcp: &TcpStream = match stream {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::NativeTls(t) => t.get_ref(),
        other => bail!("unsupported TLS stream variant: {other:?}"),
    };
    tcp.set_read_timeout(None)
        .context("clear signaling read timeout")?;
    tcp.set_write_timeout(None)
        .context("clear signaling write timeout")?;
    tcp.set_nonblocking(true).context("set_nonblocking")?;
    // Give the socket a moment to settle (TLS session state already established
    // during the blocking handshake; reads are now poll-driven).
    std::thread::sleep(Duration::from_millis(0));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        bench_status_wire, keepalive_expired, signaling_request,
        signaling_request_with_worker_version, worker_version_changed, worker_version_probe_url,
        BenchEncodeStages, BenchHybridActivity, BenchRoiActivity, BenchStageTiming,
        BenchStatusEffective, BenchStatusEncoder, SignalMsg, BENCH_STATUS_MAX_BYTES,
        KEEPALIVE_TIMEOUT, V2_REDUNDANT_BENCH_FIELDS,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn signaling_keepalive_expires_only_after_the_bounded_silence_window() {
        let acknowledged = Instant::now();
        assert!(!keepalive_expired(
            acknowledged,
            acknowledged + KEEPALIVE_TIMEOUT
        ));
        assert!(keepalive_expired(
            acknowledged,
            acknowledged + KEEPALIVE_TIMEOUT + Duration::from_millis(1)
        ));
    }

    #[test]
    fn signaling_pong_has_a_distinct_wire_shape() {
        assert_eq!(
            serde_json::to_string(&SignalMsg::Pong).unwrap(),
            r#"{"type":"pong"}"#
        );
    }

    #[test]
    fn feedback_roi_activity_serializes_a_complete_inactive_capability_window() {
        let activity = BenchRoiActivity::feedback();
        let value = serde_json::to_value(activity).unwrap();
        assert_eq!(value["mode"], "feedback");
        assert_eq!(value["reconstructionActive"], false);
        for field in [
            "feedbackSubmittedFrames",
            "feedbackReadyFrames",
            "feedbackLateFrames",
            "feedbackSkippedFrames",
            "feedbackAdjustedFrames",
            "feedbackWarmupFrames",
            "feedbackMotionBypasses",
            "feedbackMissingFrames",
            "feedbackEligibleCtbs",
            "feedbackRefinedCtbs",
            "feedbackFundedCtbs",
            "feedbackSubmitP95Us",
            "feedbackSubmitMaxUs",
            "feedbackReadbackP95Us",
            "feedbackReadbackMaxUs",
        ] {
            assert_eq!(value[field], 0, "missing feedback field {field}");
        }
        assert!(value.get("lastFeedbackSequence").is_none());
        assert!(value.get("lastFeedbackAgeFrames").is_none());
    }

    #[test]
    fn worker_version_hello_is_optional_and_detects_only_a_real_cutover() {
        let parsed: SignalMsg = serde_json::from_str(
            r#"{"type":"hello-ok","workerVersionId":"2fae2311-36ac-41fa-8d8c-8607007c99cc"}"#,
        )
        .unwrap();
        let SignalMsg::HelloOk { worker_version_id } = parsed else {
            panic!("expected hello-ok");
        };
        assert_eq!(
            worker_version_id.as_deref(),
            Some("2fae2311-36ac-41fa-8d8c-8607007c99cc")
        );
        assert!(worker_version_changed(
            worker_version_id.as_deref(),
            Some("26909381-9df3-4a13-80b6-999f83786df8")
        ));
        assert!(!worker_version_changed(
            worker_version_id.as_deref(),
            worker_version_id.as_deref()
        ));
        assert!(!worker_version_changed(worker_version_id.as_deref(), None));
        assert!(worker_version_changed(None, worker_version_id.as_deref()));
    }

    #[test]
    fn production_worker_version_probe_uses_a_separate_non_registering_socket() {
        assert_eq!(
            worker_version_probe_url("wss://play.example/host-ws").unwrap(),
            Some("wss://play.example/host-version-ws".to_string())
        );
        assert_eq!(
            worker_version_probe_url("wss://play.example/lab-host-ws?run=abc").unwrap(),
            None
        );
        assert_eq!(
            worker_version_probe_url("ws://127.0.0.1:8787/lab-host-ws?run=abc").unwrap(),
            None
        );
        assert!(worker_version_probe_url("https://play.example/host-ws").is_err());
    }

    #[test]
    fn benchmark_status_uses_the_bounded_optional_wire_contract() {
        let json = serde_json::to_value(SignalMsg::BenchStatus {
            protocol: 1,
            profile_revision: 7,
            run_id: "123e4567-e89b-42d3-a456-426614174000".into(),
            sequence: 9,
            phase: "sample".into(),
            implementation_variant: None,
            experiment_id: None,
            transport: "wss".into(),
            preset: "p5".into(),
            tuning: "low-latency".into(),
            multipass: "quarter".into(),
            aq_strength: 10,
            temporal_aq: Some(true),
            aq_mode: None,
            vbv_frames: 4,
            lookahead_depth: None,
            weighted_prediction: None,
            dirty_qp_map: None,
            main10: None,
            effective: BenchStatusEffective {
                implementation_variant: None,
                experiment_id: None,
                transport: "wss".into(),
                codec: "hevc".into(),
                stream_mode: "work".into(),
                width: 1920,
                height: 1080,
                fps: 30,
                bitrate_bps: 4_000_000,
                gop_seconds: 2,
                hybrid_mode: None,
                hybrid_cache_mode: None,
                encoder: BenchStatusEncoder {
                    preset: "p5".into(),
                    tuning: "low-latency".into(),
                    multipass: "quarter".into(),
                    aq_strength: 10,
                    temporal_aq: Some(true),
                    aq_mode: None,
                    vbv_frames: 4,
                    lookahead_depth: None,
                    weighted_prediction: None,
                    dirty_qp_map: None,
                    main10: None,
                    roi_mode: None,
                    resource_mode: None,
                    async_depth: None,
                    rate_control_mode: None,
                    target_quality: None,
                    peak_bitrate_multiplier: None,
                },
            },
            window_duration_ms: 1_000,
            encoded_bytes: 100,
            wss_payload_bytes: 144,
            encode_count: 2,
            encode_us_total: 4_000,
            encode_us_max: 2_200,
            idr_count: 1,
            delta_count: 1,
            skips: 3,
            qp_map_frames: 0,
            qp_map_changed_ctbs: 0,
            qp_map_total_ctbs: 0,
            roi_activity: None,
            encode_stages: None,
            hybrid_activity: None,
        })
        .unwrap();
        assert_eq!(json["type"], "bench-status");
        assert_eq!(json["protocol"], 1);
        assert_eq!(json["profileRevision"], 7);
        assert_eq!(json["phase"], "sample");
        assert_eq!(json["runId"], "123e4567-e89b-42d3-a456-426614174000");
        assert_eq!(json["wssPayloadBytes"], 144);
        assert_eq!(json["windowDurationMs"], 1_000);
        assert_eq!(json["encodeUsMax"], 2_200);
        assert_eq!(json["effective"]["transport"], "wss");
        assert_eq!(json["effective"]["codec"], "hevc");
        assert_eq!(json["effective"]["streamMode"], "work");
        assert_eq!(json["effective"]["width"], 1920);
        assert_eq!(json["effective"]["height"], 1080);
        assert_eq!(json["effective"]["fps"], 30);
        assert_eq!(json["effective"]["bitrateBps"], 4_000_000);
        assert_eq!(json["effective"]["gopSeconds"], 2);
        assert_eq!(json["effective"]["encoder"]["preset"], "p5");
        assert_eq!(json["effective"]["encoder"]["tuning"], "low-latency");
        assert_eq!(json["effective"]["encoder"]["multipass"], "quarter");
        assert_eq!(json["effective"]["encoder"]["aqStrength"], 10);
        assert_eq!(json["effective"]["encoder"]["temporalAq"], true);
        assert_eq!(json["effective"]["encoder"]["vbvFrames"], 4);
        assert!(serde_json::to_vec(&json).unwrap().len() <= 2_048);
    }

    #[test]
    fn benchmark_status_encoder_echoes_external_budget_vbr_controls() {
        let json = serde_json::to_value(BenchStatusEncoder {
            preset: "p3".into(),
            tuning: "ultra-low-latency".into(),
            multipass: "disabled".into(),
            aq_strength: 8,
            temporal_aq: None,
            aq_mode: Some("spatial".into()),
            vbv_frames: 4,
            lookahead_depth: Some(0),
            weighted_prediction: Some(false),
            dirty_qp_map: Some(false),
            main10: Some(false),
            roi_mode: Some("off".into()),
            resource_mode: Some("per-frame".into()),
            async_depth: Some(0),
            rate_control_mode: Some("external-budget-vbr".into()),
            target_quality: Some(23),
            peak_bitrate_multiplier: Some(4),
        })
        .unwrap();
        assert_eq!(json["rateControlMode"], "external-budget-vbr");
        assert_eq!(json["targetQuality"], 23);
        assert_eq!(json["peakBitrateMultiplier"], 4);
    }

    #[test]
    fn benchmark_status_v2_reports_the_exact_candidate_encoder_contract() {
        let wire = bench_status_wire(&SignalMsg::BenchStatus {
            protocol: 2,
            profile_revision: 11,
            run_id: "123e4567-e89b-42d3-a456-426614174000".into(),
            sequence: 19,
            phase: "sample".into(),
            implementation_variant: Some("candidate".into()),
            experiment_id: Some("enc-work-001".into()),
            transport: "wss".into(),
            preset: "p3".into(),
            tuning: "ultra-low-latency".into(),
            multipass: "disabled".into(),
            aq_strength: 15,
            temporal_aq: None,
            aq_mode: Some("spatial".into()),
            vbv_frames: 4,
            lookahead_depth: Some(2),
            weighted_prediction: Some(true),
            dirty_qp_map: Some(true),
            main10: Some(false),
            effective: BenchStatusEffective {
                implementation_variant: Some("candidate".into()),
                experiment_id: Some("enc-work-001".into()),
                transport: "wss".into(),
                codec: "hevc".into(),
                stream_mode: "work".into(),
                width: 1920,
                height: 1080,
                fps: 30,
                bitrate_bps: 6_000_000,
                gop_seconds: 2,
                hybrid_mode: Some("off".into()),
                hybrid_cache_mode: Some("suppress-reactivate".into()),
                encoder: BenchStatusEncoder {
                    preset: "p3".into(),
                    tuning: "ultra-low-latency".into(),
                    multipass: "disabled".into(),
                    aq_strength: 15,
                    temporal_aq: None,
                    aq_mode: Some("spatial".into()),
                    vbv_frames: 4,
                    lookahead_depth: Some(2),
                    weighted_prediction: Some(true),
                    dirty_qp_map: Some(true),
                    main10: Some(false),
                    roi_mode: Some("damage".into()),
                    resource_mode: Some("per-frame".into()),
                    async_depth: Some(0),
                    rate_control_mode: None,
                    target_quality: None,
                    peak_bitrate_multiplier: None,
                },
            },
            window_duration_ms: 1_007,
            encoded_bytes: 9_876_543,
            wss_payload_bytes: 12_345_678,
            encode_count: 31,
            encode_us_total: 9_876_543,
            encode_us_max: 654_321,
            idr_count: 1,
            delta_count: 30,
            skips: 12_345,
            qp_map_frames: 29,
            qp_map_changed_ctbs: 59_160,
            qp_map_total_ctbs: 63_240,
            roi_activity: Some(BenchRoiActivity {
                mode: "damage".into(),
                evaluated_frames: 31,
                mapped_frames: 29,
                missing_damage_frames: 0,
                empty_damage_frames: 0,
                idr_frames: 1,
                coverage_rejected_frames: 1,
                unsupported_frames: 0,
                source: Some("gpu-temporal".into()),
                temporal_frames: 31,
                temporal_priming_frames: 1,
                analysis_p95_us: Some(1_499),
                analysis_max_us: Some(59_876),
                last_sequence: Some(u32::MAX),
                last_reason: Some("coverage-at-least-half".into()),
                last_changed_ctbs: Some(2_039),
                last_total_ctbs: Some(2_040),
                last_min_delta: Some(-3),
                last_max_delta: Some(1),
                ..BenchRoiActivity::default()
            }),
            encode_stages: Some(BenchEncodeStages {
                frames: 31,
                register: BenchStageTiming {
                    count: 31,
                    total_us: 1_234_567,
                    max_us: 98_765,
                },
                map: BenchStageTiming {
                    count: 31,
                    total_us: 1_234_567,
                    max_us: 98_765,
                },
                submit: BenchStageTiming {
                    count: 31,
                    total_us: 12_345_678,
                    max_us: 987_654,
                },
                lock: BenchStageTiming {
                    count: 31,
                    total_us: 12_345_678,
                    max_us: 987_654,
                },
                copy: BenchStageTiming {
                    count: 31,
                    total_us: 1_234_567,
                    max_us: 98_765,
                },
                unmap: BenchStageTiming {
                    count: 31,
                    total_us: 1_234_567,
                    max_us: 98_765,
                },
                unregister: BenchStageTiming {
                    count: 31,
                    total_us: 1_234_567,
                    max_us: 98_765,
                },
            }),
            hybrid_activity: None,
        })
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(json["implementationVariant"], "candidate");
        assert_eq!(json["experimentId"], "enc-work-001");
        for field in V2_REDUNDANT_BENCH_FIELDS {
            assert!(
                json.get(field).is_none(),
                "v2 wire should omit redundant top-level field {field}"
            );
        }
        assert_eq!(json["effective"]["implementationVariant"], "candidate");
        assert_eq!(json["effective"]["experimentId"], "enc-work-001");
        assert_eq!(json["effective"]["encoder"]["aqMode"], "spatial");
        assert_eq!(json["effective"]["encoder"]["lookaheadDepth"], 2);
        assert_eq!(json["effective"]["encoder"]["weightedPrediction"], true);
        assert_eq!(json["effective"]["encoder"]["dirtyQpMap"], true);
        assert_eq!(json["effective"]["encoder"]["roiMode"], "damage");
        assert_eq!(json["effective"]["encoder"]["resourceMode"], "per-frame");
        assert_eq!(json["effective"]["encoder"]["asyncDepth"], 0);
        assert_eq!(json["effective"]["hybridMode"], "off");
        assert_eq!(json["roiActivity"]["mode"], "damage");
        assert_eq!(json["roiActivity"]["lastSequence"], u32::MAX);
        for field in [
            "source",
            "temporalFrames",
            "temporalPrimingFrames",
            "analysisP95Us",
            "analysisMaxUs",
        ] {
            assert!(
                json["roiActivity"].get(field).is_none(),
                "v2 compatibility wire should move {field} out of roiActivity"
            );
        }
        assert_eq!(json["metrics"]["temporalRoi"]["source"], "gpu-temporal");
        assert_eq!(json["metrics"]["temporalRoi"]["temporalFrames"], 31);
        assert_eq!(json["metrics"]["temporalRoi"]["temporalPrimingFrames"], 1);
        assert_eq!(json["metrics"]["temporalRoi"]["analysisP95Us"], 1_499);
        assert_eq!(json["metrics"]["temporalRoi"]["analysisMaxUs"], 59_876);
        assert_eq!(json["encodeStages"]["register"]["count"], 31);
        assert_eq!(json["encodeStages"]["unregister"]["count"], 31);
        assert_eq!(json["qpMapFrames"], 29);
        assert!(json["effective"]["encoder"].get("temporalAq").is_none());
        let wire_len = wire.len();
        assert!(
            wire_len <= BENCH_STATUS_MAX_BYTES,
            "realistic populated v2 bench status is {wire_len} bytes"
        );
    }

    #[test]
    fn populated_feedback_headroom_status_fits_the_v10_signaling_contract() {
        let roi_activity = BenchRoiActivity {
            evaluated_frames: 31,
            mapped_frames: 29,
            missing_damage_frames: 1,
            empty_damage_frames: 0,
            idr_frames: 1,
            coverage_rejected_frames: 0,
            unsupported_frames: 0,
            source: Some("gpu-temporal".into()),
            temporal_frames: 31,
            temporal_priming_frames: 1,
            analysis_p95_us: Some(947),
            analysis_max_us: Some(1_234),
            reconstruction_active: Some(true),
            feedback_submitted_frames: Some(31),
            feedback_ready_frames: Some(30),
            feedback_late_frames: Some(0),
            feedback_skipped_frames: Some(1),
            feedback_adjusted_frames: Some(29),
            feedback_warmup_frames: Some(10),
            feedback_motion_bypasses: Some(0),
            feedback_missing_frames: Some(1),
            feedback_eligible_ctbs: Some(1_450),
            feedback_refined_ctbs: Some(145),
            feedback_funded_ctbs: Some(0),
            feedback_spend_headroom_frames: Some(21),
            feedback_net_zero_frames: Some(10),
            feedback_wire_occupancy_p95_bps: Some(5_412_345),
            feedback_wire_occupancy_max_bps: Some(5_987_654),
            feedback_submit_p95_us: Some(358),
            feedback_submit_max_us: Some(987),
            feedback_readback_p95_us: Some(411),
            feedback_readback_max_us: Some(1_101),
            last_feedback_sequence: Some(405),
            last_feedback_age_frames: Some(1),
            last_feedback_budget_policy: Some("spend-headroom".into()),
            last_feedback_wire_occupancy_bps: Some(5_123_456),
            last_sequence: Some(406),
            last_reason: Some("applied".into()),
            last_changed_ctbs: Some(50),
            last_total_ctbs: Some(2_040),
            last_min_delta: Some(-5),
            last_max_delta: Some(0),
            ..BenchRoiActivity::feedback_headroom()
        };
        let wire = bench_status_wire(&SignalMsg::BenchStatus {
            protocol: 2,
            profile_revision: 11,
            run_id: "123e4567-e89b-42d3-a456-426614174000".into(),
            sequence: 19,
            phase: "sample".into(),
            implementation_variant: Some("candidate".into()),
            experiment_id: Some("feedback-headroom".into()),
            transport: "wss".into(),
            preset: "p3".into(),
            tuning: "ultra-low-latency".into(),
            multipass: "disabled".into(),
            aq_strength: 8,
            temporal_aq: None,
            aq_mode: Some("spatial".into()),
            vbv_frames: 1,
            lookahead_depth: Some(0),
            weighted_prediction: Some(false),
            dirty_qp_map: Some(true),
            main10: Some(false),
            effective: BenchStatusEffective {
                implementation_variant: Some("candidate".into()),
                experiment_id: Some("feedback-headroom".into()),
                transport: "wss".into(),
                codec: "hevc".into(),
                stream_mode: "work".into(),
                width: 1920,
                height: 1080,
                fps: 30,
                bitrate_bps: 6_000_000,
                gop_seconds: 2,
                hybrid_mode: Some("off".into()),
                hybrid_cache_mode: Some("suppress-reactivate".into()),
                encoder: BenchStatusEncoder {
                    preset: "p3".into(),
                    tuning: "ultra-low-latency".into(),
                    multipass: "disabled".into(),
                    aq_strength: 8,
                    temporal_aq: None,
                    aq_mode: Some("spatial".into()),
                    vbv_frames: 1,
                    lookahead_depth: Some(0),
                    weighted_prediction: Some(false),
                    dirty_qp_map: Some(true),
                    main10: Some(false),
                    roi_mode: Some("feedback-headroom".into()),
                    resource_mode: Some("per-frame".into()),
                    async_depth: Some(0),
                    rate_control_mode: None,
                    target_quality: None,
                    peak_bitrate_multiplier: None,
                },
            },
            window_duration_ms: 1_000,
            encoded_bytes: 1_234_567,
            wss_payload_bytes: 1_456_789,
            encode_count: 31,
            encode_us_total: 654_321,
            encode_us_max: 45_678,
            idr_count: 1,
            delta_count: 30,
            skips: 0,
            qp_map_frames: 29,
            qp_map_changed_ctbs: 1_450,
            qp_map_total_ctbs: 59_160,
            roi_activity: Some(roi_activity),
            encode_stages: Some(BenchEncodeStages {
                frames: 31,
                register: BenchStageTiming {
                    count: 31,
                    total_us: 12_345,
                    max_us: 987,
                },
                map: BenchStageTiming {
                    count: 31,
                    total_us: 12_345,
                    max_us: 987,
                },
                submit: BenchStageTiming {
                    count: 31,
                    total_us: 123_456,
                    max_us: 9_876,
                },
                lock: BenchStageTiming {
                    count: 31,
                    total_us: 234_567,
                    max_us: 19_876,
                },
                copy: BenchStageTiming {
                    count: 31,
                    total_us: 12_345,
                    max_us: 987,
                },
                unmap: BenchStageTiming {
                    count: 31,
                    total_us: 12_345,
                    max_us: 987,
                },
                unregister: BenchStageTiming {
                    count: 31,
                    total_us: 12_345,
                    max_us: 987,
                },
            }),
            hybrid_activity: None,
        })
        .expect("v10 must carry a populated feedback-headroom status");

        assert!(wire.len() > 2_048, "fixture must reproduce the v9 ceiling");
        assert!(wire.len() <= 4_096, "v10 status must remain bounded");
    }

    #[test]
    fn quiescent_benchmark_wire_prioritizes_activity_over_unrelated_encode_stages() {
        let wire = bench_status_wire(&SignalMsg::BenchStatus {
            protocol: 2,
            profile_revision: 1,
            run_id: "123e4567-e89b-42d3-a456-426614174000".into(),
            sequence: 19,
            phase: "sample".into(),
            implementation_variant: Some("candidate".into()),
            experiment_id: Some("hybrid-png-quiescent-detail".into()),
            transport: "wss".into(),
            preset: "p3".into(),
            tuning: "ultra-low-latency".into(),
            multipass: "disabled".into(),
            aq_strength: 8,
            temporal_aq: None,
            aq_mode: Some("spatial".into()),
            vbv_frames: 1,
            lookahead_depth: Some(0),
            weighted_prediction: Some(false),
            dirty_qp_map: Some(false),
            main10: Some(false),
            effective: BenchStatusEffective {
                implementation_variant: Some("candidate".into()),
                experiment_id: Some("hybrid-png-quiescent-detail".into()),
                transport: "wss".into(),
                codec: "hevc".into(),
                stream_mode: "work".into(),
                width: 1920,
                height: 1080,
                fps: 30,
                bitrate_bps: 6_000_000,
                gop_seconds: 2,
                hybrid_mode: Some("png-quiescent-detail".into()),
                hybrid_cache_mode: Some("suppress-reactivate".into()),
                encoder: BenchStatusEncoder {
                    preset: "p3".into(),
                    tuning: "ultra-low-latency".into(),
                    multipass: "disabled".into(),
                    aq_strength: 8,
                    temporal_aq: None,
                    aq_mode: Some("spatial".into()),
                    vbv_frames: 1,
                    lookahead_depth: Some(0),
                    weighted_prediction: Some(false),
                    dirty_qp_map: Some(false),
                    main10: Some(false),
                    roi_mode: Some("off".into()),
                    resource_mode: Some("per-frame".into()),
                    async_depth: Some(0),
                    rate_control_mode: None,
                    target_quality: None,
                    peak_bitrate_multiplier: None,
                },
            },
            window_duration_ms: 1_000,
            encoded_bytes: 1_234_567,
            wss_payload_bytes: 1_456_789,
            encode_count: 31,
            encode_us_total: 654_321,
            encode_us_max: 45_678,
            idr_count: 1,
            delta_count: 30,
            skips: 0,
            qp_map_frames: 0,
            qp_map_changed_ctbs: 0,
            qp_map_total_ctbs: 0,
            roi_activity: None,
            encode_stages: Some(BenchEncodeStages {
                frames: 31,
                register: BenchStageTiming {
                    count: 31,
                    total_us: 12_345,
                    max_us: 987,
                },
                map: BenchStageTiming {
                    count: 31,
                    total_us: 12_345,
                    max_us: 987,
                },
                submit: BenchStageTiming {
                    count: 31,
                    total_us: 123_456,
                    max_us: 9_876,
                },
                lock: BenchStageTiming {
                    count: 31,
                    total_us: 234_567,
                    max_us: 19_876,
                },
                copy: BenchStageTiming {
                    count: 31,
                    total_us: 12_345,
                    max_us: 987,
                },
                unmap: BenchStageTiming {
                    count: 31,
                    total_us: 12_345,
                    max_us: 987,
                },
                unregister: BenchStageTiming {
                    count: 31,
                    total_us: 12_345,
                    max_us: 987,
                },
            }),
            hybrid_activity: Some(BenchHybridActivity {
                mode: "png-quiescent-detail".into(),
                frames: 31,
                history_resets: 0,
                history_preserved_frames: 31,
                motion_bypasses: 0,
                crop_remaps: 0,
                cursor_neutral_frames: 31,
                candidates: 124,
                reissued_tiles: 0,
                upserts: 124,
                invalidations: 120,
                suppressions: 120,
                reactivations: 116,
                permanent_invalidations: 4,
                stale_reactivation_rejects: 0,
                cached_bytes_retained: 174_762,
                png_bytes_avoided: 5_091_234,
                budget_skipped_upserts: 0,
                png_bytes: 5_396_697,
                png_us_total: 654_321,
                png_us_max: 45_678,
                hevc_tile_bytes: 0,
                hevc_tile_copy_us_total: 0,
                hevc_tile_copy_us_max: 0,
                hevc_tile_encode_us_total: 0,
                hevc_tile_encode_us_max: 0,
                hevc_tile_encoder_opens: 0,
                hevc_tile_encoder_failures: 0,
                analysis_p95_us: Some(1_234),
                analysis_max_us: Some(1_456),
                validation_frames: 31,
                validation_unavailable_frames: 0,
                validation_failed_frames: 0,
                deferred_validation_ready_frames: 29,
                deferred_validation_pending_frames: 2,
                deferred_validation_age_p95_frames: Some(2),
                deferred_validation_age_max_frames: Some(2),
                deferred_validation_pending_max_age_frames: Some(2),
                damage_audit_frames: 29,
                damage_audit_metadata_unavailable_frames: 0,
                damage_audit_changed_ctbs: 59_160,
                damage_audit_covered_ctbs: 59_160,
                damage_audit_missed_ctbs: 0,
                validation_submit_p95_us: Some(358),
                validation_submit_max_us: Some(987),
                validation_collect_p95_us: Some(11),
                validation_collect_max_us: Some(54),
                last_validation_sequence: Some(405),
                sidecar_bytes: 5_401_234,
            }),
        })
        .unwrap();

        let json: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(json["effective"]["hybridCacheMode"], "suppress-reactivate");
        assert!(json.get("encodeStages").is_none());
        assert_eq!(json["hybridActivity"]["cache"]["s"], 120);
        assert_eq!(json["hybridActivity"]["cache"]["a"], 5_091_234);
        assert_eq!(json["hybridActivity"]["deferredValidationAgeP95Frames"], 2);
        assert!(json["hybridActivity"]
            .get("damageAuditMissedCtbs")
            .is_none());
        assert!(wire.len() <= BENCH_STATUS_MAX_BYTES);
    }

    #[test]
    fn host_gate_credential_is_sent_only_as_an_authorization_header() {
        let request = signaling_request(
            "wss://playfull-cat.example/host-ws",
            Some("separate-gate-credential"),
        )
        .unwrap();
        assert_eq!(
            request.uri().to_string(),
            "wss://playfull-cat.example/host-ws"
        );
        assert_eq!(
            request.headers().get("Authorization").unwrap(),
            "Bearer separate-gate-credential"
        );
        assert!(!request.uri().to_string().contains("credential"));
    }

    #[test]
    fn lab_worker_version_override_is_header_only_and_never_reaches_production() {
        let request = signaling_request_with_worker_version(
            "wss://playfull-cat.example/lab-host-ws?run=123e4567-e89b-42d3-a456-426614174000",
            None,
            Some(("playfull-cat", "c9d1446b-5314-468c-bfad-12595f9a0090")),
        )
        .unwrap();
        assert_eq!(
            request
                .headers()
                .get("Cloudflare-Workers-Version-Overrides")
                .unwrap(),
            r#"playfull-cat="c9d1446b-5314-468c-bfad-12595f9a0090""#
        );
        assert!(!request.uri().to_string().contains("c9d1446b"));

        let production = signaling_request_with_worker_version(
            "wss://playfull-cat.example/host-ws",
            None,
            Some(("playfull-cat", "c9d1446b-5314-468c-bfad-12595f9a0090")),
        )
        .unwrap();
        assert!(production
            .headers()
            .get("Cloudflare-Workers-Version-Overrides")
            .is_none());
    }

    #[test]
    fn lab_worker_version_override_rejects_noncanonical_values() {
        assert!(signaling_request_with_worker_version(
            "wss://playfull-cat.example/lab-host-ws?run=123e4567-e89b-42d3-a456-426614174000",
            None,
            Some(("Playfull-Cat", "not-a-version")),
        )
        .is_err());
    }

    #[test]
    fn signaling_refuses_cleartext_except_to_a_loopback_relay() {
        for url in [
            "ws://192.168.1.20:8787/host-ws",
            "ws://playfull-cat.example/host-ws",
            "ws://localhost.evil.com/host-ws",
        ] {
            let error = signaling_request_with_worker_version(url, Some("gate-credential"), None)
                .unwrap_err()
                .to_string();
            assert!(error.contains("loopback"), "{error}");
            assert!(!error.contains("gate-credential"), "{error}");
        }
        for url in [
            "ws://127.0.0.1:8787/lab-host-ws?run=abc",
            "ws://localhost:8787/host-ws",
            "ws://[::1]:8787/host-ws",
        ] {
            signaling_request_with_worker_version(url, Some("gate-credential"), None).unwrap();
        }
    }

    #[test]
    fn parses_client_diagnostics_message() {
        let message: SignalMsg = serde_json::from_str(
            r#"{"type":"diag","seq":12,"atMs":1780000000000,"profile":{"rtc":{"inbound":{"packetsLost":3}}}}"#,
        )
        .expect("diagnostics message should deserialize");
        match message {
            SignalMsg::Diag {
                seq,
                at_ms,
                profile,
            } => {
                assert_eq!(seq, 12);
                assert_eq!(at_ms, 1_780_000_000_000);
                assert_eq!(profile["rtc"]["inbound"]["packetsLost"], 3);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn parses_bounded_client_log_message() {
        let message: SignalMsg = serde_json::from_str(
            r#"{"type":"client-log","seq":4,"atMs":1780000000000,"level":"error","event":"decoder-error","detail":{"message":"unsupported"}}"#,
        ).unwrap();
        assert!(
            matches!(message, SignalMsg::ClientLog { level, event, .. } if level == "error" && event == "decoder-error")
        );
    }
    #[test]
    fn parses_sparse_qos_without_rejecting_missing_numbers() {
        let message: SignalMsg = serde_json::from_str(r#"{"type":"qos","sessionId":"opaque","seq":1,"atMs":2,"rx":{"lossPct":5},"presentation":{"fps":30}}"#).unwrap();
        assert!(matches!(message, SignalMsg::Qos { .. }));
    }

    #[test]
    fn parses_wss_media_control_and_ack_messages() {
        let start: SignalMsg = serde_json::from_str(
            r#"{"type":"media-control","action":"start","codec":"hevc","quality":"high","protocol":4}"#,
        ).unwrap();
        assert!(matches!(start, SignalMsg::MediaControl {
            action, codec: Some(codec), quality: Some(quality), protocol: Some(4), ..
        } if action == "start" && codec == "hevc" && quality == "high"));
        let quality_keyframe: SignalMsg = serde_json::from_str(
            r#"{"type":"media-control","action":"keyframe","reason":"stream-lab-quality-capture","requestId":"quality-0123456789abcdef"}"#,
        )
        .unwrap();
        assert!(matches!(
            quality_keyframe,
            SignalMsg::MediaControl {
                action,
                request_id: Some(request_id),
                ..
            } if action == "keyframe" && request_id == "quality-0123456789abcdef"
        ));
        let admitted: SignalMsg = serde_json::from_str(
            r#"{"type":"first-frame","protocol":1,"phase":"quality-keyframe-admitted","requestId":"quality-0123456789abcdef","generation":3,"sequence":91}"#,
        )
        .unwrap();
        assert!(matches!(
            admitted,
            SignalMsg::FirstFrame {
                phase,
                request_id,
                sequence: Some(91),
                ..
            } if phase == "quality-keyframe-admitted" && request_id == "quality-0123456789abcdef"
        ));
    }

    #[test]
    fn parses_desktop_availability_state() {
        let state: SignalMsg = serde_json::from_str(
            r#"{"type":"desktop-state","state":"unavailable","generation":4,"reason":"capture-unavailable"}"#,
        ).unwrap();
        assert!(
            matches!(state, SignalMsg::DesktopState { state, generation: 4, .. } if state == "unavailable")
        );
    }

    #[test]
    fn input_status_preserves_windows_application_evidence() {
        let status: SignalMsg = serde_json::from_str(
            r#"{"type":"input-status","protocol":1,"generation":4,"receivedPackets":12,"acceptedPackets":11,"failedPackets":1,"droppedPackets":1,"releasedInputs":2,"requestedEvents":14,"appliedEvents":13,"lastErrorCode":5,"lastSequence":99}"#,
        )
        .unwrap();
        assert!(matches!(
            status,
            SignalMsg::InputStatus {
                generation: 4,
                failed_packets: 1,
                requested_events: 14,
                applied_events: 13,
                last_error_code: Some(5),
                last_sequence: Some(99),
                ..
            }
        ));
    }

    #[test]
    fn input_status_validates_additive_privilege_fields_and_clear() {
        use crate::input::{InputRoute, InputRoutingStatus, InputStatusReason};
        let status = InputRoutingStatus {
            reason: Some(InputStatusReason::AdminRequired),
            route: InputRoute::Local,
        };
        let warning = super::input_routing_status_message(7, status);
        assert_eq!(warning["reason"], "admin-required");
        assert_eq!(warning["route"], "local");
        assert!(serde_json::from_value::<SignalMsg>(warning.clone()).is_ok());
        let clear = super::input_routing_status_message(
            7,
            InputRoutingStatus {
                reason: None,
                route: InputRoute::Admin,
            },
        );
        assert!(clear["reason"].is_null());
        assert_eq!(clear["route"], "admin");
        assert!(serde_json::from_value::<SignalMsg>(clear).is_ok());
        for (field, value) in [
            ("reason", serde_json::json!("elevated")),
            ("route", serde_json::json!("system")),
            ("reason", serde_json::json!(true)),
            ("route", serde_json::json!(7)),
            ("route", serde_json::Value::Null),
        ] {
            let mut invalid = warning.clone();
            invalid[field] = value;
            assert!(serde_json::from_value::<SignalMsg>(invalid).is_err());
        }
    }

    #[test]
    fn preserves_secure_desktop_metadata_and_host_capabilities() {
        let state: SignalMsg = serde_json::from_str(
            r#"{"type":"desktop-state","state":"active","desktop":"secure","input":"enabled","generation":5,"reason":"desktop-switched"}"#,
        ).unwrap();
        let encoded = serde_json::to_value(state).unwrap();
        assert_eq!(encoded["desktop"], "secure");
        assert_eq!(encoded["input"], "enabled");

        let capabilities = serde_json::from_str::<SignalMsg>(
            r#"{"type":"host-capabilities","serviceMode":true,"coldBoot":true,"secureDesktop":true,"secureText":true}"#,
        );
        assert!(capabilities.is_ok());
    }

    #[test]
    fn parses_first_frame_handshake_phases() {
        let request: SignalMsg = serde_json::from_str(
            r#"{"type":"first-frame","protocol":1,"phase":"request","requestId":"firstFrame_1","transport":"wss","epoch":"0123456789abcdef0123456789abcdef"}"#,
        ).unwrap();
        assert!(
            matches!(request, SignalMsg::FirstFrame { protocol: 1, phase, request_id, transport: Some(transport), .. } if phase == "request" && request_id == "firstFrame_1" && transport == "wss")
        );
        let ready: SignalMsg = serde_json::from_str(
            r#"{"type":"first-frame","protocol":1,"phase":"ready","requestId":"firstFrame_1","generation":9,"codec":"hevc"}"#,
        ).unwrap();
        assert!(
            matches!(ready, SignalMsg::FirstFrame { phase, generation: Some(9), .. } if phase == "ready")
        );
    }

    #[test]
    fn parses_stream_mode_and_extended_wss_ack_telemetry() {
        let control: SignalMsg = serde_json::from_str(
            r#"{"type":"media-control","action":"mode","protocol":2,"streamMode":"game"}"#,
        )
        .unwrap();
        assert!(
            matches!(control, SignalMsg::MediaControl { stream_mode: Some(mode), .. } if mode == "game")
        );
    }

    #[test]
    fn input_capabilities_and_clipboard_messages_match_the_contract() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/input-contract.json")).unwrap();
        let encoded = serde_json::to_value(super::input_capabilities_message()).unwrap();
        assert_eq!(encoded["type"], "input-capabilities");
        assert_eq!(encoded["protocol"], 1);
        assert_eq!(encoded["scancode"], true);
        assert_eq!(encoded["clipboard"], true);
        assert_eq!(
            encoded["clipboardMaxBytes"],
            contract["clipboardPacket"]["maxBytes"]
        );
        let chunk = serde_json::to_value(SignalMsg::Clipboard {
            protocol: 1,
            id: 3,
            index: 0,
            count: 1,
            text: "hé".into(),
        })
        .unwrap();
        assert_eq!(
            chunk,
            serde_json::json!({"type":"clipboard","protocol":1,"id":3,"index":0,"count":1,"text":"hé"})
        );
        assert_eq!(
            contract["hostClipboardMessage"]["type"],
            serde_json::json!("clipboard")
        );
    }

    #[test]
    fn host_capabilities_carry_no_media_protocol_negotiation() {
        let message = SignalMsg::HostCapabilities {
            browser_approval: false,
            service_mode: true,
            cold_boot: true,
            secure_desktop: true,
            secure_text: true,
            file_transfer: None,
            file_drop: None,
            file_drop_preview: None,
            follow_window: None,
        };
        let encoded = serde_json::to_value(message).unwrap();
        let object = encoded.as_object().unwrap();
        for legacy in [
            "mediaProtocols",
            "maxWssLanes",
            "wssFec",
            "fileTransfer",
            "followWindow",
        ] {
            assert!(!object.contains_key(legacy), "{legacy} must not be sent");
        }
    }

    #[test]
    fn host_capabilities_announce_browser_approval_as_in_contract() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../fixtures/browser-approval-contract.json"
        ))
        .unwrap();
        let value = serde_json::to_value(super::host_capabilities_message(
            false, false, false, false, false, false, None, true,
        ))
        .unwrap();
        assert_eq!(value["type"], fixture["capability"]["type"]);
        assert_eq!(
            value["browserApproval"],
            fixture["capability"]["browserApproval"]
        );
    }

    #[test]
    fn host_capabilities_omit_browser_approval_for_ineligible_or_service_hosts() {
        for (service, eligible) in [(false, false), (true, false), (true, true)] {
            let value = serde_json::to_value(super::host_capabilities_message(
                service, false, false, false, false, false, None, eligible,
            ))
            .unwrap();
            assert!(!value.as_object().unwrap().contains_key("browserApproval"));
        }
    }

    #[test]
    fn host_capabilities_advertise_file_transfer_as_in_the_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/file-transfer-contract.json"))
                .unwrap();
        let message = SignalMsg::HostCapabilities {
            browser_approval: false,
            service_mode: false,
            cold_boot: false,
            secure_desktop: false,
            secure_text: false,
            file_transfer: Some(crate::file_transfer::PROTOCOL_VERSION),
            file_drop: None,
            file_drop_preview: None,
            follow_window: None,
        };
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            fixture["signaling"]["hostCapabilities"]["text"]
                .as_str()
                .unwrap()
        );
    }

    #[test]
    fn host_capabilities_advertise_native_drop_independently() {
        let message = SignalMsg::HostCapabilities {
            browser_approval: false,
            service_mode: false,
            cold_boot: false,
            secure_desktop: false,
            secure_text: false,
            file_transfer: Some(1),
            file_drop: Some(1),
            file_drop_preview: Some(1),
            follow_window: None,
        };
        let encoded = serde_json::to_value(message).unwrap();
        assert_eq!(encoded["fileTransfer"], 1);
        assert_eq!(encoded["fileDrop"], 1);
        assert_eq!(encoded["fileDropPreview"], 1);
    }

    fn preview_wire_valid(text: &str) -> bool {
        match serde_json::from_str::<SignalMsg>(text) {
            Ok(SignalMsg::FileDragPreview { control }) => {
                super::valid_file_drag_preview_wire(text, &control)
            }
            _ => false,
        }
    }

    fn preview_update() -> serde_json::Value {
        serde_json::json!({ "type": "file-drag-preview", "protocol": 1,
            "epoch": "0123456789abcdef0123456789abcdef", "dragId": 1, "action": "update",
            "drop": { "x": 0, "y": 65535, "generation": 1, "viewportRevision": 0 },
            "types": ["text/plain", ""] })
    }

    #[test]
    fn file_drag_preview_wire_round_trip_and_cancel() {
        let value = preview_update();
        let wire = value.to_string();
        assert!(preview_wire_valid(&wire));
        let message: SignalMsg = serde_json::from_str(&wire).unwrap();
        let SignalMsg::FileDragPreview { control } = &message else {
            panic!("typed preview");
        };
        assert!(control.valid());
        assert_eq!(control.drag_id, 1);
        assert_eq!(control.drop.unwrap().y, u16::MAX);
        assert_eq!(control.types.as_ref().unwrap(), &["text/plain", ""]);
        assert_eq!(serde_json::to_value(message).unwrap(), value);
        let cancel = super::FileDragPreviewControl {
            protocol: 1,
            epoch: "0123456789abcdef0123456789abcdef".into(),
            drag_id: u32::MAX,
            action: "cancel".into(),
            drop: None,
            types: None,
        };
        let wire = serde_json::to_string(&SignalMsg::FileDragPreview { control: cancel }).unwrap();
        assert!(preview_wire_valid(&wire));
        let value: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert!(value.get("drop").is_none());
        assert!(value.get("types").is_none());
    }

    #[test]
    fn file_drag_preview_rejects_bad_bounds_shapes_and_mime_hints() {
        for (field, bad) in [
            ("protocol", serde_json::json!(2)),
            ("dragId", serde_json::json!(0)),
            ("dragId", serde_json::json!(4294967296u64)),
            ("dragId", serde_json::json!(1.5)),
            (
                "epoch",
                serde_json::json!("0123456789ABCDEF0123456789ABCDEF"),
            ),
            ("epoch", serde_json::json!("short")),
            ("action", serde_json::json!("drop")),
            ("types", serde_json::json!([])),
            ("types", serde_json::json!(["bad type"])),
            ("types", serde_json::json!(["text/ü"])),
            ("types", serde_json::json!(["text/\n"])),
            ("types", serde_json::json!(["a".repeat(97)])),
            ("types", serde_json::json!(vec![""; 65])),
            ("types", serde_json::json!([1])),
            ("extra", serde_json::json!(true)),
        ] {
            let mut value = preview_update();
            value[field] = bad;
            assert!(!preview_wire_valid(&value.to_string()), "{field}: {value}");
        }
        for (field, bad) in [
            ("x", serde_json::json!(-1)),
            ("x", serde_json::json!(65536)),
            ("y", serde_json::json!(0.5)),
            ("generation", serde_json::json!(0)),
            ("generation", serde_json::json!(4294967296u64)),
            ("viewportRevision", serde_json::json!(-1)),
            ("extra", serde_json::json!(0)),
        ] {
            let mut value = preview_update();
            value["drop"][field] = bad;
            assert!(
                !preview_wire_valid(&value.to_string()),
                "drop {field}: {value}"
            );
        }
        for missing in ["drop", "types", "protocol", "epoch", "dragId"] {
            let mut value = preview_update();
            value.as_object_mut().unwrap().remove(missing);
            assert!(!preview_wire_valid(&value.to_string()), "missing {missing}");
        }
        let mut cancel = preview_update();
        cancel["action"] = serde_json::json!("cancel");
        assert!(!preview_wire_valid(&cancel.to_string()));
        cancel["drop"] = serde_json::Value::Null;
        cancel["types"] = serde_json::Value::Null;
        assert!(
            !preview_wire_valid(&cancel.to_string()),
            "cancel must omit fields, not null them"
        );
    }

    #[test]
    fn file_drag_preview_accepts_limits_and_bounds_raw_utf8_text() {
        let mut value = preview_update();
        value["dragId"] = serde_json::json!(u32::MAX);
        value["drop"]["generation"] = serde_json::json!(u32::MAX);
        value["drop"]["viewportRevision"] = serde_json::json!(u32::MAX);
        value["types"] = serde_json::json!(vec!["a".repeat(96); 64]);
        let mut wire = value.to_string();
        assert!(preview_wire_valid(&wire));
        wire.extend(std::iter::repeat_n(
            ' ',
            super::FILE_DRAG_PREVIEW_MAX_BYTES - wire.len(),
        ));
        assert!(preview_wire_valid(&wire));
        wire.push(' ');
        assert!(!preview_wire_valid(&wire));
        let escaped = value.to_string().replace('a', "\\u0061");
        assert!(escaped.len() > super::FILE_DRAG_PREVIEW_MAX_BYTES);
        assert!(!preview_wire_valid(&escaped));
    }

    #[test]
    fn file_drag_preview_capability_is_optional_for_older_hosts() {
        for text in [
            r#"{"type":"host-capabilities","serviceMode":true,"coldBoot":true,"secureDesktop":true,"secureText":false}"#,
            r#"{"type":"host-capabilities","serviceMode":false,"coldBoot":false,"secureDesktop":false,"secureText":false,"fileTransfer":1,"fileDrop":1}"#,
        ] {
            let message: SignalMsg = serde_json::from_str(text).unwrap();
            assert!(matches!(
                &message,
                SignalMsg::HostCapabilities {
                    file_drop_preview: None,
                    ..
                }
            ));
            assert_eq!(
                serde_json::to_value(message).unwrap(),
                serde_json::from_str::<serde_json::Value>(text).unwrap()
            );
        }
    }

    #[test]
    fn monitor_refresh_media_control_preserves_v2_epoch() {
        let control: SignalMsg = serde_json::from_str(
            r#"{"type":"media-control","action":"monitors","protocol":2,"epoch":"0123456789abcdef0123456789abcdef"}"#,
        )
        .unwrap();
        assert!(matches!(
            control,
            SignalMsg::MediaControl {
                action,
                protocol: Some(2),
                epoch: Some(epoch),
                monitor: None,
                ..
            } if action == "monitors" && epoch == "0123456789abcdef0123456789abcdef"
        ));
    }

    fn follow_fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../../fixtures/follow-window-contract.json")).unwrap()
    }

    #[test]
    fn secure_desktop_state_serializes_optional_contract_fields() {
        let message = SignalMsg::DesktopState {
            state: "unavailable".into(),
            desktop: Some("secure".into()),
            secure_kind: Some("other".into()),
            secure_capabilities: Some(vec![]),
            input_state: Some("suspended".into()),
            generation: 7,
            reason: "secure-desktop".into(),
        };
        let value = serde_json::to_value(message).unwrap();
        assert_eq!(value["secureKind"], "other");
        assert_eq!(value["secureCapabilities"], serde_json::json!([]));
        let legacy: SignalMsg = serde_json::from_str(r#"{"type":"desktop-state","state":"active","generation":8,"reason":"capture-recovered"}"#).unwrap();
        let legacy = serde_json::to_value(legacy).unwrap();
        assert!(legacy.get("secureKind").is_none());
        assert!(legacy.get("secureCapabilities").is_none());
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/secure-desktop-contract.json"))
                .unwrap();
        assert_eq!(value, fixture["desktopStates"][0]);
        for expected in fixture["desktopStates"].as_array().unwrap() {
            let message: SignalMsg = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(message).unwrap(), *expected);
        }
    }

    #[test]
    fn secure_deny_contract_is_parameterless_and_results_roundtrip() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/secure-desktop-contract.json"))
                .unwrap();
        for message in fixture["secureActions"].as_array().unwrap() {
            assert!(super::valid_secure_action_wire(&message.to_string()));
        }
        for message in fixture["secureActionResults"].as_array().unwrap() {
            let parsed: SignalMsg = serde_json::from_value(message.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), *message);
        }
        assert!(!super::valid_secure_action_wire(
            r#"{"type":"secure-action","action":"deny","x":0}"#
        ));
        assert!(!super::valid_secure_action_wire(
            r#"{"type":"secure-action","action":"allow"}"#
        ));
        assert!(!super::valid_secure_action_wire(
            r#"{"type":"secure-action-result","action":"deny"}"#
        ));
    }

    #[test]
    fn host_capabilities_advertise_follow_window_as_in_the_fixture() {
        let fixture = follow_fixture();
        assert_eq!(fixture["protocol"], crate::follow_window::PROTOCOL_VERSION);
        let message = SignalMsg::HostCapabilities {
            browser_approval: false,
            service_mode: false,
            cold_boot: false,
            secure_desktop: false,
            secure_text: false,
            file_transfer: None,
            file_drop: None,
            file_drop_preview: None,
            follow_window: Some(crate::follow_window::PROTOCOL_VERSION),
        };
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            fixture["signaling"]["hostCapabilities"]["text"]
                .as_str()
                .unwrap()
        );
    }

    #[test]
    fn host_capabilities_without_follow_serialize_as_before() {
        let message = SignalMsg::HostCapabilities {
            browser_approval: false,
            service_mode: true,
            cold_boot: true,
            secure_desktop: true,
            secure_text: false,
            file_transfer: None,
            file_drop: None,
            file_drop_preview: None,
            follow_window: None,
        };
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"type":"host-capabilities","serviceMode":true,"coldBoot":true,"secureDesktop":true,"secureText":false}"#
        );
    }

    #[test]
    fn follow_media_control_parses_as_in_the_fixture() {
        let fixture = follow_fixture();
        let control: SignalMsg = serde_json::from_str(
            fixture["signaling"]["followControl"]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            control,
            SignalMsg::MediaControl {
                action,
                follow: Some(true),
                protocol: Some(4),
                epoch: Some(_),
                reason: Some(reason),
                ..
            } if action == "follow" && reason == "user-follow"
        ));
        let off: SignalMsg =
            serde_json::from_str(r#"{"type":"media-control","action":"follow","follow":false}"#)
                .unwrap();
        assert!(matches!(
            off,
            SignalMsg::MediaControl {
                follow: Some(false),
                ..
            }
        ));
        // Older controls carry no follow flag.
        let monitor: SignalMsg = serde_json::from_str(
            r#"{"type":"media-control","action":"monitor","monitor":"DISPLAY2"}"#,
        )
        .unwrap();
        assert!(matches!(
            monitor,
            SignalMsg::MediaControl { follow: None, .. }
        ));
    }

    #[test]
    fn monitor_list_carries_follow_as_in_the_fixture() {
        let fixture = follow_fixture();
        let text = fixture["signaling"]["monitorList"]["text"]
            .as_str()
            .unwrap();
        let parsed: SignalMsg = serde_json::from_str(text).unwrap();
        let SignalMsg::MonitorList { monitors, follow } = parsed else {
            panic!("not a monitor-list");
        };
        assert_eq!(follow, Some(true));
        let message = SignalMsg::MonitorList {
            monitors: monitors.clone(),
            follow: Some(true),
        };
        assert_eq!(serde_json::to_string(&message).unwrap(), text);
        // Without the capability the list is serialized as before.
        let legacy = serde_json::to_value(SignalMsg::MonitorList {
            monitors,
            follow: None,
        })
        .unwrap();
        assert!(!legacy.as_object().unwrap().contains_key("follow"));
    }

    #[test]
    fn ignored_window_classes_match_the_fixture() {
        let fixture = follow_fixture();
        for class in fixture["ignoredWindowClasses"].as_array().unwrap() {
            assert!(crate::follow_window::is_shell_class(
                class.as_str().unwrap()
            ));
        }
        assert_eq!(
            fixture["sampleIntervalMs"],
            crate::follow_window::SAMPLE_INTERVAL.as_millis() as u64
        );
        assert_eq!(
            fixture["minSwitchIntervalMs"],
            crate::follow_window::MIN_SWITCH_INTERVAL.as_millis() as u64
        );
    }
}
