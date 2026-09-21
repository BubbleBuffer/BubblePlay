//! Binary-owned benchmark boundary; preserves existing streaming policy.
use super::{
    benchmark, wss_gop_interval, BenchEncodeStages, BenchHybridActivity, BenchRoiActivity,
    BenchStageTiming, BenchmarkHybridMode, BenchmarkRoiMode, BenchmarkStreamMode,
    BenchmarkTransport, DamageCoverageAudit, DeferredReadbackActivity, Duration,
    EncodeStageTimings, FeedbackBudgetPolicy, FeedbackPlan, FeedbackPlanReason,
    FeedbackWireDecision, HybridTileOperation, HybridWireBudget, LabRoiReason, LabRoiTrace,
    QuiescentTilePlan, QuiescentTilePlanner, QuiescentValidationStatus,
    ReconstructionFeedbackSubmission, RoiMapReason, RoiMapTelemetry, StreamMode,
    TemporalDiffResult, TemporalMaskDecision, VideoCodec, IDR_INTERVAL,
};

pub(super) fn benchmark_mode_matches(
    benchmark_mode: BenchmarkStreamMode,
    stream_mode: StreamMode,
) -> bool {
    matches!(
        (benchmark_mode, stream_mode),
        (BenchmarkStreamMode::Work, StreamMode::Work)
            | (BenchmarkStreamMode::Game, StreamMode::Game)
    )
}

pub(super) fn benchmark_owned_encoder_bitrate(
    requested_bitrate_bps: u32,
    benchmark_profile: Option<&benchmark::BenchmarkProfile>,
    transport: BenchmarkTransport,
    stream_mode: StreamMode,
    codec: VideoCodec,
) -> u32 {
    benchmark_profile
        .filter(|profile| {
            codec == VideoCodec::Hevc
                && profile.transport == transport
                && benchmark_mode_matches(profile.stream_mode, stream_mode)
        })
        .map_or(requested_bitrate_bps, |profile| profile.bitrate_bps)
}

pub(super) fn effective_idr_interval(
    stream_mode: StreamMode,
    wss_active: bool,
    benchmark_gop_seconds: Option<u8>,
) -> Duration {
    benchmark_gop_seconds
        .map(|seconds| Duration::from_secs(u64::from(seconds)))
        .unwrap_or_else(|| {
            if wss_active {
                wss_gop_interval(stream_mode)
            } else {
                IDR_INTERVAL
            }
        })
}

pub(super) fn advance_benchmark_profile_revision(
    current: u64,
    effective_profile_changed: bool,
) -> u64 {
    if effective_profile_changed {
        current.wrapping_add(1).max(1)
    } else {
        current
    }
}

pub(super) fn new_benchmark_roi_activity(
    profile: &benchmark::BenchmarkProfile,
) -> Option<BenchRoiActivity> {
    let mode = match profile.effective_roi_mode() {
        BenchmarkRoiMode::Off => return None,
        BenchmarkRoiMode::Damage => "damage",
        BenchmarkRoiMode::TextEdge => "text-edge",
        BenchmarkRoiMode::Feedback => return Some(BenchRoiActivity::feedback()),
        BenchmarkRoiMode::FeedbackHeadroom => return Some(BenchRoiActivity::feedback_headroom()),
    };
    Some(BenchRoiActivity {
        mode: mode.into(),
        ..BenchRoiActivity::default()
    })
}

pub(super) fn new_benchmark_hybrid_activity(
    profile: &benchmark::BenchmarkProfile,
) -> Option<BenchHybridActivity> {
    let mode = match profile.hybrid_mode {
        BenchmarkHybridMode::PngQuiescentDetail => "png-quiescent-detail",
        BenchmarkHybridMode::HevcQuiescentTiles => "hevc-quiescent-tiles",
        _ => return None,
    };
    Some(BenchHybridActivity {
        mode: mode.into(),
        ..BenchHybridActivity::default()
    })
}

