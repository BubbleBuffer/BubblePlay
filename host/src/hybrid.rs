use crate::frame_damage::DamageRect;
use anyhow::{ensure, Context};
use std::collections::{BTreeMap, BTreeSet};

pub const HYBRID_FRAME_MAGIC: [u8; 4] = *b"BPH1";
pub const HYBRID_FRAME_VERSION: u8 = 1;
pub const MAX_HYBRID_TILES: usize = 4;
pub const MAX_HYBRID_TILE_DIMENSION: u16 = 512;
pub const HEVC_HYBRID_TILE_DIMENSION: u16 = 128;
pub const MAX_HYBRID_SIDECAR_BYTES: usize = 256 * 1024;
const MAX_HYBRID_CARRIED_BYTES: usize = MAX_HYBRID_SIDECAR_BYTES * MAX_HYBRID_TILES;
const HYBRID_HEADER_BYTES: usize = 32;
const HYBRID_MAX_OPERATIONS: usize = MAX_HYBRID_TILES * 3 + 1;
const OP_UPSERT: u8 = 1;
const OP_INVALIDATE: u8 = 2;
const OP_CLEAR: u8 = 3;
const OP_SUPPRESS: u8 = 4;
const OP_REACTIVATE: u8 = 5;
const OP_INVALIDATE_VALIDATED: u8 = 6;
const OP_HEVC_UPSERT: u8 = 7;
const QUIESCENT_REBALANCE_GAIN_NUMERATOR: u128 = 5;
const QUIESCENT_REBALANCE_GAIN_DENOMINATOR: u128 = 4;

pub fn hybrid_sidecar_budget(
    total_wss_bitrate_bps: u32,
    fps: u32,
    lane_count: u8,
    data_shards: u8,
    access_unit_bytes: usize,
) -> usize {
    logical_bytes_per_frame(total_wss_bitrate_bps, fps, lane_count, data_shards)
        .saturating_sub(access_unit_bytes)
        .min(MAX_HYBRID_SIDECAR_BYTES)
}

fn logical_bytes_per_frame(
    total_wss_bitrate_bps: u32,
    fps: u32,
    lane_count: u8,
    data_shards: u8,
) -> usize {
    if fps == 0 || lane_count == 0 || data_shards == 0 || data_shards > lane_count {
        return 0;
    }
    let logical_bytes = u128::from(total_wss_bitrate_bps).saturating_mul(u128::from(data_shards))
        / u128::from(lane_count)
        / 8
        / u128::from(fps);
    usize::try_from(logical_bytes).unwrap_or(usize::MAX)
}

#[derive(Debug, Default)]
pub struct HybridWireBudget {
    configuration: Option<(u32, u32, u8, u8)>,
    carried_bytes: usize,
}

impl HybridWireBudget {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn begin_frame(
        &mut self,
        total_wss_bitrate_bps: u32,
        fps: u32,
        lane_count: u8,
        data_shards: u8,
        access_unit_bytes: usize,
    ) -> usize {
        let configuration = (total_wss_bitrate_bps, fps, lane_count, data_shards);
        if self.configuration != Some(configuration) {
            self.configuration = Some(configuration);
            self.carried_bytes = 0;
        }
        let frame_credit =
            logical_bytes_per_frame(total_wss_bitrate_bps, fps, lane_count, data_shards);
        if frame_credit == 0 {
            self.carried_bytes = 0;
            return 0;
        }
        self.carried_bytes = self
            .carried_bytes
            .saturating_add(frame_credit)
            .min(MAX_HYBRID_CARRIED_BYTES)
            .saturating_sub(access_unit_bytes);
        self.carried_bytes.min(MAX_HYBRID_SIDECAR_BYTES)
    }

    pub fn commit_sidecar(&mut self, sidecar_bytes: usize) {
        self.carried_bytes = self.carried_bytes.saturating_sub(sidecar_bytes);
    }

