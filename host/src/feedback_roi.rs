use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use anyhow::{ensure, Result};

use crate::frame_damage::{DamageRect, HevcQpDeltaMap};

pub const FEEDBACK_WARMUP_FRAMES: u32 = 10;
pub const STABILITY_PROMOTION_FRAMES: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackBudgetPolicy {
    NetZero,
    SpendHeadroom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedbackWireDecision {
    pub policy: FeedbackBudgetPolicy,
    pub occupancy_bps: u64,
}

pub struct FeedbackWireBudget {
    started_at: Instant,
    samples: VecDeque<(Instant, u64)>,
    headroom_active: bool,
}

impl FeedbackWireBudget {
    const WINDOW: Duration = Duration::from_secs(1);
    const MIN_OBSERVATION: Duration = Duration::from_millis(250);

    pub fn new(now: Instant) -> Self {
        Self {
            started_at: now,
            samples: VecDeque::new(),
            headroom_active: false,
        }
    }

    pub fn record(&mut self, now: Instant, wire_bytes: u64) {
        self.samples.push_back((now, wire_bytes));
    }

    pub fn decision(&mut self, now: Instant, cap_bps: u64) -> FeedbackWireDecision {
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > Self::WINDOW)
        {
            self.samples.pop_front();
        }
        let observed = now
            .saturating_duration_since(self.started_at)
            .min(Self::WINDOW);
        let bytes = self
            .samples
            .iter()
            .fold(0u64, |total, (_, bytes)| total.saturating_add(*bytes));
        let observed_us = observed.as_micros();
        let occupancy_bps = if observed_us == 0 {
            0
        } else {
            (u128::from(bytes)
                .saturating_mul(8_000_000)
                .checked_div(observed_us)
                .unwrap_or(0))
            .min(u128::from(u64::MAX)) as u64
        };
        if cap_bps == 0 || observed < Self::MIN_OBSERVATION {
            self.headroom_active = false;
        } else if self.headroom_active {
            if u128::from(occupancy_bps).saturating_mul(10) >= u128::from(cap_bps).saturating_mul(9)
            {
                self.headroom_active = false;
            }
        } else if u128::from(occupancy_bps).saturating_mul(5)
            <= u128::from(cap_bps).saturating_mul(4)
        {
            self.headroom_active = true;
        }
        FeedbackWireDecision {
            policy: if self.headroom_active {
                FeedbackBudgetPolicy::SpendHeadroom
            } else {
                FeedbackBudgetPolicy::NetZero
            },
            occupancy_bps,
        }
    }
}