pub(super) fn record_benchmark_hybrid_plan(
    window: &mut Option<BenchHybridActivity>,
    analysis_samples_us: &mut Vec<u32>,
    plan: &QuiescentTilePlan,
    damage_empty: bool,
    analysis_us: u32,
) {
    let Some(window) = window.as_mut() else {
        return;
    };
    window.frames = window.frames.saturating_add(1);
    window.history_resets = window
        .history_resets
        .saturating_add(u64::from(plan.clear && !plan.history_preserved));
    window.history_preserved_frames = window
        .history_preserved_frames
        .saturating_add(u64::from(plan.history_preserved));
    window.motion_bypasses = window
        .motion_bypasses
        .saturating_add(u64::from(plan.motion_bypass));
    window.crop_remaps = window
        .crop_remaps
        .saturating_add(u64::from(plan.clear && plan.history_preserved));
    window.cursor_neutral_frames = window.cursor_neutral_frames.saturating_add(u64::from(
        damage_empty && !plan.clear && plan.invalidated.is_empty() && plan.candidates.is_empty(),
    ));
    window.candidates = window
        .candidates
        .saturating_add(plan.candidates.len() as u64);
    window.reissued_tiles = window
        .reissued_tiles
        .saturating_add(if plan.history_preserved {
            plan.candidates.len() as u64
        } else {
            0
        });
    window.invalidations = window
        .invalidations
        .saturating_add(plan.invalidated.len() as u64);
    analysis_samples_us.push(analysis_us);
    let mut ordered = analysis_samples_us.clone();
    ordered.sort_unstable();
    let p95_index = ordered
        .len()
        .saturating_mul(95)
        .div_ceil(100)
        .saturating_sub(1);
    window.analysis_p95_us = ordered.get(p95_index).copied();
    window.analysis_max_us = ordered.last().copied();
}

pub(super) fn record_benchmark_hybrid_validation(
    window: &mut Option<BenchHybridActivity>,
    submit_samples_us: &mut Vec<u32>,
    collect_samples_us: &mut Vec<u32>,
    deferred_age_samples_frames: &mut Vec<u32>,
    submit_us: u32,
    collect_us: u32,
    status: QuiescentValidationStatus,
    deferred: &DeferredReadbackActivity,
    damage_audit: Option<DamageCoverageAudit>,
    sequence: u32,
) {
    let Some(window) = window.as_mut() else {
        return;
    };
    window.validation_frames = window.validation_frames.saturating_add(1);
    window.validation_unavailable_frames = window
        .validation_unavailable_frames
        .saturating_add(u64::from(status == QuiescentValidationStatus::Unavailable));
    window.validation_failed_frames = window
        .validation_failed_frames
        .saturating_add(u64::from(status == QuiescentValidationStatus::Failed));
    window.last_validation_sequence = Some(sequence);
    window.deferred_validation_ready_frames = window
        .deferred_validation_ready_frames
        .saturating_add(deferred.ready_ages_frames.len() as u64);
    window.deferred_validation_pending_frames = window
        .deferred_validation_pending_frames
        .saturating_add(u64::from(deferred.pending_frames));
    if let Some(pending_age) = deferred.max_pending_age_frames {
        window.deferred_validation_pending_max_age_frames = Some(
            window
                .deferred_validation_pending_max_age_frames
                .map_or(pending_age, |current| current.max(pending_age)),
        );
    }
    deferred_age_samples_frames.extend_from_slice(&deferred.ready_ages_frames);
    deferred_age_samples_frames.sort_unstable();
    let deferred_p95 = deferred_age_samples_frames
        .len()
        .saturating_mul(95)
        .div_ceil(100)
        .saturating_sub(1);
    window.deferred_validation_age_p95_frames =
        deferred_age_samples_frames.get(deferred_p95).copied();
    window.deferred_validation_age_max_frames = deferred_age_samples_frames.last().copied();

    if let Some(audit) = damage_audit {
        window.damage_audit_frames = window.damage_audit_frames.saturating_add(1);
        window.damage_audit_metadata_unavailable_frames = window
            .damage_audit_metadata_unavailable_frames
            .saturating_add(u64::from(!audit.metadata_available));
        window.damage_audit_changed_ctbs = window
            .damage_audit_changed_ctbs
            .saturating_add(u64::from(audit.changed_ctbs));
        window.damage_audit_covered_ctbs = window
            .damage_audit_covered_ctbs
            .saturating_add(u64::from(audit.covered_ctbs));
        window.damage_audit_missed_ctbs = window
            .damage_audit_missed_ctbs
            .saturating_add(u64::from(audit.missed_ctbs));
    }

    submit_samples_us.push(submit_us);
    submit_samples_us.sort_unstable();
    let submit_p95 = submit_samples_us
        .len()
        .saturating_mul(95)
        .div_ceil(100)
        .saturating_sub(1);
    window.validation_submit_p95_us = submit_samples_us.get(submit_p95).copied();
    window.validation_submit_max_us = submit_samples_us.last().copied();

    collect_samples_us.push(collect_us);
    collect_samples_us.sort_unstable();
    let collect_p95 = collect_samples_us
        .len()
        .saturating_mul(95)
        .div_ceil(100)
        .saturating_sub(1);
    window.validation_collect_p95_us = collect_samples_us.get(collect_p95).copied();
    window.validation_collect_max_us = collect_samples_us.last().copied();
}