    pub const fn carried_bytes(&self) -> usize {
        self.carried_bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HybridTileOperation {
    Clear,
    Invalidate {
        tile_id: u16,
    },
    Upsert {
        tile_id: u16,
        rect: DamageRect,
        png: Vec<u8>,
    },
    HevcUpsert {
        tile_id: u16,
        rect: DamageRect,
        access_unit: Vec<u8>,
    },
    Suppress {
        tile_id: u16,
        content_revision: u32,
        validation_sequence: u32,
    },
    Reactivate {
        tile_id: u16,
        content_revision: u32,
        validation_sequence: u32,
    },
    InvalidateValidated {
        tile_id: u16,
        content_revision: u32,
        validation_sequence: u32,
    },
}

pub const fn hybrid_operation_wire_bytes(operation: &HybridTileOperation) -> usize {
    match operation {
        HybridTileOperation::Clear | HybridTileOperation::Invalidate { .. } => 4,
        HybridTileOperation::Upsert { png, .. } => 16 + png.len(),
        HybridTileOperation::HevcUpsert { access_unit, .. } => 16 + access_unit.len(),
        HybridTileOperation::Suppress { .. }
        | HybridTileOperation::Reactivate { .. }
        | HybridTileOperation::InvalidateValidated { .. } => 12,
    }
}

fn is_self_contained_hevc_idr(access_unit: &[u8]) -> bool {
    if !(access_unit.starts_with(&[0, 0, 1]) || access_unit.starts_with(&[0, 0, 0, 1])) {
        return false;
    }
    let mut cursor = 0usize;
    let mut saw_vps = false;
    let mut saw_sps = false;
    let mut saw_pps = false;
    let mut saw_idr = false;
    while cursor + 3 <= access_unit.len() {
        let start_code_bytes = if access_unit[cursor..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if access_unit[cursor..].starts_with(&[0, 0, 1]) {
            3
        } else {
            cursor += 1;
            continue;
        };
        let nal_offset = cursor + start_code_bytes;
        if nal_offset + 2 > access_unit.len() {
            return false;
        }
        match (access_unit[nal_offset] >> 1) & 0x3f {
            19 | 20 => saw_idr = true,
            32 => saw_vps = true,
            33 => saw_sps = true,
            34 => saw_pps = true,
            _ => {}
        }
        cursor = nal_offset + 2;
    }
    saw_vps && saw_sps && saw_pps && saw_idr
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HybridFrame {
    pub generation: u32,
    pub sequence: u32,
    pub crop_revision: u32,
    pub access_unit: Vec<u8>,
    pub operations: Vec<HybridTileOperation>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StaticTilePlan {
    pub clear: bool,
    pub invalidated: Vec<u16>,
    pub candidates: Vec<(u16, DamageRect)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportGeometry {
    pub generation: u32,
    pub crop_revision: u32,
    pub source: DamageRect,
    pub output_width: u32,
    pub output_height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuiescentTileCandidate {
    pub tile_id: u16,
    pub source: DamageRect,
    pub output: DamageRect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuiescentValidationRequest {
    pub tile_id: u16,
    pub source: DamageRect,
    pub content_revision: u32,
    pub generation: u32,
    pub crop_revision: u32,
    pub validation_sequence: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuiescentValidationVerdict {
    pub request: QuiescentValidationRequest,
    pub changed: bool,
    pub age_frames: u32,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct QuiescentTilePlan {
    pub clear: bool,
    pub history_preserved: bool,
    pub output_changed: bool,
    pub motion_bypass: bool,
    pub active_before_planning: usize,
    pub changed_cells: usize,
    pub total_cells: usize,
    pub edge_score_cells: usize,
    pub expected_edge_score_cells: usize,
    pub stability_zero_cells: usize,
    pub stability_one_cells: usize,
    pub stability_two_cells: usize,
    pub stability_three_plus_cells: usize,
    pub fully_stable_blocks: usize,
    pub edge_eligible_blocks: usize,
    pub preserved_candidates: usize,
    pub rejected_damage: usize,
    pub rejected_partial_visibility: usize,
    pub rejected_mapping: usize,
    pub rejected_dimension: usize,
    pub invalidated: Vec<u16>,
    pub candidates: Vec<QuiescentTileCandidate>,
}

#[derive(Debug, Default)]
pub struct QuiescentTilePlanner {
    source_width: u32,
    source_height: u32,
    stable_frames: Vec<u8>,
    output: Option<ViewportGeometry>,
    active: BTreeMap<u16, QuiescentActiveTile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QuiescentActiveTile {
    source: DamageRect,
    content_revision: u32,
    png_bytes: usize,
    suppression_sequence: Option<u32>,
    validation_sequence: Option<u32>,
}

impl QuiescentTilePlanner {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn mark_output_stale(&mut self) {
        self.output = None;
    }

    pub fn reject_candidate(&mut self, tile_id: u16) {
        self.active.remove(&tile_id);
    }

    pub fn commit_candidate(
        &mut self,
        tile_id: u16,
        content_revision: u32,
        png_bytes: usize,
    ) -> bool {
        let Some(tile) = self.active.get_mut(&tile_id) else {
            return false;
        };
        tile.content_revision = content_revision;
        tile.png_bytes = png_bytes;
        tile.suppression_sequence = None;
        tile.validation_sequence = None;
        true
    }

    pub fn validation_requests(
        &self,
        viewport: ViewportGeometry,
        validation_sequence: u32,
        capture_damage: Option<&[DamageRect]>,
        is_idle_snapshot: bool,
    ) -> Vec<QuiescentValidationRequest> {
        if is_idle_snapshot || self.output != Some(viewport) {
            return Vec::new();
        }
        self.active
            .iter()
            .filter(|(_, tile)| {
                tile.content_revision != 0
                    && capture_damage.is_none_or(|damage| {
                        damage.iter().any(|changed| {
                            tile.source.left < changed.right
                                && tile.source.right > changed.left
                                && tile.source.top < changed.bottom
                                && tile.source.bottom > changed.top
                        })
                    })
            })
            .map(|(tile_id, tile)| QuiescentValidationRequest {
                tile_id: *tile_id,
                source: tile.source,
                content_revision: tile.content_revision,
                generation: viewport.generation,
                crop_revision: viewport.crop_revision,
                validation_sequence,
            })
            .take(MAX_HYBRID_TILES)
            .collect()
    }

    pub fn suppress_requests(
        &mut self,
        requests: &[QuiescentValidationRequest],
    ) -> Vec<HybridTileOperation> {
        let Some(viewport) = self.output else {
            return Vec::new();
        };
        requests
            .iter()
            .filter_map(|request| {
                if request.generation != viewport.generation
                    || request.crop_revision != viewport.crop_revision
                {
                    return None;
                }
                let tile = self.active.get_mut(&request.tile_id)?;
                if tile.source != request.source
                    || tile.content_revision != request.content_revision
                    || tile.content_revision == 0
                {
                    return None;
                }
                let already_suppressed = tile.suppression_sequence.is_some();
                let suppression_sequence = tile
                    .suppression_sequence
                    .unwrap_or(request.validation_sequence);
                tile.suppression_sequence = Some(suppression_sequence);
                tile.validation_sequence = Some(request.validation_sequence);
                (!already_suppressed).then_some(HybridTileOperation::Suppress {
                    tile_id: request.tile_id,
                    content_revision: request.content_revision,
                    validation_sequence: suppression_sequence,
                })
            })
            .collect()
    }

    pub fn resolve_validation(
        &mut self,
        viewport: ViewportGeometry,
        verdict: &QuiescentValidationVerdict,
    ) -> Option<HybridTileOperation> {
        if self.output != Some(viewport)
            || verdict.request.generation != viewport.generation
            || verdict.request.crop_revision != viewport.crop_revision
        {
            return None;
        }
        let tile = self.active.get(&verdict.request.tile_id)?;
        if tile.source != verdict.request.source
            || tile.content_revision != verdict.request.content_revision
        {
            return None;
        }
        let current_validation_sequence = tile.validation_sequence?;
        let suppression_sequence = tile.suppression_sequence?;
        if !verdict.changed && current_validation_sequence != verdict.request.validation_sequence {
            return None;
        }
        if verdict.changed {
            self.active.remove(&verdict.request.tile_id);
            Some(HybridTileOperation::InvalidateValidated {
                tile_id: verdict.request.tile_id,
                content_revision: verdict.request.content_revision,
                validation_sequence: suppression_sequence,
            })
        } else {
            let tile = self
                .active
                .get_mut(&verdict.request.tile_id)
                .expect("validated tile disappeared");
            tile.suppression_sequence = None;
            tile.validation_sequence = None;
            Some(HybridTileOperation::Reactivate {
                tile_id: verdict.request.tile_id,
                content_revision: verdict.request.content_revision,
                validation_sequence: suppression_sequence,
            })
        }
    }

    pub fn validation_verdict_superseded(
        &self,
        viewport: ViewportGeometry,
        verdict: &QuiescentValidationVerdict,
    ) -> bool {
        if self.output != Some(viewport)
            || verdict.request.generation != viewport.generation
            || verdict.request.crop_revision != viewport.crop_revision
        {
            return false;
        }
        self.active
            .get(&verdict.request.tile_id)
            .is_some_and(|tile| {
                tile.source == verdict.request.source
                    && tile.content_revision == verdict.request.content_revision
                    && tile
                        .validation_sequence
                        .is_some_and(|sequence| sequence != verdict.request.validation_sequence)
            })
    }

    pub fn suppressed_tile_count(&self) -> usize {
        self.active
            .values()
            .filter(|tile| tile.suppression_sequence.is_some())
            .count()
    }

    pub fn retained_png_bytes(&self) -> usize {
        self.active.values().map(|tile| tile.png_bytes).sum()
    }

    pub fn cached_png_bytes(&self, tile_id: u16) -> Option<usize> {
        self.active.get(&tile_id).map(|tile| tile.png_bytes)
    }

    pub fn plan(
        &mut self,
        source_width: u32,
        source_height: u32,
        viewport: ViewportGeometry,
        damage: &[DamageRect],
        edge_scores: &[u32],
    ) -> QuiescentTilePlan {
        self.plan_with_immediate_damage(
            source_width,
            source_height,
            viewport,
            damage,
            damage,
            edge_scores,
        )
    }

    pub fn plan_with_immediate_damage(
        &mut self,
        source_width: u32,
        source_height: u32,
        viewport: ViewportGeometry,
        history_damage: &[DamageRect],
        immediate_damage: &[DamageRect],
        edge_scores: &[u32],
    ) -> QuiescentTilePlan {
        const CTB: u32 = 32;
        const TILE: u32 = 128;
        let columns = source_width.div_ceil(CTB);
        let rows = source_height.div_ceil(CTB);
        let total = columns.saturating_mul(rows) as usize;
        let source_reset = self.source_width != source_width
            || self.source_height != source_height
            || self.stable_frames.len() != total;
        if source_reset {
            self.source_width = source_width;
            self.source_height = source_height;
            self.stable_frames = vec![0; total];
            self.output = None;
            self.active.clear();
        }
        let output_changed = self.output != Some(viewport);
        let active_before_planning = self.active.len();
        let active_before_output_change = if output_changed && !source_reset {
            std::mem::take(&mut self.active)
        } else {
            BTreeMap::new()
        };
        if output_changed {
            self.output = Some(viewport);
        }
        let mut plan = QuiescentTilePlan {
            clear: source_reset || output_changed,
            history_preserved: output_changed && !source_reset,
            output_changed,
            active_before_planning,
            total_cells: total,
            edge_score_cells: edge_scores.len(),
            expected_edge_score_cells: total,
            stability_zero_cells: self
                .stable_frames
                .iter()
                .filter(|stable| **stable == 0)
                .count(),
            stability_one_cells: self
                .stable_frames
                .iter()
                .filter(|stable| **stable == 1)
                .count(),
            stability_two_cells: self
                .stable_frames
                .iter()
                .filter(|stable| **stable == 2)
                .count(),
            stability_three_plus_cells: self
                .stable_frames
                .iter()
                .filter(|stable| **stable >= 3)
                .count(),
            ..QuiescentTilePlan::default()
        };
        if source_width == 0
            || source_height == 0
            || edge_scores.len() != total
            || viewport.source.left >= viewport.source.right
            || viewport.source.top >= viewport.source.bottom
            || viewport.source.right > source_width
            || viewport.source.bottom > source_height
            || viewport.output_width == 0
            || viewport.output_height == 0
        {
            return plan;
        }

        let intersects = |left: DamageRect, right: DamageRect| {
            left.left < right.right
                && left.right > right.left
                && left.top < right.bottom
                && left.bottom > right.top
        };
        if !output_changed {
            let invalidated = self
                .active
                .iter()
                .filter_map(|(tile_id, tile)| {
                    immediate_damage
                        .iter()
                        .any(|changed| intersects(tile.source, *changed))
                        .then_some(*tile_id)
                })
                .collect::<Vec<_>>();
            for tile_id in &invalidated {
                self.active.remove(tile_id);
            }
            plan.invalidated = invalidated;
        }

        let mut changed = vec![false; total];
        for rect in history_damage {
            let left = (rect.left / CTB).min(columns);
            let top = (rect.top / CTB).min(rows);
            let right = rect.right.div_ceil(CTB).min(columns);
            let bottom = rect.bottom.div_ceil(CTB).min(rows);
            for row in top..bottom {
                for column in left..right {
                    changed[(row * columns + column) as usize] = true;
                }
            }
        }
        let changed_count = changed.iter().filter(|value| **value).count();
        plan.changed_cells = changed_count;
        for (index, stable) in self.stable_frames.iter_mut().enumerate() {
            *stable = if changed[index] {
                0
            } else {
                stable.saturating_add(1)
            };
        }
        plan.stability_zero_cells = self
            .stable_frames
            .iter()
            .filter(|stable| **stable == 0)
            .count();
        plan.stability_one_cells = self
            .stable_frames
            .iter()
            .filter(|stable| **stable == 1)
            .count();
        plan.stability_two_cells = self
            .stable_frames
            .iter()
            .filter(|stable| **stable == 2)
            .count();
        plan.stability_three_plus_cells = self
            .stable_frames
            .iter()
            .filter(|stable| **stable >= 3)
            .count();
        plan.motion_bypass = total > 0 && changed_count.saturating_mul(2) >= total;
        if plan.motion_bypass {
            return plan;
        }

        if output_changed {
            for (tile_id, tile) in active_before_output_change {
                let source = tile.source;
                if plan.candidates.len() >= MAX_HYBRID_TILES {
                    break;
                }
                if immediate_damage
                    .iter()
                    .any(|changed| intersects(source, *changed))
                {
                    plan.rejected_damage = plan.rejected_damage.saturating_add(1);
                    continue;
                }
                if source.left < viewport.source.left
                    || source.top < viewport.source.top
                    || source.right > viewport.source.right
                    || source.bottom > viewport.source.bottom
                {
                    plan.rejected_partial_visibility =
                        plan.rejected_partial_visibility.saturating_add(1);
                    continue;
                }
                let Some(output) = map_source_tile(source, viewport) else {
                    plan.rejected_mapping = plan.rejected_mapping.saturating_add(1);
                    continue;
                };
                if output.right - output.left > u32::from(MAX_HYBRID_TILE_DIMENSION)
                    || output.bottom - output.top > u32::from(MAX_HYBRID_TILE_DIMENSION)
                {
                    plan.rejected_dimension = plan.rejected_dimension.saturating_add(1);
                    continue;
                }
                self.active.insert(
                    tile_id,
                    QuiescentActiveTile {
                        source,
                        content_revision: 0,
                        png_bytes: 0,
                        suppression_sequence: None,
                        validation_sequence: None,
                    },
                );
                plan.preserved_candidates = plan.preserved_candidates.saturating_add(1);
                plan.candidates.push(QuiescentTileCandidate {
                    tile_id,
                    source,
                    output,
                });
            }
        }

        let block_columns = source_width.div_ceil(TILE);
        let block_rows = source_height.div_ceil(TILE);
        let mut ranked = Vec::<(u16, u64, DamageRect, DamageRect)>::new();
        for block_row in 0..block_rows {
            for block_column in 0..block_columns {
                let source = DamageRect {
                    left: block_column * TILE,
                    top: block_row * TILE,
                    right: ((block_column + 1) * TILE).min(source_width),
                    bottom: ((block_row + 1) * TILE).min(source_height),
                };
                if source.left < viewport.source.left
                    || source.top < viewport.source.top
                    || source.right > viewport.source.right
                    || source.bottom > viewport.source.bottom
                {
                    continue;
                }
                let mut score = 0u64;
                let mut stable = true;
                for row in source.top / CTB..source.bottom.div_ceil(CTB) {
                    for column in source.left / CTB..source.right.div_ceil(CTB) {
                        let index = (row * columns + column) as usize;
                        stable &= self.stable_frames[index] >= 3;
                        score = score.saturating_add(u64::from(edge_scores[index]));
                    }
                }
                if !stable {
                    continue;
                }
                plan.fully_stable_blocks = plan.fully_stable_blocks.saturating_add(1);
                if score == 0 {
                    continue;
                }
                plan.edge_eligible_blocks = plan.edge_eligible_blocks.saturating_add(1);
                let Some(output) = map_source_tile(source, viewport) else {
                    continue;
                };
                if output.right - output.left > u32::from(MAX_HYBRID_TILE_DIMENSION)
                    || output.bottom - output.top > u32::from(MAX_HYBRID_TILE_DIMENSION)
                {
                    continue;
                }
                let tile_id =
                    (block_row * block_columns + block_column + 1).min(u32::from(u16::MAX)) as u16;
                ranked.push((tile_id, score, source, output));
            }
        }
        ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        if !output_changed && self.active.len() == MAX_HYBRID_TILES {
            let scores_by_tile = ranked
                .iter()
                .map(|(tile_id, score, _, _)| (*tile_id, *score))
                .collect::<BTreeMap<_, _>>();
            for (tile_id, candidate_score, source, output) in ranked
                .iter()
                .filter(|(tile_id, _, _, _)| !self.active.contains_key(tile_id))
                .cloned()
                .collect::<Vec<_>>()
            {
                let Some((weakest_tile_id, weakest_score)) = self
                    .active
                    .iter()
                    .filter(|(_, tile)| {
                        tile.content_revision != 0
                            && tile.suppression_sequence.is_none()
                            && tile.validation_sequence.is_none()
                    })
                    .map(|(active_tile_id, _)| {
                        (
                            *active_tile_id,
                            scores_by_tile
                                .get(active_tile_id)
                                .copied()
                                .unwrap_or_default(),
                        )
                    })
                    .min_by_key(|(active_tile_id, score)| (*score, *active_tile_id))
                else {
                    break;
                };
                let materially_better = candidate_score > weakest_score
                    && u128::from(candidate_score)
                        .saturating_mul(QUIESCENT_REBALANCE_GAIN_DENOMINATOR)
                        >= u128::from(weakest_score)
                            .saturating_mul(QUIESCENT_REBALANCE_GAIN_NUMERATOR);
                if !materially_better {
                    break;
                }
                self.active.remove(&weakest_tile_id);
                plan.invalidated.push(weakest_tile_id);
                self.active.insert(
                    tile_id,
                    QuiescentActiveTile {
                        source,
                        content_revision: 0,
                        png_bytes: 0,
                        suppression_sequence: None,
                        validation_sequence: None,
                    },
                );
                plan.candidates.push(QuiescentTileCandidate {
                    tile_id,
                    source,
                    output,
                });
            }
        }
        let capacity = MAX_HYBRID_TILES.saturating_sub(self.active.len());
        for (tile_id, _, source, output) in ranked {
            if plan.candidates.len() >= capacity {
                break;
            }
            if self.active.contains_key(&tile_id) {
                continue;
            }
            self.active.insert(
                tile_id,
                QuiescentActiveTile {
                    source,
                    content_revision: 0,
                    png_bytes: 0,
                    suppression_sequence: None,
                    validation_sequence: None,
                },
            );
            plan.candidates.push(QuiescentTileCandidate {
                tile_id,
                source,
                output,
            });
        }
        plan
    }
}

fn map_source_tile(source: DamageRect, viewport: ViewportGeometry) -> Option<DamageRect> {
    let crop_width = viewport.source.right.checked_sub(viewport.source.left)?;
    let crop_height = viewport.source.bottom.checked_sub(viewport.source.top)?;
    let relative_left = source.left.checked_sub(viewport.source.left)?;
    let relative_top = source.top.checked_sub(viewport.source.top)?;
    let relative_right = source.right.checked_sub(viewport.source.left)?;
    let relative_bottom = source.bottom.checked_sub(viewport.source.top)?;
    let left = relative_left.saturating_mul(viewport.output_width) / crop_width;
    let top = relative_top.saturating_mul(viewport.output_height) / crop_height;
    let right = relative_right
        .saturating_mul(viewport.output_width)
        .div_ceil(crop_width);
    let bottom = relative_bottom
        .saturating_mul(viewport.output_height)
        .div_ceil(crop_height);
    (left < right
        && top < bottom
        && right <= viewport.output_width
        && bottom <= viewport.output_height)
        .then_some(DamageRect {
            left,
            top,
            right,
            bottom,
        })
}

#[derive(Debug, Default)]
pub struct StaticTilePlanner {
    generation: Option<u32>,
    crop_revision: u32,
    width: u32,
    height: u32,
    stable_frames: Vec<u8>,
    active: BTreeMap<u16, DamageRect>,
}

impl StaticTilePlanner {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn reject_candidate(&mut self, tile_id: u16) {
        self.active.remove(&tile_id);
    }

    pub fn plan(
        &mut self,
        generation: u32,
        crop_revision: u32,
        width: u32,
        height: u32,
        damage: &[DamageRect],
        edge_scores: &[u32],
    ) -> StaticTilePlan {
        const CTB: u32 = 32;
        const TILE: u32 = 128;
        let columns = width.div_ceil(CTB);
        let rows = height.div_ceil(CTB);
        let total = (columns * rows) as usize;
        let reset = self.generation != Some(generation)
            || self.crop_revision != crop_revision
            || self.width != width
            || self.height != height;
        if reset {
            self.generation = Some(generation);
            self.crop_revision = crop_revision;
            self.width = width;
            self.height = height;
            self.stable_frames = vec![0; total];
            self.active.clear();
        }
        let mut plan = StaticTilePlan {
            clear: reset,
            ..StaticTilePlan::default()
        };
        if width == 0 || height == 0 || edge_scores.len() != total {
            return plan;
        }
        let intersects = |left: DamageRect, right: DamageRect| {
            left.left < right.right
                && left.right > right.left
                && left.top < right.bottom
                && left.bottom > right.top
        };
        let invalidated = self
            .active
            .iter()
            .filter_map(|(tile_id, rect)| {
                damage
                    .iter()
                    .any(|changed| intersects(*rect, *changed))
                    .then_some(*tile_id)
            })
            .collect::<Vec<_>>();
        for tile_id in &invalidated {
            self.active.remove(tile_id);
        }
        plan.invalidated = invalidated;

        let mut changed = vec![false; total];
        for rect in damage {
            let left = (rect.left / CTB).min(columns);
            let top = (rect.top / CTB).min(rows);
            let right = rect.right.div_ceil(CTB).min(columns);
            let bottom = rect.bottom.div_ceil(CTB).min(rows);
            for row in top..bottom {
                for column in left..right {
                    changed[(row * columns + column) as usize] = true;
                }
            }
        }
        for (index, stable) in self.stable_frames.iter_mut().enumerate() {
            *stable = if changed[index] {
                0
            } else {
                stable.saturating_add(1)
            };
        }

        let block_columns = width.div_ceil(TILE);
        let mut unstable_blocks = BTreeSet::new();
        for row in 0..rows {
            for column in 0..columns {
                let index = (row * columns + column) as usize;
                if self.stable_frames[index] < 3 {
                    let block_column = column * CTB / TILE;
                    let block_row = row * CTB / TILE;
                    let tile_id = (block_row * block_columns + block_column + 1)
                        .min(u32::from(u16::MAX)) as u16;
                    unstable_blocks.insert(tile_id);
                }
            }
        }
        let mut blocks = BTreeMap::<u16, (u64, DamageRect)>::new();
        for row in 0..rows {
            for column in 0..columns {
                let index = (row * columns + column) as usize;
                if self.stable_frames[index] < 3 || edge_scores[index] == 0 {
                    continue;
                }
                let block_column = column * CTB / TILE;
                let block_row = row * CTB / TILE;
                let tile_id =
                    (block_row * block_columns + block_column + 1).min(u32::from(u16::MAX)) as u16;
                if unstable_blocks.contains(&tile_id) {
                    continue;
                }
                let rect = DamageRect {
                    left: block_column * TILE,
                    top: block_row * TILE,
                    right: ((block_column + 1) * TILE).min(width),
                    bottom: ((block_row + 1) * TILE).min(height),
                };
                let entry = blocks.entry(tile_id).or_insert((0, rect));
                entry.0 = entry.0.saturating_add(u64::from(edge_scores[index]));
            }
        }
        let mut ranked = blocks.into_iter().collect::<Vec<_>>();
        ranked.sort_by(|(left_id, (left_score, _)), (right_id, (right_score, _))| {
            right_score
                .cmp(left_score)
                .then_with(|| left_id.cmp(right_id))
        });
        let capacity = MAX_HYBRID_TILES.saturating_sub(self.active.len());
        for (tile_id, (_, rect)) in ranked {
            if plan.candidates.len() >= capacity {
                break;
            }
            if self.active.contains_key(&tile_id) {
                continue;
            }
            self.active.insert(tile_id, rect);
            plan.candidates.push((tile_id, rect));
        }
        plan
    }
}

pub fn encode_hybrid_frame_v1(frame: &HybridFrame) -> anyhow::Result<Vec<u8>> {
    ensure!(
        !frame.access_unit.is_empty(),
        "hybrid HEVC access unit is empty"
    );
    ensure!(
        frame.operations.len() <= HYBRID_MAX_OPERATIONS,
        "too many hybrid tile operations"
    );
    let mut sidecar = Vec::new();
    for operation in &frame.operations {
        match operation {
            HybridTileOperation::Clear => {
                sidecar.extend_from_slice(&[OP_CLEAR, 0, 0, 0]);
            }
            HybridTileOperation::Invalidate { tile_id } => {
                sidecar.extend_from_slice(&[OP_INVALIDATE, 0]);
                sidecar.extend_from_slice(&tile_id.to_be_bytes());
            }
            HybridTileOperation::Suppress {
                tile_id,
                content_revision,
                validation_sequence,
            }
            | HybridTileOperation::Reactivate {
                tile_id,
                content_revision,
                validation_sequence,
            }
            | HybridTileOperation::InvalidateValidated {
                tile_id,
                content_revision,
                validation_sequence,
            } => {
                let kind = match operation {
                    HybridTileOperation::Suppress { .. } => OP_SUPPRESS,
                    HybridTileOperation::Reactivate { .. } => OP_REACTIVATE,
                    HybridTileOperation::InvalidateValidated { .. } => OP_INVALIDATE_VALIDATED,
                    _ => unreachable!(),
                };
                sidecar.extend_from_slice(&[kind, 0]);
                sidecar.extend_from_slice(&tile_id.to_be_bytes());
                sidecar.extend_from_slice(&content_revision.to_be_bytes());
                sidecar.extend_from_slice(&validation_sequence.to_be_bytes());
            }
            HybridTileOperation::Upsert { tile_id, rect, png } => {
                let width = rect
                    .right
                    .checked_sub(rect.left)
                    .context("invalid tile width")?;
                let height = rect
                    .bottom
                    .checked_sub(rect.top)
                    .context("invalid tile height")?;
                ensure!(
                    width > 0
                        && height > 0
                        && width <= u32::from(MAX_HYBRID_TILE_DIMENSION)
                        && height <= u32::from(MAX_HYBRID_TILE_DIMENSION),
                    "hybrid tile dimensions are out of bounds"
                );
                ensure!(
                    rect.left <= u32::from(u16::MAX)
                        && rect.top <= u32::from(u16::MAX)
                        && width <= u32::from(u16::MAX)
                        && height <= u32::from(u16::MAX),
                    "hybrid tile geometry exceeds the wire format"
                );
                ensure!(
                    png.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]),
                    "hybrid tile is not PNG"
                );
                let png_len = u32::try_from(png.len()).context("hybrid PNG is too large")?;
                sidecar.extend_from_slice(&[OP_UPSERT, 0]);
                sidecar.extend_from_slice(&tile_id.to_be_bytes());
                sidecar.extend_from_slice(&(rect.left as u16).to_be_bytes());
                sidecar.extend_from_slice(&(rect.top as u16).to_be_bytes());
                sidecar.extend_from_slice(&(width as u16).to_be_bytes());
                sidecar.extend_from_slice(&(height as u16).to_be_bytes());
                sidecar.extend_from_slice(&png_len.to_be_bytes());
                sidecar.extend_from_slice(png);
            }
            HybridTileOperation::HevcUpsert {
                tile_id,
                rect,
                access_unit,
            } => {
                let width = rect
                    .right
                    .checked_sub(rect.left)
                    .context("invalid HEVC tile width")?;
                let height = rect
                    .bottom
                    .checked_sub(rect.top)
                    .context("invalid HEVC tile height")?;
                ensure!(
                    width == u32::from(HEVC_HYBRID_TILE_DIMENSION)
                        && height == u32::from(HEVC_HYBRID_TILE_DIMENSION),
                    "HEVC hybrid tiles must be exactly 128x128"
                );
                ensure!(
                    rect.left <= u32::from(u16::MAX) && rect.top <= u32::from(u16::MAX),
                    "HEVC tile geometry exceeds the wire format"
                );
                ensure!(
                    is_self_contained_hevc_idr(access_unit),
                    "HEVC hybrid tile is not a self-contained IDR"
                );
                let access_unit_len = u32::try_from(access_unit.len())
                    .context("HEVC tile access unit is too large")?;
                sidecar.extend_from_slice(&[OP_HEVC_UPSERT, 0]);
                sidecar.extend_from_slice(&tile_id.to_be_bytes());
                sidecar.extend_from_slice(&(rect.left as u16).to_be_bytes());
                sidecar.extend_from_slice(&(rect.top as u16).to_be_bytes());
                sidecar.extend_from_slice(&HEVC_HYBRID_TILE_DIMENSION.to_be_bytes());
                sidecar.extend_from_slice(&HEVC_HYBRID_TILE_DIMENSION.to_be_bytes());
                sidecar.extend_from_slice(&access_unit_len.to_be_bytes());
                sidecar.extend_from_slice(access_unit);
            }
        }
        ensure!(
            sidecar.len() <= MAX_HYBRID_SIDECAR_BYTES,
            "hybrid sidecar exceeds the per-frame cap"
        );
    }
    let access_unit_len =
        u32::try_from(frame.access_unit.len()).context("hybrid access unit is too large")?;
    let sidecar_len = u32::try_from(sidecar.len()).context("hybrid sidecar is too large")?;
    let mut encoded =
        Vec::with_capacity(HYBRID_HEADER_BYTES + sidecar.len() + frame.access_unit.len());
    encoded.extend_from_slice(&HYBRID_FRAME_MAGIC);
    encoded.push(HYBRID_FRAME_VERSION);
    encoded.push(frame.operations.len() as u8);
    encoded.extend_from_slice(&0u16.to_be_bytes());
    encoded.extend_from_slice(&frame.generation.to_be_bytes());
    encoded.extend_from_slice(&frame.sequence.to_be_bytes());
    encoded.extend_from_slice(&frame.crop_revision.to_be_bytes());
    encoded.extend_from_slice(&access_unit_len.to_be_bytes());
    encoded.extend_from_slice(&sidecar_len.to_be_bytes());
    encoded.extend_from_slice(&0u32.to_be_bytes());
    encoded.extend_from_slice(&sidecar);
    encoded.extend_from_slice(&frame.access_unit);
    Ok(encoded)
}

pub fn decode_hybrid_frame_v1(bytes: &[u8]) -> Option<HybridFrame> {
    if bytes.len() < HYBRID_HEADER_BYTES || bytes[..4] != HYBRID_FRAME_MAGIC {
        return None;
    }
    if bytes[4] != HYBRID_FRAME_VERSION || bytes[6..8] != [0, 0] || bytes[28..32] != [0; 4] {
        return None;
    }
    let operation_count = bytes[5] as usize;
    if operation_count > HYBRID_MAX_OPERATIONS {
        return None;
    }
    let generation = u32::from_be_bytes(bytes[8..12].try_into().ok()?);
    let sequence = u32::from_be_bytes(bytes[12..16].try_into().ok()?);
    let crop_revision = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let access_unit_len = u32::from_be_bytes(bytes[20..24].try_into().ok()?) as usize;
    let sidecar_len = u32::from_be_bytes(bytes[24..28].try_into().ok()?) as usize;
    if access_unit_len == 0
        || sidecar_len > MAX_HYBRID_SIDECAR_BYTES
        || HYBRID_HEADER_BYTES
            .checked_add(sidecar_len)?
            .checked_add(access_unit_len)?
            != bytes.len()
    {
        return None;
    }
    let sidecar_end = HYBRID_HEADER_BYTES + sidecar_len;
    let mut cursor = HYBRID_HEADER_BYTES;
    let mut operations = Vec::with_capacity(operation_count);
    for _ in 0..operation_count {
        if cursor + 4 > sidecar_end || bytes[cursor + 1] != 0 {
            return None;
        }
        let kind = bytes[cursor];
        let tile_id = u16::from_be_bytes(bytes[cursor + 2..cursor + 4].try_into().ok()?);
        cursor += 4;
        match kind {
            OP_CLEAR if tile_id == 0 => operations.push(HybridTileOperation::Clear),
            OP_INVALIDATE => operations.push(HybridTileOperation::Invalidate { tile_id }),
            OP_SUPPRESS | OP_REACTIVATE | OP_INVALIDATE_VALIDATED => {
                if cursor + 8 > sidecar_end {
                    return None;
                }
                let content_revision =
                    u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().ok()?);
                let validation_sequence =
                    u32::from_be_bytes(bytes[cursor + 4..cursor + 8].try_into().ok()?);
                cursor += 8;
                operations.push(match kind {
                    OP_SUPPRESS => HybridTileOperation::Suppress {
                        tile_id,
                        content_revision,
                        validation_sequence,
                    },
                    OP_REACTIVATE => HybridTileOperation::Reactivate {
                        tile_id,
                        content_revision,
                        validation_sequence,
                    },
                    OP_INVALIDATE_VALIDATED => HybridTileOperation::InvalidateValidated {
                        tile_id,
                        content_revision,
                        validation_sequence,
                    },
                    _ => unreachable!(),
                });
            }
            OP_UPSERT => {
                if cursor + 12 > sidecar_end {
                    return None;
                }
                let left = u16::from_be_bytes(bytes[cursor..cursor + 2].try_into().ok()?) as u32;
                let top = u16::from_be_bytes(bytes[cursor + 2..cursor + 4].try_into().ok()?) as u32;
                let width =
                    u16::from_be_bytes(bytes[cursor + 4..cursor + 6].try_into().ok()?) as u32;
                let height =
                    u16::from_be_bytes(bytes[cursor + 6..cursor + 8].try_into().ok()?) as u32;
                let png_len =
                    u32::from_be_bytes(bytes[cursor + 8..cursor + 12].try_into().ok()?) as usize;
                cursor += 12;
                if width == 0
                    || height == 0
                    || width > u32::from(MAX_HYBRID_TILE_DIMENSION)
                    || height > u32::from(MAX_HYBRID_TILE_DIMENSION)
                    || cursor.checked_add(png_len)? > sidecar_end
                {
                    return None;
                }
                let png = bytes[cursor..cursor + png_len].to_vec();
                if !png.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]) {
                    return None;
                }
                cursor += png_len;
                operations.push(HybridTileOperation::Upsert {
                    tile_id,
                    rect: DamageRect {
                        left,
                        top,
                        right: left.checked_add(width)?,
                        bottom: top.checked_add(height)?,
                    },
                    png,
                });
            }
            OP_HEVC_UPSERT => {
                if cursor + 12 > sidecar_end {
                    return None;
                }
                let left = u16::from_be_bytes(bytes[cursor..cursor + 2].try_into().ok()?) as u32;
                let top = u16::from_be_bytes(bytes[cursor + 2..cursor + 4].try_into().ok()?) as u32;
                let width =
                    u16::from_be_bytes(bytes[cursor + 4..cursor + 6].try_into().ok()?) as u32;
                let height =
                    u16::from_be_bytes(bytes[cursor + 6..cursor + 8].try_into().ok()?) as u32;
                let access_unit_len =
                    u32::from_be_bytes(bytes[cursor + 8..cursor + 12].try_into().ok()?) as usize;
                cursor += 12;
                if width != u32::from(HEVC_HYBRID_TILE_DIMENSION)
                    || height != u32::from(HEVC_HYBRID_TILE_DIMENSION)
                    || cursor.checked_add(access_unit_len)? > sidecar_end
                {
                    return None;
                }
                let access_unit = bytes[cursor..cursor + access_unit_len].to_vec();
                if !is_self_contained_hevc_idr(&access_unit) {
                    return None;
                }
                cursor += access_unit_len;
                operations.push(HybridTileOperation::HevcUpsert {
                    tile_id,
                    rect: DamageRect {
                        left,
                        top,
                        right: left.checked_add(width)?,
                        bottom: top.checked_add(height)?,
                    },
                    access_unit,
                });
            }
            _ => return None,
        }
    }
    if cursor != sidecar_end {
        return None;
    }
    Some(HybridFrame {
        generation,
        sequence,
        crop_revision,
        access_unit: bytes[sidecar_end..].to_vec(),
        operations,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        decode_hybrid_frame_v1, encode_hybrid_frame_v1, hybrid_sidecar_budget, HybridFrame,
        HybridTileOperation, HybridWireBudget, QuiescentTilePlanner, QuiescentValidationRequest,
        QuiescentValidationVerdict, StaticTilePlanner, ViewportGeometry, MAX_HYBRID_SIDECAR_BYTES,
    };
    use crate::frame_damage::DamageRect;

    #[test]
    fn hybrid_frame_v1_decodes_a_self_contained_hevc_tile_upsert() {
        let tile_access_unit = vec![
            0, 0, 0, 1, 64, 1, // VPS (NAL type 32)
            0, 0, 0, 1, 66, 1, // SPS (NAL type 33)
            0, 0, 0, 1, 68, 1, // PPS (NAL type 34)
            0, 0, 0, 1, 38, 1, // IDR_W_RADL (NAL type 19)
        ];
        let mut sidecar = vec![7, 0, 0, 9];
        sidecar.extend_from_slice(&64u16.to_be_bytes());
        sidecar.extend_from_slice(&96u16.to_be_bytes());
        sidecar.extend_from_slice(&128u16.to_be_bytes());
        sidecar.extend_from_slice(&128u16.to_be_bytes());
        sidecar.extend_from_slice(&(tile_access_unit.len() as u32).to_be_bytes());
        sidecar.extend_from_slice(&tile_access_unit);

        let base_access_unit = vec![0, 0, 0, 1, 38, 1];
        let mut encoded = Vec::new();
        encoded.extend_from_slice(&super::HYBRID_FRAME_MAGIC);
        encoded.push(super::HYBRID_FRAME_VERSION);
        encoded.push(1);
        encoded.extend_from_slice(&0u16.to_be_bytes());
        encoded.extend_from_slice(&7u32.to_be_bytes());
        encoded.extend_from_slice(&41u32.to_be_bytes());
        encoded.extend_from_slice(&3u32.to_be_bytes());
        encoded.extend_from_slice(&(base_access_unit.len() as u32).to_be_bytes());
        encoded.extend_from_slice(&(sidecar.len() as u32).to_be_bytes());
        encoded.extend_from_slice(&0u32.to_be_bytes());
        encoded.extend_from_slice(&sidecar);
        encoded.extend_from_slice(&base_access_unit);

        let decoded = decode_hybrid_frame_v1(&encoded)
            .expect("self-contained HEVC tile upsert should decode");
        assert_eq!(decoded.operations.len(), 1);
    }

    #[test]
    fn hybrid_frame_v1_round_trips_only_self_contained_hevc_tile_idrs() {
        let self_contained_idr = vec![
            0, 0, 0, 1, 64, 1, 0, 0, 0, 1, 66, 1, 0, 0, 0, 1, 68, 1, 0, 0, 0, 1, 38, 1,
        ];
        let frame = HybridFrame {
            generation: 7,
            sequence: 41,
            crop_revision: 3,
            access_unit: vec![0, 0, 0, 1, 38, 1],
            operations: vec![HybridTileOperation::HevcUpsert {
                tile_id: 9,
                rect: DamageRect {
                    left: 64,
                    top: 96,
                    right: 192,
                    bottom: 224,
                },
                access_unit: self_contained_idr,
            }],
        };
        let encoded = encode_hybrid_frame_v1(&frame).expect("self-contained tile should encode");
        assert_eq!(decode_hybrid_frame_v1(&encoded), Some(frame.clone()));

        let mut non_self_contained = frame;
        let HybridTileOperation::HevcUpsert { access_unit, .. } =
            &mut non_self_contained.operations[0]
        else {
            unreachable!()
        };
        access_unit.drain(..18);
        assert!(encode_hybrid_frame_v1(&non_self_contained).is_err());
    }

    #[test]
    fn hybrid_frame_v1_round_trips_hevc_and_bounded_tile_operations() {
        let frame = HybridFrame {
            generation: 7,
            sequence: 41,
            crop_revision: 3,
            access_unit: vec![0, 0, 0, 1, 38, 1, 2, 3],
            operations: vec![
                HybridTileOperation::Clear,
                HybridTileOperation::Upsert {
                    tile_id: 9,
                    rect: DamageRect {
                        left: 64,
                        top: 96,
                        right: 192,
                        bottom: 224,
                    },
                    png: vec![137, 80, 78, 71, 13, 10, 26, 10, 1, 2, 3],
                },
                HybridTileOperation::Invalidate { tile_id: 9 },
            ],
        };

        let encoded = encode_hybrid_frame_v1(&frame).expect("bounded hybrid frame should encode");
        assert_eq!(decode_hybrid_frame_v1(&encoded), Some(frame));
    }

    #[test]
    fn hybrid_frame_v1_round_trips_cache_suppression_and_exact_validation_operations() {
        let frame = HybridFrame {
            generation: 7,
            sequence: 42,
            crop_revision: 3,
            access_unit: vec![0, 0, 0, 1, 38],
            operations: vec![
                HybridTileOperation::Suppress {
                    tile_id: 9,
                    content_revision: 41,
                    validation_sequence: 42,
                },
                HybridTileOperation::Reactivate {
                    tile_id: 10,
                    content_revision: 39,
                    validation_sequence: 41,
                },
                HybridTileOperation::InvalidateValidated {
                    tile_id: 11,
                    content_revision: 40,
                    validation_sequence: 41,
                },
            ],
        };

        let encoded = encode_hybrid_frame_v1(&frame).expect("bounded validation operations encode");
        assert_eq!(decode_hybrid_frame_v1(&encoded), Some(frame));
    }

    #[test]
    fn hybrid_frame_v1_rejects_sidecars_above_the_per_frame_cap() {
        let frame = HybridFrame {
            generation: 1,
            sequence: 1,
            crop_revision: 0,
            access_unit: vec![1],
            operations: vec![HybridTileOperation::Upsert {
                tile_id: 1,
                rect: DamageRect {
                    left: 0,
                    top: 0,
                    right: 32,
                    bottom: 32,
                },
                png: vec![0; MAX_HYBRID_SIDECAR_BYTES + 1],
            }],
        };

        assert!(encode_hybrid_frame_v1(&frame).is_err());
    }

    #[test]
    fn hybrid_sidecar_budget_debits_base_video_and_fec_from_the_total_wss_cap() {
        assert_eq!(hybrid_sidecar_budget(6_000_000, 30, 16, 15, 20_000), 3_437);
        assert_eq!(hybrid_sidecar_budget(6_000_000, 30, 16, 15, 24_000), 0);
        assert_eq!(hybrid_sidecar_budget(6_000_000, 0, 16, 15, 0), 0);
    }

    #[test]
    fn hybrid_wire_budget_carries_static_headroom_into_a_transition_idr() {
        let mut budget = HybridWireBudget::default();
        assert_eq!(budget.begin_frame(6_000_000, 30, 16, 15, 1_000), 22_437);
        budget.commit_sidecar(4_000);
        assert_eq!(budget.begin_frame(6_000_000, 30, 16, 15, 30_000), 11_874);
        budget.commit_sidecar(10_000);
        assert_eq!(budget.carried_bytes(), 1_874);
    }

    #[test]
    fn hybrid_wire_budget_retains_quiet_credit_for_four_bounded_transition_bursts() {
        let mut budget = HybridWireBudget::default();
        for _ in 0..60 {
            assert!(budget.begin_frame(6_000_000, 30, 16, 15, 0) <= MAX_HYBRID_SIDECAR_BYTES);
        }

        for _ in 0..4 {
            let available = budget.begin_frame(6_000_000, 30, 16, 15, 36_000);
            assert!(available >= 64 * 1024);
            assert!(available <= MAX_HYBRID_SIDECAR_BYTES);
            budget.commit_sidecar(64 * 1024);
        }
    }

    #[test]
    fn hybrid_wire_budget_resets_when_the_cap_or_topology_changes() {
        let mut budget = HybridWireBudget::default();
        assert!(budget.begin_frame(6_000_000, 30, 16, 15, 1_000) > 0);
        assert_eq!(budget.begin_frame(4_000_000, 30, 16, 15, 20_000), 0);
        assert_eq!(budget.carried_bytes(), 0);
    }

    #[test]
    fn static_detail_tiles_require_three_stable_frames_and_invalidate_on_damage() {
        let mut planner = StaticTilePlanner::default();
        let edges = vec![100; 32 * 18];
        let first = planner.plan(7, 0, 1024, 576, &[], &edges);
        assert!(first.clear);
        assert!(first.candidates.is_empty());
        assert!(planner
            .plan(7, 0, 1024, 576, &[], &edges)
            .candidates
            .is_empty());
        let promoted = planner.plan(7, 0, 1024, 576, &[], &edges);
        assert_eq!(promoted.candidates.len(), 4);
        assert!(promoted
            .candidates
            .iter()
            .all(|(_, rect)| rect.right - rect.left <= 512 && rect.bottom - rect.top <= 512));

        let changed = promoted.candidates[0].1;
        let invalidated = planner.plan(7, 0, 1024, 576, &[changed], &edges);
        assert!(invalidated.invalidated.contains(&promoted.candidates[0].0));
    }

    #[test]
    fn decoder_reset_clears_and_reissues_stable_detail_tiles() {
        let mut planner = StaticTilePlanner::default();
        let edges = vec![100; 32 * 18];
        assert!(planner
            .plan(7, 0, 1024, 576, &[], &edges)
            .candidates
            .is_empty());
        assert!(planner
            .plan(7, 0, 1024, 576, &[], &edges)
            .candidates
            .is_empty());
        assert_eq!(
            planner.plan(7, 0, 1024, 576, &[], &edges).candidates.len(),
            4
        );

        planner.reset();

        let reset = planner.plan(7, 0, 1024, 576, &[], &edges);
        assert!(reset.clear);
        assert!(reset.candidates.is_empty());
        assert!(planner
            .plan(7, 0, 1024, 576, &[], &edges)
            .candidates
            .is_empty());
        assert_eq!(
            planner.plan(7, 0, 1024, 576, &[], &edges).candidates.len(),
            4
        );
    }

    #[test]
    fn static_detail_tile_requires_every_covered_ctb_to_be_stable() {
        let mut planner = StaticTilePlanner::default();
        let mut edges = vec![0; 4 * 4];
        edges[0] = 100;
        let changing_neighbor = DamageRect {
            left: 32,
            top: 0,
            right: 64,
            bottom: 32,
        };

        for _ in 0..3 {
            let plan = planner.plan(7, 0, 128, 128, &[changing_neighbor], &edges);
            assert!(plan.candidates.is_empty());
        }
    }

    fn full_view(generation: u32, crop_revision: u32) -> ViewportGeometry {
        ViewportGeometry {
            generation,
            crop_revision,
            source: DamageRect {
                left: 0,
                top: 0,
                right: 256,
                bottom: 128,
            },
            output_width: 256,
            output_height: 128,
        }
    }

    #[test]
    fn quiescent_history_survives_crop_generation_and_reissues_mapped_tiles() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for generation in 1..=2 {
            let plan = planner.plan(256, 128, full_view(generation, 0), &[], &edges);
            assert!(plan.candidates.is_empty());
        }
        let initial = planner.plan(256, 128, full_view(3, 0), &[], &edges);
        assert_eq!(initial.candidates.len(), 2);

        let zoomed = ViewportGeometry {
            generation: 4,
            crop_revision: 1,
            source: DamageRect {
                left: 128,
                top: 0,
                right: 256,
                bottom: 128,
            },
            output_width: 256,
            output_height: 128,
        };
        let remapped = planner.plan(256, 128, zoomed, &[], &edges);
        assert!(remapped.clear);
        assert!(remapped.history_preserved);
        assert!(remapped.output_changed);
        assert_eq!(remapped.active_before_planning, 2);
        assert_eq!(remapped.changed_cells, 0);
        assert_eq!(remapped.total_cells, 32);
        assert_eq!(remapped.preserved_candidates, 1);
        assert_eq!(remapped.rejected_partial_visibility, 1);
        assert_eq!(remapped.rejected_damage, 0);
        assert_eq!(remapped.rejected_dimension, 0);
        assert_eq!(remapped.candidates.len(), 1);
        assert_eq!(
            remapped.candidates[0].output,
            DamageRect {
                left: 0,
                top: 0,
                right: 256,
                bottom: 128
            }
        );
    }

    #[test]
    fn quiescent_crop_reissues_active_tiles_without_fresh_edge_ranking() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }

        let zoomed = ViewportGeometry {
            generation: 2,
            crop_revision: 1,
            source: DamageRect {
                left: 128,
                top: 0,
                right: 256,
                bottom: 128,
            },
            output_width: 256,
            output_height: 128,
        };
        let remapped = planner.plan(256, 128, zoomed, &[], &vec![0; 8 * 4]);

        assert!(remapped.clear);
        assert!(remapped.history_preserved);
        assert_eq!(remapped.candidates.len(), 1);
        assert_eq!(remapped.candidates[0].source, zoomed.source);
        assert_eq!(
            remapped.candidates[0].output,
            DamageRect {
                left: 0,
                top: 0,
                right: 256,
                bottom: 128,
            }
        );
    }

    #[test]
    fn quiescent_motion_bypass_invalidates_without_promoting() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }
        let broad_motion = DamageRect {
            left: 0,
            top: 0,
            right: 256,
            bottom: 128,
        };
        let plan = planner.plan(256, 128, full_view(1, 0), &[broad_motion], &edges);
        assert!(plan.motion_bypass);
        assert_eq!(plan.invalidated.len(), 2);
        assert!(plan.candidates.is_empty());
    }

    #[test]
    fn quiescent_plan_reports_stability_and_edge_eligibility() {
        let mut planner = QuiescentTilePlanner::default();
        let no_edges = vec![0; 8 * 4];

        let first = planner.plan(256, 128, full_view(1, 0), &[], &no_edges);
        assert_eq!(first.edge_score_cells, 32);
        assert_eq!(first.expected_edge_score_cells, 32);
        assert_eq!(first.stability_zero_cells, 0);
        assert_eq!(first.stability_one_cells, 32);
        assert_eq!(first.stability_two_cells, 0);
        assert_eq!(first.stability_three_plus_cells, 0);
        assert_eq!(first.fully_stable_blocks, 0);
        assert_eq!(first.edge_eligible_blocks, 0);

        let second = planner.plan(256, 128, full_view(1, 0), &[], &no_edges);
        assert_eq!(second.stability_two_cells, 32);
        assert_eq!(second.fully_stable_blocks, 0);

        let third = planner.plan(256, 128, full_view(1, 0), &[], &no_edges);
        assert_eq!(third.stability_three_plus_cells, 32);
        assert_eq!(third.fully_stable_blocks, 2);
        assert_eq!(third.edge_eligible_blocks, 0);
        assert!(third.candidates.is_empty());

        let mut one_edge_tile = no_edges;
        one_edge_tile[0] = 1;
        let fourth = planner.plan(256, 128, full_view(1, 0), &[], &one_edge_tile);
        assert_eq!(fourth.fully_stable_blocks, 2);
        assert_eq!(fourth.edge_eligible_blocks, 1);
        assert_eq!(fourth.candidates.len(), 1);
    }

    #[test]
    fn quiescent_plan_reports_edge_geometry_mismatch() {
        let mut planner = QuiescentTilePlanner::default();
        let plan = planner.plan(256, 128, full_view(1, 0), &[], &vec![1; 31]);

        assert_eq!(plan.edge_score_cells, 31);
        assert_eq!(plan.expected_edge_score_cells, 32);
        assert_eq!(plan.stability_zero_cells, 32);
        assert_eq!(plan.stability_one_cells, 0);
        assert_eq!(plan.fully_stable_blocks, 0);
        assert_eq!(plan.edge_eligible_blocks, 0);
        assert!(plan.candidates.is_empty());
    }

    #[test]
    fn quiescent_immediate_damage_invalidates_and_regenerates_without_resetting_history() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }
        let changed_active_tile = DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 128,
        };