pub fn changed_ctb_mask(width: u32, height: u32, damage: &[DamageRect]) -> Result<Vec<bool>> {
    ensure!(
        width > 0 && height > 0,
        "feedback mask dimensions must be nonzero"
    );
    let columns = width.div_ceil(32);
    let rows = height.div_ceil(32);
    let mut changed = vec![false; (columns * rows) as usize];
    for rect in damage {
        let left = rect.left.min(width);
        let top = rect.top.min(height);
        let right = rect.right.min(width);
        let bottom = rect.bottom.min(height);
        if left >= right || top >= bottom {
            continue;
        }
        for row in top / 32..bottom.div_ceil(32).min(rows) {
            for column in left / 32..right.div_ceil(32).min(columns) {
                changed[(row * columns + column) as usize] = true;
            }
        }
    }
    Ok(changed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackPlanReason {
    Applied,
    Warmup,
    MissingFeedback,
    MotionBypass,
    EmptyDetail,
    IdrReset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackPlan {
    pub reason: FeedbackPlanReason,
    pub map: Option<HevcQpDeltaMap>,
    pub eligible_ctbs: u32,
    pub refined_ctbs: u32,
    pub funded_ctbs: u32,
    pub feedback_frames: u32,
}

pub struct FeedbackRoiPlanner {
    columns: u32,
    rows: u32,
    stable_ages: Vec<u8>,
    error_ema_q8: Vec<u32>,
    error_ema_valid: Vec<bool>,
    feedback_frames: u32,
    rotation_cursor: usize,
}

impl FeedbackRoiPlanner {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        ensure!(
            width > 0 && height > 0,
            "feedback ROI dimensions must be nonzero"
        );
        let columns = width.div_ceil(32);
        let rows = height.div_ceil(32);
        let total = columns
            .checked_mul(rows)
            .expect("feedback ROI CTB count overflow") as usize;
        Ok(Self {
            columns,
            rows,
            stable_ages: vec![0; total],
            error_ema_q8: vec![0; total],
            error_ema_valid: vec![false; total],
            feedback_frames: 0,
            rotation_cursor: 0,
        })
    }

    pub fn reset(&mut self) {
        self.stable_ages.fill(0);
        self.error_ema_q8.fill(0);
        self.error_ema_valid.fill(false);
        self.feedback_frames = 0;
        self.rotation_cursor = 0;
    }

    pub fn plan(
        &mut self,
        changed_ctbs: &[bool],
        edge_scores: &[u32],
        reconstruction_errors: Option<&[u32]>,
        idr: bool,
    ) -> Result<FeedbackPlan> {
        self.plan_with_policy(
            changed_ctbs,
            edge_scores,
            reconstruction_errors,
            idr,
            FeedbackBudgetPolicy::NetZero,
        )
    }

    pub fn plan_with_policy(
        &mut self,
        changed_ctbs: &[bool],
        edge_scores: &[u32],
        reconstruction_errors: Option<&[u32]>,
        idr: bool,
        policy: FeedbackBudgetPolicy,
    ) -> Result<FeedbackPlan> {
        let total = self.stable_ages.len();
        ensure!(
            changed_ctbs.len() == total,
            "feedback changed mask length mismatch"
        );
        ensure!(
            edge_scores.len() == total,
            "feedback edge score length mismatch"
        );
        if let Some(errors) = reconstruction_errors {
            ensure!(errors.len() == total, "feedback error length mismatch");
        }
        if idr {
            self.reset();
            return Ok(self.empty(FeedbackPlanReason::IdrReset));
        }

        let changed_count = changed_ctbs.iter().filter(|changed| **changed).count();
        for (index, changed) in changed_ctbs.iter().copied().enumerate() {
            if changed {
                self.stable_ages[index] = 0;
                self.error_ema_q8[index] = 0;
                self.error_ema_valid[index] = false;
            } else {
                self.stable_ages[index] = self.stable_ages[index].saturating_add(1);
            }
        }
        if changed_count * 2 >= total {
            return Ok(self.empty(FeedbackPlanReason::MotionBypass));
        }

        let Some(errors) = reconstruction_errors else {
            return Ok(self.empty(FeedbackPlanReason::MissingFeedback));
        };
        self.feedback_frames = self.feedback_frames.saturating_add(1);
        for (index, error) in errors.iter().copied().enumerate() {
            if changed_ctbs[index] {
                continue;
            }
            let sample_q8 = error.saturating_mul(256);
            self.error_ema_q8[index] = if self.error_ema_valid[index] {
                self.error_ema_q8[index]
                    .saturating_mul(3)
                    .saturating_add(sample_q8)
                    .saturating_add(2)
                    / 4
            } else {
                sample_q8
            };
            self.error_ema_valid[index] = true;
        }
        if self.feedback_frames < FEEDBACK_WARMUP_FRAMES {
            return Ok(self.empty(FeedbackPlanReason::Warmup));
        }

        let mut stable = (0..total)
            .filter(|index| {
                self.stable_ages[*index] >= STABILITY_PROMOTION_FRAMES
                    && self.error_ema_valid[*index]
            })
            .collect::<Vec<_>>();
        if stable.len() < 3 {
            return Ok(self.empty(FeedbackPlanReason::EmptyDetail));
        }
        stable.sort_by(|left, right| {
            edge_scores[*right].cmp(&edge_scores[*left]).then_with(|| {
                self.circular_distance(*left)
                    .cmp(&self.circular_distance(*right))
            })
        });
        let high_detail_count = stable.len().div_ceil(4).max(1);
        let mut high_detail = stable[..high_detail_count]
            .iter()
            .copied()
            .filter(|index| edge_scores[*index] > 0 && self.error_ema_q8[*index] > 0)
            .collect::<Vec<_>>();
        if high_detail.is_empty() {
            return Ok(self.empty(FeedbackPlanReason::EmptyDetail));
        }
        high_detail.sort_by(|left, right| {
            self.error_ema_q8[*right]
                .cmp(&self.error_ema_q8[*left])
                .then_with(|| {
                    self.circular_distance(*left)
                        .cmp(&self.circular_distance(*right))
                })
        });
        let requested_refined = high_detail.len().div_ceil(10).max(1);
        let refined_count = requested_refined.min(stable.len() / 3);
        if refined_count == 0 {
            return Ok(self.empty(FeedbackPlanReason::EmptyDetail));
        }
        let refined = high_detail[..refined_count].to_vec();
        let mut funding = stable
            .iter()
            .copied()
            .filter(|index| !refined.contains(index))
            .collect::<Vec<_>>();
        funding.sort_by(|left, right| {
            self.error_ema_q8[*left]
                .cmp(&self.error_ema_q8[*right])
                .then_with(|| edge_scores[*left].cmp(&edge_scores[*right]))
                .then_with(|| {
                    self.circular_distance(*left)
                        .cmp(&self.circular_distance(*right))
                })
        });
        let funded_count = match policy {
            FeedbackBudgetPolicy::NetZero => refined_count * 2,
            FeedbackBudgetPolicy::SpendHeadroom => 0,
        };
        if funding.len() < funded_count {
            return Ok(self.empty(FeedbackPlanReason::EmptyDetail));
        }

        let mut values = vec![0i8; total];
        for index in refined.iter().copied() {
            values[index] = match policy {
                FeedbackBudgetPolicy::NetZero => -2,
                FeedbackBudgetPolicy::SpendHeadroom => -5,
            };
        }
        for index in funding.iter().copied().take(funded_count) {
            values[index] = 1;
        }
        self.rotation_cursor = (refined.last().copied().unwrap_or(0) + 1) % total;
        Ok(FeedbackPlan {
            reason: FeedbackPlanReason::Applied,
            map: Some(HevcQpDeltaMap {
                columns: self.columns,
                rows: self.rows,
                values,
            }),
            eligible_ctbs: high_detail.len() as u32,
            refined_ctbs: refined_count as u32,
            funded_ctbs: funded_count as u32,
            feedback_frames: self.feedback_frames,
        })
    }

    fn circular_distance(&self, index: usize) -> usize {
        (index + self.stable_ages.len() - self.rotation_cursor) % self.stable_ages.len()
    }

    fn empty(&self, reason: FeedbackPlanReason) -> FeedbackPlan {
        FeedbackPlan {
            reason,
            map: None,
            eligible_ctbs: 0,
            refined_ctbs: 0,
            funded_ctbs: 0,
            feedback_frames: self.feedback_frames,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        changed_ctb_mask, FeedbackBudgetPolicy, FeedbackPlanReason, FeedbackRoiPlanner,
        FeedbackWireBudget, FEEDBACK_WARMUP_FRAMES, STABILITY_PROMOTION_FRAMES,
    };
    use crate::frame_damage::DamageRect;

    fn warm(planner: &mut FeedbackRoiPlanner, changed: &[bool], edges: &[u32], errors: &[u32]) {
        for _ in 0..FEEDBACK_WARMUP_FRAMES {
            planner
                .plan(changed, edges, Some(errors), false)
                .expect("warm feedback planner");
        }
    }

    #[test]
    fn promotes_only_stable_high_detail_ctbs_after_feedback_warmup_and_funds_them() {
        let mut planner = FeedbackRoiPlanner::new(128, 64).unwrap();
        let changed = vec![false; 8];
        let edges = vec![100, 90, 80, 70, 10, 9, 8, 7];
        let errors = vec![500, 400, 300, 200, 4, 3, 2, 1];
        warm(&mut planner, &changed, &edges, &errors);

        let plan = planner
            .plan(&changed, &edges, Some(&errors), false)
            .expect("feedback plan");
        assert_eq!(plan.reason, FeedbackPlanReason::Applied);
        assert!(plan.eligible_ctbs > 0);
        assert!(plan.refined_ctbs > 0);
        assert_eq!(plan.funded_ctbs, plan.refined_ctbs * 2);
        assert!(planner
            .stable_ages
            .iter()
            .all(|age| *age >= STABILITY_PROMOTION_FRAMES));
        let map = plan.map.expect("applied QP map");
        assert_eq!(
            map.values.iter().filter(|delta| **delta < 0).count() as u32,
            plan.refined_ctbs
        );
        assert_eq!(
            map.values.iter().filter(|delta| **delta > 0).count() as u32,
            plan.funded_ctbs
        );
        assert_eq!(
            map.values
                .iter()
                .map(|delta| i32::from(*delta))
                .sum::<i32>(),
            0
        );
    }

    #[test]
    fn headroom_policy_spends_bits_on_refinement_without_positive_qp_funding() {
        let mut planner = FeedbackRoiPlanner::new(128, 64).unwrap();
        let changed = vec![false; 8];
        let edges = vec![100, 90, 80, 70, 10, 9, 8, 7];
        let errors = vec![500, 400, 300, 200, 4, 3, 2, 1];
        warm(&mut planner, &changed, &edges, &errors);

        let plan = planner
            .plan_with_policy(
                &changed,
                &edges,
                Some(&errors),
                false,
                FeedbackBudgetPolicy::SpendHeadroom,
            )
            .expect("headroom feedback plan");
        assert_eq!(plan.reason, FeedbackPlanReason::Applied);
        assert_eq!(plan.funded_ctbs, 0);
        let map = plan.map.expect("headroom QP map");
        assert!(map.values.contains(&-5));
        assert!(map.values.iter().all(|delta| *delta <= 0));
    }

    #[test]
    fn wire_budget_enters_headroom_only_after_measured_low_occupancy_and_exits_near_cap() {
        let start = Instant::now();
        let mut budget = FeedbackWireBudget::new(start);
        assert_eq!(
            budget.decision(start, 6_000_000).policy,
            FeedbackBudgetPolicy::NetZero
        );

        for frame in 1..=9 {
            budget.record(start + Duration::from_millis(frame * 33), 6_800);
        }
        let low = budget.decision(start + Duration::from_millis(300), 6_000_000);
        assert_eq!(low.policy, FeedbackBudgetPolicy::SpendHeadroom);
        assert!((1_500_000..=2_000_000).contains(&low.occupancy_bps));

        budget.record(start + Duration::from_millis(330), 180_000);
        let near_cap = budget.decision(start + Duration::from_millis(330), 6_000_000);
        assert_eq!(near_cap.policy, FeedbackBudgetPolicy::NetZero);
        assert!(near_cap.occupancy_bps >= 5_400_000);
    }

    #[test]
    fn changed_ctb_loses_history_and_cannot_be_refined() {
        let mut planner = FeedbackRoiPlanner::new(128, 64).unwrap();
        let stable = vec![false; 8];
        let edges = vec![100, 90, 80, 70, 10, 9, 8, 7];
        let errors = vec![500, 400, 300, 200, 4, 3, 2, 1];
        warm(&mut planner, &stable, &edges, &errors);
        let mut changed = stable.clone();
        changed[0] = true;

        let plan = planner
            .plan(&changed, &edges, Some(&errors), false)
            .unwrap();
        assert_eq!(planner.stable_ages[0], 0);
        assert_eq!(planner.error_ema_q8[0], 0);
        assert!(plan.map.is_none_or(|map| map.values[0] >= 0));
    }

    #[test]
    fn equal_error_detail_budget_rotates_instead_of_pinching_one_ctb_forever() {
        let mut planner = FeedbackRoiPlanner::new(320, 64).unwrap();
        let changed = vec![false; 20];
        let edges = vec![100; 20];
        let errors = vec![50; 20];
        warm(&mut planner, &changed, &edges, &errors);
        let first = planner
            .plan(&changed, &edges, Some(&errors), false)
            .unwrap()
            .map
            .unwrap();
        let second = planner
            .plan(&changed, &edges, Some(&errors), false)
            .unwrap()
            .map
            .unwrap();
        let selected = |values: &[i8]| {
            values
                .iter()
                .enumerate()
                .filter_map(|(index, delta)| (*delta < 0).then_some(index))
                .collect::<Vec<_>>()
        };
        assert_ne!(selected(&first.values), selected(&second.values));
    }

    #[test]
    fn half_frame_motion_bypasses_refinement() {
        let mut planner = FeedbackRoiPlanner::new(128, 64).unwrap();
        let stable = vec![false; 8];
        let edges = vec![100; 8];
        let errors = vec![50; 8];
        warm(&mut planner, &stable, &edges, &errors);
        let changed = vec![true, true, true, true, false, false, false, false];
        let plan = planner
            .plan(&changed, &edges, Some(&errors), false)
            .unwrap();
        assert_eq!(plan.reason, FeedbackPlanReason::MotionBypass);
        assert!(plan.map.is_none());
    }

    #[test]
    fn idr_clears_feedback_history_and_requires_fresh_warmup() {
        let mut planner = FeedbackRoiPlanner::new(128, 64).unwrap();
        let changed = vec![false; 8];
        let edges = vec![100; 8];
        let errors = vec![50; 8];
        warm(&mut planner, &changed, &edges, &errors);
        let reset = planner.plan(&changed, &edges, Some(&errors), true).unwrap();
        assert_eq!(reset.reason, FeedbackPlanReason::IdrReset);
        assert_eq!(reset.feedback_frames, 0);
        assert!(planner.stable_ages.iter().all(|age| *age == 0));
        let after = planner
            .plan(&changed, &edges, Some(&errors), false)
            .unwrap();
        assert_eq!(after.reason, FeedbackPlanReason::Warmup);
        assert!(after.map.is_none());
    }

    #[test]
    fn changed_mask_tracks_aligned_damage_without_rejecting_empty_or_motion_frames() {
        let localized = [DamageRect {
            left: 32,
            top: 0,
            right: 64,
            bottom: 32,
        }];
        assert_eq!(
            changed_ctb_mask(128, 64, &localized).unwrap(),
            vec![false, true, false, false, false, false, false, false]
        );
        assert_eq!(changed_ctb_mask(128, 64, &[]).unwrap(), vec![false; 8]);
        assert_eq!(
            changed_ctb_mask(
                128,
                64,
                &[DamageRect {
                    left: 0,
                    top: 0,
                    right: 128,
                    bottom: 64,
                }],
            )
            .unwrap(),
            vec![true; 8]
        );
    }
}