pub(super) const fn quiescent_transition_followup_needed(
    output_changed: bool,
    upserts: usize,
    budget_skipped_upserts: usize,
    suppressed_tiles: usize,
) -> bool {
    suppressed_tiles > 0 || (output_changed && (upserts > 0 || budget_skipped_upserts > 0))
}

pub(super) fn coalesce_quiescent_cache_round_trips(operations: &mut Vec<HybridTileOperation>) {
    let mut suppress_index = 0usize;
    while suppress_index < operations.len() {
        let Some((tile_id, content_revision, validation_sequence)) =
            (match &operations[suppress_index] {
                HybridTileOperation::Suppress {
                    tile_id,
                    content_revision,
                    validation_sequence,
                } => Some((*tile_id, *content_revision, *validation_sequence)),
                _ => None,
            })
        else {
            suppress_index += 1;
            continue;
        };
        let matching_reactivation = operations
            .iter()
            .enumerate()
            .skip(suppress_index + 1)
            .find_map(|(index, operation)| match operation {
                HybridTileOperation::Reactivate {
                    tile_id: candidate_tile_id,
                    content_revision: candidate_content_revision,
                    validation_sequence: candidate_validation_sequence,
                } if (
                    *candidate_tile_id,
                    *candidate_content_revision,
                    *candidate_validation_sequence,
                ) == (tile_id, content_revision, validation_sequence) =>
                {
                    Some(index)
                }
                _ => None,
            });
        if let Some(reactivation_index) = matching_reactivation {
            operations.remove(reactivation_index);
            operations.remove(suppress_index);
        } else {
            suppress_index += 1;
        }
    }
}

pub(super) const fn quiescent_transition_upsert_allowed(
    output_changed: bool,
    successful_upserts: usize,
) -> bool {
    !output_changed || successful_upserts == 0
}