        let plan = planner.plan_with_immediate_damage(
            256,
            128,
            full_view(1, 0),
            &[],
            &[changed_active_tile],
            &edges,
        );

        assert_eq!(plan.invalidated, vec![1]);
        assert_eq!(plan.changed_cells, 0);
        assert!(!plan.motion_bypass);
        assert_eq!(plan.stability_three_plus_cells, 32);
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].tile_id, 1);
    }

    #[test]
    fn quiescent_cache_replaces_a_committed_low_value_tile_when_better_detail_stabilizes() {
        let mut planner = QuiescentTilePlanner::default();
        let viewport = ViewportGeometry {
            generation: 1,
            crop_revision: 0,
            source: DamageRect {
                left: 0,
                top: 0,
                right: 640,
                bottom: 128,
            },
            output_width: 640,
            output_height: 128,
        };
        let mut edges = vec![0; 20 * 4];
        for row in 0..4 {
            for tile in 0..5 {
                for column in 0..4 {
                    edges[row * 20 + tile * 4 + column] = 100 + tile as u32 * 10;
                }
            }
        }
        let delayed_high_value_tile = DamageRect {
            left: 512,
            top: 0,
            right: 640,
            bottom: 128,
        };
        for _ in 0..3 {
            let plan = planner.plan(640, 128, viewport, &[delayed_high_value_tile], &edges);
            if !plan.candidates.is_empty() {
                assert_eq!(plan.candidates.len(), 4);
                for candidate in plan.candidates {
                    assert!(planner.commit_candidate(candidate.tile_id, 1, 1_000));
                }
            }
        }
        for row in 0..4 {
            for column in 16..20 {
                edges[row * 20 + column] = 1_000;
            }
        }

        assert!(planner
            .plan(640, 128, viewport, &[], &edges)
            .candidates
            .is_empty());
        assert!(planner
            .plan(640, 128, viewport, &[], &edges)
            .candidates
            .is_empty());
        let rebalanced = planner.plan(640, 128, viewport, &[], &edges);

        assert_eq!(rebalanced.invalidated, vec![1]);
        assert_eq!(rebalanced.candidates.len(), 1);
        assert_eq!(rebalanced.candidates[0].tile_id, 5);
        assert!(planner.commit_candidate(5, 2, 2_000));
        let stable_top_set = planner.plan(640, 128, viewport, &[], &edges);
        assert!(stable_top_set.invalidated.is_empty());
        assert!(stable_top_set.candidates.is_empty());
    }

    #[test]
    fn full_frame_immediate_damage_does_not_prevent_quiescence_promotion() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        let full_frame_dda = [DamageRect {
            left: 0,
            top: 0,
            right: 256,
            bottom: 128,
        }];

        for expected_age in 1..=3 {
            let plan = planner.plan_with_immediate_damage(
                256,
                128,
                full_view(1, 0),
                &[],
                &full_frame_dda,
                &edges,
            );
            assert_eq!(plan.changed_cells, 0);
            assert!(!plan.motion_bypass);
            if expected_age < 3 {
                assert!(plan.candidates.is_empty());
            } else {
                assert_eq!(plan.stability_three_plus_cells, 32);
                assert_eq!(plan.candidates.len(), 2);
            }
        }
    }

    #[test]
    fn quiescent_false_positive_damage_suppresses_without_deleting_then_reactivates_without_png() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }
        assert!(planner.commit_candidate(1, 10, 12_345));
        assert!(planner.commit_candidate(2, 10, 23_456));

        let dda_damage = [DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 128,
        }];
        let requests = planner.validation_requests(full_view(1, 0), 11, Some(&dda_damage), false);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].tile_id, 1);
        assert_eq!(
            planner.suppress_requests(&requests),
            vec![HybridTileOperation::Suppress {
                tile_id: 1,
                content_revision: 10,
                validation_sequence: 11,
            },]
        );
        assert_eq!(planner.suppressed_tile_count(), 1);
        assert_eq!(planner.retained_png_bytes(), 12_345 + 23_456);

        let resolution = planner.resolve_validation(
            full_view(1, 0),
            &QuiescentValidationVerdict {
                request: requests[0].clone(),
                changed: false,
                age_frames: 1,
            },
        );
        assert_eq!(
            resolution,
            Some(HybridTileOperation::Reactivate {
                tile_id: 1,
                content_revision: 10,
                validation_sequence: 11,
            })
        );
        assert_eq!(planner.suppressed_tile_count(), 0);
        assert!(planner
            .plan(256, 128, full_view(1, 0), &[], &edges)
            .candidates
            .is_empty());
    }

    #[test]
    fn identical_false_positive_dda_damage_repngs_control_but_reactivates_candidate() {
        let mut edges = vec![0; 8 * 4];
        for row in 0..4 {
            edges[row * 8..row * 8 + 4].fill(100);
        }
        let viewport = full_view(1, 0);
        let dda_damage = [DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 128,
        }];
        let mut control = QuiescentTilePlanner::default();
        let mut candidate = QuiescentTilePlanner::default();
        for _ in 0..3 {
            control.plan(256, 128, viewport, &[], &edges);
            candidate.plan(256, 128, viewport, &[], &edges);
        }
        assert!(control.commit_candidate(1, 10, 12_345));
        assert!(candidate.commit_candidate(1, 10, 12_345));

        let control_damage =
            control.plan_with_immediate_damage(256, 128, viewport, &[], &dda_damage, &edges);
        assert_eq!(control_damage.invalidated, vec![1]);
        assert_ne!(control.cached_png_bytes(1), Some(12_345));
        assert!(control_damage
            .candidates
            .iter()
            .any(|candidate| candidate.tile_id == 1));

        let requests = candidate.validation_requests(viewport, 11, Some(&dda_damage), false);
        assert_eq!(requests.len(), 1);
        assert_eq!(candidate.suppress_requests(&requests).len(), 1);
        let candidate_plan =
            candidate.plan_with_immediate_damage(256, 128, viewport, &[], &[], &edges);
        assert!(candidate_plan.invalidated.is_empty());
        assert!(candidate_plan
            .candidates
            .iter()
            .all(|candidate| candidate.tile_id != 1));
        assert_eq!(
            candidate.resolve_validation(
                viewport,
                &QuiescentValidationVerdict {
                    request: requests[0].clone(),
                    changed: false,
                    age_frames: 1,
                },
            ),
            Some(HybridTileOperation::Reactivate {
                tile_id: 1,
                content_revision: 10,
                validation_sequence: 11,
            })
        );
        assert_eq!(candidate.cached_png_bytes(1), Some(12_345));
    }

    #[test]
    fn quiescent_suppressed_tile_waits_for_its_outstanding_gpu_verdict() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }
        assert!(planner.commit_candidate(1, 10, 12_345));
        let dda_damage = [DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 128,
        }];
        let first = planner.validation_requests(full_view(1, 0), 11, Some(&dda_damage), false);
        assert_eq!(planner.suppress_requests(&first).len(), 1);

        assert!(planner
            .validation_requests(full_view(1, 0), 12, Some(&[]), false)
            .is_empty());
        assert_eq!(
            planner.resolve_validation(
                full_view(1, 0),
                &QuiescentValidationVerdict {
                    request: first[0].clone(),
                    changed: false,
                    age_frames: 2,
                },
            ),
            Some(HybridTileOperation::Reactivate {
                tile_id: 1,
                content_revision: 10,
                validation_sequence: 11,
            })
        );
        assert_eq!(
            planner
                .validation_requests(full_view(1, 0), 13, Some(&dda_damage), false)
                .len(),
            1
        );
    }

    #[test]
    fn quiescent_new_damage_supersedes_pending_validation_before_old_verdict() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }
        assert!(planner.commit_candidate(1, 10, 12_345));
        let dda_damage = [DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 128,
        }];

        let first = planner.validation_requests(full_view(1, 0), 11, Some(&dda_damage), false);
        assert_eq!(planner.suppress_requests(&first).len(), 1);

        let current = planner.validation_requests(full_view(1, 0), 12, Some(&dda_damage), false);
        assert_eq!(current.len(), 1);
        assert!(planner.suppress_requests(&current).is_empty());
        assert!(planner.validation_verdict_superseded(
            full_view(1, 0),
            &QuiescentValidationVerdict {
                request: first[0].clone(),
                changed: false,
                age_frames: 1,
            },
        ));
        assert_eq!(
            planner.resolve_validation(
                full_view(1, 0),
                &QuiescentValidationVerdict {
                    request: first[0].clone(),
                    changed: false,
                    age_frames: 1,
                },
            ),
            None
        );
        assert_eq!(planner.suppressed_tile_count(), 1);
        assert_eq!(
            planner.resolve_validation(
                full_view(1, 0),
                &QuiescentValidationVerdict {
                    request: current[0].clone(),
                    changed: false,
                    age_frames: 1,
                },
            ),
            Some(HybridTileOperation::Reactivate {
                tile_id: 1,
                content_revision: 10,
                validation_sequence: 11,
            })
        );
    }

    #[test]
    fn repeated_damage_reactivation_targets_the_announced_browser_suppression_token() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }
        assert!(planner.commit_candidate(1, 10, 12_345));
        let dda_damage = [DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 128,
        }];

        let announced = planner.validation_requests(full_view(1, 0), 11, Some(&dda_damage), false);
        assert_eq!(
            planner.suppress_requests(&announced),
            vec![HybridTileOperation::Suppress {
                tile_id: 1,
                content_revision: 10,
                validation_sequence: 11,
            }]
        );

        let latest = planner.validation_requests(full_view(1, 0), 12, Some(&dda_damage), false);
        assert!(planner.suppress_requests(&latest).is_empty());
        assert_eq!(
            planner.resolve_validation(
                full_view(1, 0),
                &QuiescentValidationVerdict {
                    request: latest[0].clone(),
                    changed: false,
                    age_frames: 2,
                },
            ),
            Some(HybridTileOperation::Reactivate {
                tile_id: 1,
                content_revision: 10,
                validation_sequence: 11,
            })
        );
    }

    #[test]
    fn quiescent_superseded_changed_verdict_invalidates_announced_suppression_token() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }
        assert!(planner.commit_candidate(1, 10, 12_345));
        let dda_damage = [DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 128,
        }];

        let first = planner.validation_requests(full_view(1, 0), 11, Some(&dda_damage), false);
        assert_eq!(planner.suppress_requests(&first).len(), 1);
        let current = planner.validation_requests(full_view(1, 0), 12, Some(&dda_damage), false);
        assert!(planner.suppress_requests(&current).is_empty());

        assert_eq!(
            planner.resolve_validation(
                full_view(1, 0),
                &QuiescentValidationVerdict {
                    request: first[0].clone(),
                    changed: true,
                    age_frames: 1,
                },
            ),
            Some(HybridTileOperation::InvalidateValidated {
                tile_id: 1,
                content_revision: 10,
                validation_sequence: 11,
            })
        );
        assert_eq!(planner.suppressed_tile_count(), 0);
        assert_eq!(planner.cached_png_bytes(1), None);
    }

    #[test]
    fn quiescent_changed_verdict_permanently_invalidates_the_exact_cached_revision() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }
        assert!(planner.commit_candidate(1, 10, 12_345));
        let requests = planner.validation_requests(full_view(1, 0), 11, None, false);
        planner.suppress_requests(&requests);

        assert_eq!(
            planner.resolve_validation(
                full_view(1, 0),
                &QuiescentValidationVerdict {
                    request: requests[0].clone(),
                    changed: true,
                    age_frames: 1,
                },
            ),
            Some(HybridTileOperation::InvalidateValidated {
                tile_id: 1,
                content_revision: 10,
                validation_sequence: 11,
            })
        );
        assert_eq!(planner.retained_png_bytes(), 0);
    }

    #[test]
    fn quiescent_exact_validation_without_a_damage_prefilter_covers_every_active_tile() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }
        assert!(planner.commit_candidate(1, 10, 12_345));
        assert!(planner.commit_candidate(2, 10, 23_456));

        let requests = planner.validation_requests(full_view(1, 0), 11, None, false);

        assert_eq!(
            requests
                .iter()
                .map(|request| request.tile_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(planner.suppress_requests(&requests).len(), 2);
    }

    #[test]
    fn quiescent_stale_or_missing_verdict_never_reactivates_a_suppressed_tile() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }
        assert!(planner.commit_candidate(1, 10, 12_345));
        let requests = planner.validation_requests(full_view(1, 0), 11, None, false);
        planner.suppress_requests(&requests);

        let mut stale_request = requests[0].clone();
        stale_request.validation_sequence = 9;
        assert_eq!(
            planner.resolve_validation(
                full_view(1, 0),
                &QuiescentValidationVerdict {
                    request: stale_request,
                    changed: false,
                    age_frames: 2,
                },
            ),
            None
        );
        assert_eq!(
            planner.resolve_validation(
                full_view(2, 1),
                &QuiescentValidationVerdict {
                    request: QuiescentValidationRequest {
                        generation: 2,
                        crop_revision: 1,
                        ..requests[0].clone()
                    },
                    changed: false,
                    age_frames: 1,
                },
            ),
            None
        );
        assert_eq!(planner.suppressed_tile_count(), 1);
        assert_eq!(planner.retained_png_bytes(), 12_345);
    }

    #[test]
    fn quiescent_cursor_neutral_empty_damage_emits_no_tile_work() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }
        let cursor_only = planner.plan(256, 128, full_view(1, 0), &[], &edges);
        assert!(!cursor_only.clear);
        assert!(cursor_only.invalidated.is_empty());
        assert!(cursor_only.candidates.is_empty());
    }

    #[test]
    fn quiescent_recovery_reissues_without_losing_stability_history() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }
        planner.mark_output_stale();
        let recovery = planner.plan(256, 128, full_view(1, 0), &[], &vec![0; 8 * 4]);
        assert!(recovery.clear);
        assert!(recovery.history_preserved);
        assert_eq!(recovery.candidates.len(), 2);
    }

    #[test]
    fn quiescent_source_geometry_change_resets_history() {
        let mut planner = QuiescentTilePlanner::default();
        let edges = vec![100; 8 * 4];
        for _ in 0..3 {
            planner.plan(256, 128, full_view(1, 0), &[], &edges);
        }
        let resized_edges = vec![100; 10 * 4];
        let resized = ViewportGeometry {
            generation: 2,
            crop_revision: 0,
            source: DamageRect {
                left: 0,
                top: 0,
                right: 320,
                bottom: 128,
            },
            output_width: 320,
            output_height: 128,
        };
        let reset = planner.plan(320, 128, resized, &[], &resized_edges);
        assert!(reset.clear);
        assert!(!reset.history_preserved);
        assert!(reset.candidates.is_empty());
    }
}