pub(super) fn recover_quiescent_after_unsubmitted_frame(
    planner: &mut QuiescentTilePlanner,
    wire_budget: &mut HybridWireBudget,
) {
    planner.reset();
    wire_budget.reset();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdleSnapshotCompletionOrder {
    None,
    Acknowledge,
    Request,
    AcknowledgeThenRequest,
}

pub(super) const fn idle_snapshot_completion_order(
    acknowledge_current: bool,
    request_followup: bool,
) -> IdleSnapshotCompletionOrder {
    match (acknowledge_current, request_followup) {
        (false, false) => IdleSnapshotCompletionOrder::None,
        (true, false) => IdleSnapshotCompletionOrder::Acknowledge,
        (false, true) => IdleSnapshotCompletionOrder::Request,
        (true, true) => IdleSnapshotCompletionOrder::AcknowledgeThenRequest,
    }
}

pub(super) fn record_benchmark_roi_activity(
    window: &mut Option<BenchRoiActivity>,
    frame: RoiMapTelemetry,
    sequence: Option<u32>,
) {
    let Some(window) = window.as_mut() else {
        return;
    };
    window.evaluated_frames = window.evaluated_frames.saturating_add(1);
    match frame.reason {
        RoiMapReason::Applied => {
            window.mapped_frames = window.mapped_frames.saturating_add(1);
        }
        RoiMapReason::MissingDamage => {
            window.missing_damage_frames = window.missing_damage_frames.saturating_add(1);
        }
        RoiMapReason::EmptyDamage => {
            window.empty_damage_frames = window.empty_damage_frames.saturating_add(1);
        }
        RoiMapReason::Idr => {
            window.idr_frames = window.idr_frames.saturating_add(1);
        }
        RoiMapReason::CoverageAtLeastHalf => {
            window.coverage_rejected_frames = window.coverage_rejected_frames.saturating_add(1);
        }
        RoiMapReason::UnsupportedEncoderState => {
            window.unsupported_frames = window.unsupported_frames.saturating_add(1);
        }
    }
    window.last_sequence = sequence;
    window.last_reason = Some(frame.reason.label().into());
    window.last_changed_ctbs = Some(frame.changed_ctbs);
    window.last_total_ctbs = Some(frame.total_ctbs);
    window.last_min_delta = frame.min_delta;
    window.last_max_delta = frame.max_delta;
}

pub(super) fn lab_roi_trace(frame: Option<RoiMapTelemetry>) -> Option<LabRoiTrace> {
    frame.map(|frame| LabRoiTrace {
        reason: match frame.reason {
            RoiMapReason::Applied => LabRoiReason::Applied,
            RoiMapReason::MissingDamage => LabRoiReason::MissingDamage,
            RoiMapReason::EmptyDamage => LabRoiReason::EmptyDamage,
            RoiMapReason::Idr => LabRoiReason::Idr,
            RoiMapReason::CoverageAtLeastHalf => LabRoiReason::CoverageAtLeastHalf,
            RoiMapReason::UnsupportedEncoderState => LabRoiReason::UnsupportedEncoderState,
        },
        changed_ctbs: frame.changed_ctbs.min(u32::from(u16::MAX)) as u16,
        total_ctbs: frame.total_ctbs.min(u32::from(u16::MAX)) as u16,
    })
}

pub(super) fn record_benchmark_temporal_activity(
    window: &mut Option<BenchRoiActivity>,
    analysis_samples_us: &mut Vec<u32>,
    frame: &TemporalDiffResult,
    sequence: Option<u32>,
) {
    let Some(window) = window.as_mut() else {
        return;
    };
    let (changed_ctbs, total_ctbs) = match &frame.decision {
        TemporalMaskDecision::Applied {
            changed_ctbs,
            total_ctbs,
            ..
        }
        | TemporalMaskDecision::Rejected {
            changed_ctbs,
            total_ctbs,
            ..
        } => (*changed_ctbs, *total_ctbs),
    };
    window.source = Some("gpu-temporal".into());
    window.temporal_frames = window.temporal_frames.saturating_add(1);
    if frame.priming {
        window.temporal_priming_frames = window.temporal_priming_frames.saturating_add(1);
    }
    analysis_samples_us.push(frame.analysis_us);
    let mut ordered = analysis_samples_us.clone();
    ordered.sort_unstable();
    let p95_index = ordered
        .len()
        .saturating_mul(95)
        .div_ceil(100)
        .saturating_sub(1);
    window.analysis_p95_us = ordered.get(p95_index).copied();
    window.analysis_max_us = ordered.last().copied();
    window.last_sequence = sequence;
    window.last_changed_ctbs = Some(changed_ctbs);
    window.last_total_ctbs = Some(total_ctbs);
}

pub(super) fn add_feedback_count(field: &mut Option<u64>, amount: u64) {
    *field = Some(field.unwrap_or(0).saturating_add(amount));
}

pub(super) fn record_feedback_timing(
    samples: &mut Vec<u32>,
    p95: &mut Option<u32>,
    max: &mut Option<u32>,
    elapsed_us: u32,
) {
    samples.push(elapsed_us);
    let mut ordered = samples.clone();
    ordered.sort_unstable();
    let p95_index = ordered
        .len()
        .saturating_mul(95)
        .div_ceil(100)
        .saturating_sub(1);
    *p95 = ordered.get(p95_index).copied();
    *max = ordered.last().copied();
}

pub(super) fn record_benchmark_feedback_plan(
    window: &mut Option<BenchRoiActivity>,
    wire_occupancy_samples_bps: &mut Vec<u64>,
    plan: &FeedbackPlan,
    wire_decision: Option<FeedbackWireDecision>,
) {
    let Some(window) = window.as_mut() else {
        return;
    };
    match plan.reason {
        FeedbackPlanReason::Applied => {
            add_feedback_count(&mut window.feedback_adjusted_frames, 1);
            add_feedback_count(
                &mut window.feedback_eligible_ctbs,
                u64::from(plan.eligible_ctbs),
            );
            add_feedback_count(
                &mut window.feedback_refined_ctbs,
                u64::from(plan.refined_ctbs),
            );
            add_feedback_count(
                &mut window.feedback_funded_ctbs,
                u64::from(plan.funded_ctbs),
            );
            if let Some(decision) = wire_decision {
                match decision.policy {
                    FeedbackBudgetPolicy::NetZero => {
                        add_feedback_count(&mut window.feedback_net_zero_frames, 1)
                    }
                    FeedbackBudgetPolicy::SpendHeadroom => {
                        add_feedback_count(&mut window.feedback_spend_headroom_frames, 1)
                    }
                }
                wire_occupancy_samples_bps.push(decision.occupancy_bps);
                let mut ordered = wire_occupancy_samples_bps.clone();
                ordered.sort_unstable();
                let p95_index = ordered
                    .len()
                    .saturating_mul(95)
                    .div_ceil(100)
                    .saturating_sub(1);
                window.feedback_wire_occupancy_p95_bps = ordered.get(p95_index).copied();
                window.feedback_wire_occupancy_max_bps = ordered.last().copied();
                window.last_feedback_budget_policy = Some(
                    match decision.policy {
                        FeedbackBudgetPolicy::NetZero => "net-zero",
                        FeedbackBudgetPolicy::SpendHeadroom => "spend-headroom",
                    }
                    .into(),
                );
                window.last_feedback_wire_occupancy_bps = Some(decision.occupancy_bps);
            }
        }
        FeedbackPlanReason::Warmup => {
            add_feedback_count(&mut window.feedback_warmup_frames, 1);
        }
        FeedbackPlanReason::MotionBypass => {
            add_feedback_count(&mut window.feedback_motion_bypasses, 1);
        }
        FeedbackPlanReason::MissingFeedback
        | FeedbackPlanReason::EmptyDetail
        | FeedbackPlanReason::IdrReset => {}
    }
}

pub(super) fn record_benchmark_feedback_submission(
    window: &mut Option<BenchRoiActivity>,
    samples: &mut Vec<u32>,
    submission: ReconstructionFeedbackSubmission,
) {
    let Some(window) = window.as_mut() else {
        return;
    };
    if submission.submitted {
        add_feedback_count(&mut window.feedback_submitted_frames, 1);
        record_feedback_timing(
            samples,
            &mut window.feedback_submit_p95_us,
            &mut window.feedback_submit_max_us,
            submission.submit_us,
        );
    } else {
        add_feedback_count(&mut window.feedback_skipped_frames, 1);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TemporalReadbackPolicy {
    Deferred,
    Aligned,
}

pub(super) fn temporal_readback_policy(
    static_hybrid_enabled: bool,
    _feedback_roi_enabled: bool,
) -> TemporalReadbackPolicy {
    if static_hybrid_enabled {
        TemporalReadbackPolicy::Aligned
    } else {
        TemporalReadbackPolicy::Deferred
    }
}

pub(super) fn record_benchmark_stage(stage: &mut BenchStageTiming, count: u32, elapsed_us: u64) {
    if count == 0 {
        return;
    }
    stage.count = stage.count.saturating_add(u64::from(count));
    stage.total_us = stage.total_us.saturating_add(elapsed_us);
    stage.max_us = stage.max_us.max(elapsed_us.min(u64::from(u32::MAX)) as u32);
}

pub(super) fn record_benchmark_encode_stages(
    window: &mut BenchEncodeStages,
    frame: EncodeStageTimings,
) {
    window.frames = window.frames.saturating_add(1);
    record_benchmark_stage(
        &mut window.register,
        frame.register_calls,
        frame.register_us,
    );
    record_benchmark_stage(&mut window.map, 1, frame.map_us);
    record_benchmark_stage(&mut window.submit, 1, frame.submit_us);
    record_benchmark_stage(&mut window.lock, 1, frame.lock_us);
    record_benchmark_stage(&mut window.copy, 1, frame.copy_us);
    record_benchmark_stage(&mut window.unmap, 1, frame.unmap_us);
    record_benchmark_stage(
        &mut window.unregister,
        frame.unregister_calls,
        frame.unregister_us,
    );
}

pub(super) fn should_reset_lab_encoder_for_keyframe(
    enabled: bool,
    action: &str,
    reason: Option<&str>,
) -> bool {
    enabled && action == "keyframe" && reason == Some("stream-lab-quality-capture")
}

pub(super) fn valid_lab_quality_request_id(value: &str) -> bool {
    (8..=96).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(super) fn lab_quality_keyframe_request<'a>(
    action: &str,
    reason: Option<&str>,
    request_id: Option<&'a str>,
) -> Option<&'a str> {
    if action != "keyframe" || reason != Some("stream-lab-quality-capture") {
        return None;
    }
    request_id.filter(|value| valid_lab_quality_request_id(value))
}

pub(super) fn benchmark_status_retry_delay(status_sent: bool) -> Duration {
    if status_sent {
        Duration::from_secs(1)
    } else {
        Duration::ZERO
    }
}
