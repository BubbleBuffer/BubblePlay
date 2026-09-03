use std::collections::VecDeque;
use std::ffi::c_void;
use std::slice;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, ID3DInclude};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11UnorderedAccessView, D3D11_BIND_CONSTANT_BUFFER,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS, D3D11_BUFFER_DESC, D3D11_BUFFER_UAV,
    D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAP_WRITE_DISCARD,
    D3D11_RESOURCE_MISC_BUFFER_STRUCTURED, D3D11_TEXTURE2D_DESC, D3D11_UAV_DIMENSION_BUFFER,
    D3D11_UNORDERED_ACCESS_VIEW_DESC, D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_DYNAMIC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;

use crate::frame_damage::DamageRect;
use crate::hybrid::{QuiescentValidationRequest, QuiescentValidationVerdict};

pub const CTB_SIZE: u32 = 32;
pub const DEFAULT_CHANGE_SCORE_THRESHOLD: u32 = 2_048;
const MAX_ACTIVE_TILES: usize = 4;
const ACTIVE_CTBS_PER_TILE: usize = 16;
const MAX_ACTIVE_CTBS: u32 = (MAX_ACTIVE_TILES * ACTIVE_CTBS_PER_TILE) as u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalMaskRejectReason {
    InvalidGeometry,
    EmptyDamage,
    CoverageAtLeastHalf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemporalMaskDecision {
    Applied {
        rects: Vec<DamageRect>,
        changed_ctbs: u32,
        total_ctbs: u32,
    },
    Rejected {
        reason: TemporalMaskRejectReason,
        changed_ctbs: u32,
        total_ctbs: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalDiffResult {
    pub decision: TemporalMaskDecision,
    pub damage_rects: Vec<DamageRect>,
    pub immediate_damage_rects: Vec<DamageRect>,
    pub edge_scores: Vec<u32>,
    pub analysis_us: u32,
    pub priming: bool,
    pub damage_audit: Option<DamageCoverageAudit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuiescentValidationStatus {
    NotRequested,
    Pending,
    Ready,
    Unavailable,
    Failed,
}

pub struct QuiescentDiffSubmission {
    result: TemporalDiffResult,
    active: ActiveDamageSubmission,
    submit_us: u32,
    deferred_readback: DeferredReadbackActivity,
    pub accepted_requests: Vec<QuiescentValidationRequest>,
}

pub struct QuiescentDiffCompletion {
    pub result: TemporalDiffResult,
    pub submit_us: u32,
    pub collect_us: u32,
    pub validation_status: QuiescentValidationStatus,
    pub validation_error: Option<String>,
    pub deferred_readback: DeferredReadbackActivity,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeferredReadbackActivity {
    pub ready_ages_frames: Vec<u32>,
    pub pending_frames: u32,
    pub max_pending_age_frames: Option<u32>,
    pub verdicts: Vec<QuiescentValidationVerdict>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveDamageSubmission {
    NotRequested,
    Pending { readback_slot: usize },
    Unavailable,
}

enum ActiveDamageCollection<'a> {
    Ready(&'a [u32]),
    Unavailable,
    Failed,
}

#[derive(Debug, Clone)]
struct TemporalFeatures {
    scores: Vec<u32>,
    edge_scores: Vec<u32>,
    frame_index: u64,
}

#[derive(Debug, Clone)]
struct DamageHistoryEntry {
    frame_index: u64,
    damage: Option<Vec<DamageRect>>,
}

#[derive(Debug, Clone)]
struct ActiveValidationBatch {
    requests: Vec<QuiescentValidationRequest>,
}

const TEMPORAL_DIFF_SHADER: &str = r#"
Texture2D<float4> CurrentFrame : register(t0);
Texture2D<float4> PreviousFrame : register(t1);
RWStructuredBuffer<uint> CtbScores : register(u0);
RWStructuredBuffer<uint> EdgeScores : register(u1);

cbuffer ActiveTileBounds : register(b0) {
    uint4 ActiveBounds[4];
    uint ActiveTileCount;
    uint3 ActivePadding;
};

groupshared uint PartialChangeScores[64];
groupshared uint PartialEdgeScores[64];

[numthreads(8, 8, 1)]
void main(
    uint3 group_id : SV_GroupID,
    uint3 group_thread_id : SV_GroupThreadID,
    uint group_index : SV_GroupIndex
) {
    uint width;
    uint height;
    CurrentFrame.GetDimensions(width, height);
    uint2 origin = group_id.xy * 32 + group_thread_id.xy * 4;
    uint change_score = 0;
    uint edge_score = 0;
    [unroll]
    for (uint y = 0; y < 4; ++y) {
        [unroll]
        for (uint x = 0; x < 4; ++x) {
            uint2 position = origin + uint2(x, y);
            if (position.x >= width || position.y >= height) {
                continue;
            }
            uint3 current = (uint3)round(saturate(CurrentFrame.Load(int3(position, 0)).rgb) * 255.0);
            uint3 previous = (uint3)round(saturate(PreviousFrame.Load(int3(position, 0)).rgb) * 255.0);
            change_score += (uint)abs((int)current.x - (int)previous.x);
            change_score += (uint)abs((int)current.y - (int)previous.y);
            change_score += (uint)abs((int)current.z - (int)previous.z);
            uint luma = (current.x * 19 + current.y * 183 + current.z * 54) >> 8;
            if (position.x + 1 < width) {
                uint3 right = (uint3)round(saturate(CurrentFrame.Load(int3(position + uint2(1, 0), 0)).rgb) * 255.0);
                uint right_luma = (right.x * 19 + right.y * 183 + right.z * 54) >> 8;
                edge_score += (uint)abs((int)luma - (int)right_luma);
            }
            if (position.y + 1 < height) {
                uint3 down = (uint3)round(saturate(CurrentFrame.Load(int3(position + uint2(0, 1), 0)).rgb) * 255.0);
                uint down_luma = (down.x * 19 + down.y * 183 + down.z * 54) >> 8;
                edge_score += (uint)abs((int)luma - (int)down_luma);
            }
        }
    }
    PartialChangeScores[group_index] = change_score;
    PartialEdgeScores[group_index] = edge_score;
    GroupMemoryBarrierWithGroupSync();
    [unroll]
    for (uint stride = 32; stride > 0; stride >>= 1) {
        if (group_index < stride) {
            PartialChangeScores[group_index] += PartialChangeScores[group_index + stride];
            PartialEdgeScores[group_index] += PartialEdgeScores[group_index + stride];
        }
        GroupMemoryBarrierWithGroupSync();
    }
    if (group_index == 0) {
        uint columns = (width + 31) / 32;
        uint index = group_id.y * columns + group_id.x;
        CtbScores[index] = PartialChangeScores[0];
        EdgeScores[index] = PartialEdgeScores[0];
    }
}

[numthreads(8, 8, 1)]
void active_main(
    uint3 group_id : SV_GroupID,
    uint3 group_thread_id : SV_GroupThreadID,
    uint group_index : SV_GroupIndex
) {
    uint tile_index = group_id.x / 16;
    uint local_index = group_id.x % 16;
    if (tile_index >= ActiveTileCount) {
        return;
    }
    uint4 bounds = ActiveBounds[tile_index];
    uint tile_columns = bounds.z - bounds.x;
    uint tile_rows = bounds.w - bounds.y;
    if (tile_columns == 0 || tile_rows == 0 || local_index >= tile_columns * tile_rows) {
        if (group_index == 0) {
            CtbScores[group_id.x] = 0;
        }
        return;
    }
    uint ctb_column = bounds.x + local_index % tile_columns;
    uint ctb_row = bounds.y + local_index / tile_columns;
    uint width;
    uint height;
    CurrentFrame.GetDimensions(width, height);
    uint2 origin = uint2(ctb_column, ctb_row) * 32 + group_thread_id.xy * 4;
    uint change_score = 0;
    [unroll]
    for (uint y = 0; y < 4; ++y) {
        [unroll]
        for (uint x = 0; x < 4; ++x) {
            uint2 position = origin + uint2(x, y);
            if (position.x >= width || position.y >= height) {
                continue;
            }
            float3 current = CurrentFrame.Load(int3(position, 0)).rgb;
            float3 previous = PreviousFrame.Load(int3(position, 0)).rgb;
            change_score |= any(current != previous);
        }
    }
    PartialChangeScores[group_index] = change_score;
    GroupMemoryBarrierWithGroupSync();
    [unroll]
    for (uint stride = 32; stride > 0; stride >>= 1) {
        if (group_index < stride) {
            PartialChangeScores[group_index] |= PartialChangeScores[group_index + stride];
        }
        GroupMemoryBarrierWithGroupSync();
    }
    if (group_index == 0) {
        CtbScores[group_id.x] = PartialChangeScores[0];
    }
}
"#;

pub struct TemporalDiffAnalyzer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    shader: ID3D11ComputeShader,
    active_shader: ID3D11ComputeShader,
    textures: [ID3D11Texture2D; 2],
    texture_views: [ID3D11ShaderResourceView; 2],
    score_buffer: ID3D11Buffer,
    score_view: ID3D11UnorderedAccessView,
    edge_score_buffer: ID3D11Buffer,
    edge_score_view: ID3D11UnorderedAccessView,
    readback_buffers: [ID3D11Buffer; 2],
    edge_readback_buffers: [ID3D11Buffer; 2],
    active_bounds_buffer: ID3D11Buffer,
    active_score_buffer: ID3D11Buffer,
    active_score_view: ID3D11UnorderedAccessView,
    active_readback_buffers: [ID3D11Buffer; 2],
    active_readback_ring: ReadbackRing,
    active_validation_batches: [Option<ActiveValidationBatch>; 2],
    readback_ring: ReadbackRing,
    width: u32,
    height: u32,
    columns: u32,
    rows: u32,
    frame_index: u64,
    next_texture: usize,
    primed: bool,
    last_features: Option<TemporalFeatures>,
    damage_history: VecDeque<DamageHistoryEntry>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ReadbackRing {
    next_write: usize,
    next_sequence: u64,
    pending: [Option<ReadbackTicket>; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadbackTicket {
    sequence: u64,
    frame_index: u64,
}

impl ReadbackRing {
    fn oldest_pending(&self) -> Option<usize> {
        self.pending
            .iter()
            .enumerate()
            .filter_map(|(slot, ticket)| ticket.map(|ticket| (slot, ticket.sequence)))
            .min_by_key(|(_, sequence)| *sequence)
            .map(|(slot, _)| slot)
    }

    fn free_slot(&self) -> Option<usize> {
        (0..self.pending.len())
            .map(|offset| (self.next_write + offset) % self.pending.len())
            .find(|slot| self.pending[*slot].is_none())
    }

    fn submit(&mut self, slot: usize, frame_index: u64) {
        debug_assert!(self.pending[slot].is_none());
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);
        self.pending[slot] = Some(ReadbackTicket {
            sequence: self.next_sequence,
            frame_index,
        });
        self.next_write = (slot + 1) % self.pending.len();
    }

    fn pending_age(&self, slot: usize, current_frame: u64) -> Option<u32> {
        self.pending[slot].map(|ticket| {
            current_frame
                .saturating_sub(ticket.frame_index)
                .min(u64::from(u32::MAX)) as u32
        })
    }

    fn complete(&mut self, slot: usize) {
        self.pending[slot] = None;
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

impl TemporalDiffAnalyzer {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        ensure!(
            width > 0 && height > 0,
            "temporal-diff dimensions must be nonzero"
        );
        let columns = width.div_ceil(CTB_SIZE);
        let rows = height.div_ceil(CTB_SIZE);
        let total_ctbs = columns
            .checked_mul(rows)
            .context("temporal-diff CTB count overflow")?;
        ensure!(
            total_ctbs <= 8_160,
            "temporal-diff CTB grid exceeds the lab bound"
        );
        let context =
            unsafe { device.GetImmediateContext() }.context("temporal-diff GetImmediateContext")?;
        let shader = compile_shader(device, PCSTR(b"main\0".as_ptr()), "temporal-diff")?;
        let active_shader =
            compile_shader(device, PCSTR(b"active_main\0".as_ptr()), "active-tile-diff")?;
        let first = create_history_texture(device, width, height)?;
        let second = create_history_texture(device, width, height)?;
        let first_view = create_texture_view(device, &first)?;
        let second_view = create_texture_view(device, &second)?;
        let score_buffer = create_score_buffer(device, total_ctbs)?;
        let score_view = create_score_view(device, &score_buffer, total_ctbs)?;
        let edge_score_buffer = create_score_buffer(device, total_ctbs)?;
        let edge_score_view = create_score_view(device, &edge_score_buffer, total_ctbs)?;
        let first_readback = create_readback_buffer(device, total_ctbs)?;
        let second_readback = create_readback_buffer(device, total_ctbs)?;
        let first_edge_readback = create_readback_buffer(device, total_ctbs)?;
        let second_edge_readback = create_readback_buffer(device, total_ctbs)?;
        let active_bounds_buffer = create_active_bounds_buffer(device)?;
        let active_score_buffer = create_score_buffer(device, MAX_ACTIVE_CTBS)?;
        let active_score_view = create_score_view(device, &active_score_buffer, MAX_ACTIVE_CTBS)?;
        let first_active_readback = create_readback_buffer(device, MAX_ACTIVE_CTBS)?;
        let second_active_readback = create_readback_buffer(device, MAX_ACTIVE_CTBS)?;
        Ok(Self {
            device: device.clone(),
            context,
            shader,
            active_shader,
            textures: [first, second],
            texture_views: [first_view, second_view],
            score_buffer,
            score_view,
            edge_score_buffer,
            edge_score_view,
            readback_buffers: [first_readback, second_readback],
            edge_readback_buffers: [first_edge_readback, second_edge_readback],
            active_bounds_buffer,
            active_score_buffer,
            active_score_view,
            active_readback_buffers: [first_active_readback, second_active_readback],
            active_readback_ring: ReadbackRing::default(),
            active_validation_batches: [None, None],
            readback_ring: ReadbackRing::default(),
            width,
            height,
            columns,
            rows,
            frame_index: 0,
            next_texture: 0,
            primed: false,
            last_features: None,
            damage_history: VecDeque::new(),
        })
    }

    pub fn is_for(&self, device: &ID3D11Device, width: u32, height: u32) -> bool {
        self.device.as_raw() == device.as_raw() && self.width == width && self.height == height
    }

    pub fn reset(&mut self) {
        self.frame_index = 0;
        self.next_texture = 0;
        self.primed = false;
        self.active_readback_ring.reset();
        self.active_validation_batches = [None, None];
        self.readback_ring.reset();
        self.last_features = None;
        self.damage_history.clear();
    }

    pub fn analyze(&mut self, source: &ID3D11Texture2D) -> Result<TemporalDiffResult> {
        self.analyze_with_readback(source, false, None, None)
            .map(|(result, _, _)| result)
    }

    pub fn analyze_aligned(&mut self, source: &ID3D11Texture2D) -> Result<TemporalDiffResult> {
        self.analyze_with_readback(source, true, None, None)
            .map(|(result, _, _)| result)
    }

    pub fn submit_quiescent(
        &mut self,
        source: &ID3D11Texture2D,
        validation_requests: &[QuiescentValidationRequest],
        capture_damage: Option<&[DamageRect]>,
    ) -> Result<QuiescentDiffSubmission> {
        let (result, active, deferred_readback) = self.analyze_with_readback(
            source,
            false,
            Some(validation_requests),
            Some(capture_damage),
        )?;
        let submit_us = result.analysis_us;
        let accepted_requests = if matches!(active, ActiveDamageSubmission::Pending { .. }) {
            validation_requests.to_vec()
        } else {
            Vec::new()
        };
        Ok(QuiescentDiffSubmission {
            result,
            active,
            submit_us,
            deferred_readback,
            accepted_requests,
        })
    }

    pub fn collect_quiescent(
        &mut self,
        submission: QuiescentDiffSubmission,
    ) -> QuiescentDiffCompletion {
        let QuiescentDiffSubmission {
            result,
            active,
            submit_us,
            mut deferred_readback,
            accepted_requests: _,
        } = submission;
        let started = Instant::now();
        let validation_error = None;
        let validation_status = match active {
            ActiveDamageSubmission::NotRequested if deferred_readback.verdicts.is_empty() => {
                QuiescentValidationStatus::NotRequested
            }
            ActiveDamageSubmission::Unavailable if deferred_readback.verdicts.is_empty() => {
                QuiescentValidationStatus::Unavailable
            }
            ActiveDamageSubmission::Pending { .. } => {
                deferred_readback.pending_frames =
                    deferred_readback.pending_frames.saturating_add(1);
                deferred_readback.max_pending_age_frames = Some(
                    deferred_readback
                        .max_pending_age_frames
                        .map_or(0, |current| current.max(0)),
                );
                if deferred_readback.verdicts.is_empty() {
                    QuiescentValidationStatus::Pending
                } else {
                    QuiescentValidationStatus::Ready
                }
            }
            ActiveDamageSubmission::NotRequested | ActiveDamageSubmission::Unavailable => {
                QuiescentValidationStatus::Ready
            }
        };
        let immediate_damage_rects = deferred_readback
            .verdicts
            .iter()
            .filter(|verdict| verdict.changed)
            .map(|verdict| verdict.request.source)
            .collect();
        let collect_us = elapsed_us(started);
        let mut result = result;
        result.immediate_damage_rects = immediate_damage_rects;
        result.analysis_us = submit_us.saturating_add(collect_us);
        QuiescentDiffCompletion {
            result,
            submit_us,
            collect_us,
            validation_status,
            validation_error,
            deferred_readback,
        }
    }

    fn analyze_with_readback(
        &mut self,
        source: &ID3D11Texture2D,
        aligned: bool,
        active_tiles: Option<&[QuiescentValidationRequest]>,
        capture_damage: Option<Option<&[DamageRect]>>,
    ) -> Result<(
        TemporalDiffResult,
        ActiveDamageSubmission,
        DeferredReadbackActivity,
    )> {
        let started = Instant::now();
        self.validate_source(source)?;
        self.frame_index = self.frame_index.wrapping_add(1).max(1);
        let frame_index = self.frame_index;
        if let Some(capture_damage) = capture_damage {
            self.damage_history.push_back(DamageHistoryEntry {
                frame_index,
                damage: capture_damage.map(<[DamageRect]>::to_vec),
            });
            while self.damage_history.len() > 16 {
                self.damage_history.pop_front();
            }
        }
        let current = self.next_texture;
        if !self.primed {
            unsafe {
                self.context.CopyResource(&self.textures[current], source);
            }
            self.primed = true;
            self.next_texture = 1 - current;
            return Ok((
                TemporalDiffResult {
                    decision: TemporalMaskDecision::Rejected {
                        reason: TemporalMaskRejectReason::EmptyDamage,
                        changed_ctbs: 0,
                        total_ctbs: self.columns * self.rows,
                    },
                    damage_rects: Vec::new(),
                    immediate_damage_rects: Vec::new(),
                    edge_scores: Vec::new(),
                    analysis_us: elapsed_us(started),
                    priming: true,
                    damage_audit: None,
                },
                ActiveDamageSubmission::NotRequested,
                DeferredReadbackActivity::default(),
            ));
        }

        let previous = 1 - current;
        let mut fresh_features = None;
        if let Some(readback) = self.readback_ring.oldest_pending() {
            let ticket = self.readback_ring.pending[readback]
                .context("temporal readback slot lost its frame ticket")?;
            let features = self.read_features(readback, !aligned)?;
            if let Some((scores, edge_scores)) = features {
                self.readback_ring.complete(readback);
                fresh_features = Some(TemporalFeatures {
                    scores,
                    edge_scores,
                    frame_index: ticket.frame_index,
                });
            }
        }
        if aligned && self.readback_ring.free_slot().is_none() {
            let readback = self
                .readback_ring
                .oldest_pending()
                .context("aligned temporal readback has no pending slot to drain")?;
            let ticket = self.readback_ring.pending[readback]
                .context("aligned temporal readback slot lost its frame ticket")?;
            let (scores, edge_scores) = self
                .read_features(readback, false)?
                .context("blocking temporal readback returned no features")?;
            self.readback_ring.complete(readback);
            fresh_features = Some(TemporalFeatures {
                scores,
                edge_scores,
                frame_index: ticket.frame_index,
            });
        }
        let write_readback = self.readback_ring.free_slot();
        unsafe {
            self.context.CopyResource(&self.textures[current], source);
        }
        let current_view = self.texture_views[current].clone();
        let (active_submission, deferred_readback) = match active_tiles {
            Some(validation_requests) => self.submit_active_damage(
                &current_view,
                previous,
                validation_requests,
                frame_index,
            )?,
            None => (
                ActiveDamageSubmission::NotRequested,
                DeferredReadbackActivity::default(),
            ),
        };
        if let Some(write_readback) = write_readback {
            let shader_resources = [
                Some(current_view.clone()),
                Some(self.texture_views[previous].clone()),
            ];
            let unordered_views = [
                Some(self.score_view.clone()),
                Some(self.edge_score_view.clone()),
            ];
            unsafe {
                self.context.CSSetShader(&self.shader, None);
                self.context
                    .CSSetShaderResources(0, Some(&shader_resources));
                self.context.CSSetUnorderedAccessViews(
                    0,
                    unordered_views.len() as u32,
                    Some(unordered_views.as_ptr()),
                    None,
                );
                self.context.Dispatch(self.columns, self.rows, 1);
                self.context.CSSetShaderResources(0, Some(&[None, None]));
                self.context.CSSetUnorderedAccessViews(
                    0,
                    2,
                    Some(
                        [
                            None::<ID3D11UnorderedAccessView>,
                            None::<ID3D11UnorderedAccessView>,
                        ]
                        .as_ptr(),
                    ),
                    None,
                );
                self.context.CSSetShader(None::<&ID3D11ComputeShader>, None);
                self.context
                    .CopyResource(&self.readback_buffers[write_readback], &self.score_buffer);
                self.context.CopyResource(
                    &self.edge_readback_buffers[write_readback],
                    &self.edge_score_buffer,
                );
            }
            self.readback_ring.submit(write_readback, frame_index);
            if aligned {
                let (scores, edge_scores) = self
                    .read_features(write_readback, false)?
                    .context("aligned current temporal readback returned no features")?;
                fresh_features = Some(TemporalFeatures {
                    scores,
                    edge_scores,
                    frame_index,
                });
                self.readback_ring.complete(write_readback);
            }
        }
        self.next_texture = previous;
        let fresh_frame_index = fresh_features.as_ref().map(|features| features.frame_index);
        if let Some(features) = fresh_features.as_ref() {
            self.last_features = Some(features.clone());
        }
        let features = fresh_features.or_else(|| self.last_features.clone());
        let Some(TemporalFeatures {
            scores,
            edge_scores,
            ..
        }) = features
        else {
            return Ok((
                TemporalDiffResult {
                    decision: TemporalMaskDecision::Rejected {
                        reason: TemporalMaskRejectReason::EmptyDamage,
                        changed_ctbs: 0,
                        total_ctbs: self.columns * self.rows,
                    },
                    damage_rects: Vec::new(),
                    immediate_damage_rects: Vec::new(),
                    edge_scores: Vec::new(),
                    analysis_us: elapsed_us(started),
                    priming: true,
                    damage_audit: None,
                },
                active_submission,
                deferred_readback,
            ));
        };
        let decision = classify_temporal_ctb_scores(
            self.width,
            self.height,
            &scores,
            DEFAULT_CHANGE_SCORE_THRESHOLD,
        );
        let damage_rects = collect_temporal_damage_rects(
            self.width,
            self.height,
            &scores,
            DEFAULT_CHANGE_SCORE_THRESHOLD,
        )
        .context("temporal-diff score geometry changed after validation")?;
        let damage_audit = fresh_frame_index.and_then(|source_frame| {
            self.damage_history
                .iter()
                .find(|sample| sample.frame_index == source_frame)
                .map(|sample| {
                    audit_damage_coverage(
                        self.width,
                        self.height,
                        sample.damage.as_deref(),
                        &damage_rects,
                    )
                })
        });
        Ok((
            TemporalDiffResult {
                decision,
                damage_rects,
                immediate_damage_rects: Vec::new(),
                edge_scores,
                analysis_us: elapsed_us(started),
                priming: false,
                damage_audit,
            },
            active_submission,
            deferred_readback,
        ))
    }

    fn submit_active_damage(
        &mut self,
        current_view: &ID3D11ShaderResourceView,
        previous: usize,
        validation_requests: &[QuiescentValidationRequest],
        frame_index: u64,
    ) -> Result<(ActiveDamageSubmission, DeferredReadbackActivity)> {
        let requests = validation_requests
            .iter()
            .cloned()
            .take(MAX_ACTIVE_TILES)
            .filter(|request| {
                let tile = request.source;
                tile.left < tile.right
                    && tile.top < tile.bottom
                    && tile.right <= self.width
                    && tile.bottom <= self.height
                    && tile.right.div_ceil(CTB_SIZE) - tile.left / CTB_SIZE <= 4
                    && tile.bottom.div_ceil(CTB_SIZE) - tile.top / CTB_SIZE <= 4
            })
            .collect::<Vec<_>>();
        let mut deferred = DeferredReadbackActivity::default();
        for slot in 0..self.active_readback_buffers.len() {
            if self.active_readback_ring.pending[slot].is_none() {
                continue;
            }
            let age = self
                .active_readback_ring
                .pending_age(slot, frame_index)
                .unwrap_or_default();
            if let Some(scores) = self.read_active_scores(slot, false)? {
                let batch = self.active_validation_batches[slot]
                    .take()
                    .context("active validation readback lost its request batch")?;
                self.active_readback_ring.complete(slot);
                deferred.ready_ages_frames.push(age);
                deferred.verdicts.extend(active_verdicts_after_collection(
                    &batch.requests,
                    ActiveDamageCollection::Ready(&scores),
                    age,
                ));
            } else {
                deferred.pending_frames = deferred.pending_frames.saturating_add(1);
                deferred.max_pending_age_frames = Some(
                    deferred
                        .max_pending_age_frames
                        .map_or(age, |current| current.max(age)),
                );
            }
        }
        let tiles = requests
            .iter()
            .map(|request| request.source)
            .collect::<Vec<_>>();
        let bounds = active_tile_ctb_bounds(self.width, self.height, &tiles);
        if bounds.is_empty() {
            return Ok((ActiveDamageSubmission::NotRequested, deferred));
        }
        let Some(readback_slot) = self.active_readback_ring.free_slot() else {
            return Ok((ActiveDamageSubmission::Unavailable, deferred));
        };
        let mut constants = [0u32; 20];
        for (index, bound) in bounds.iter().enumerate() {
            constants[index * 4..index * 4 + 4].copy_from_slice(bound);
        }
        constants[16] = bounds.len() as u32;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context.Map(
                &self.active_bounds_buffer,
                0,
                D3D11_MAP_WRITE_DISCARD,
                0,
                Some(&mut mapped),
            )?;
            std::ptr::copy_nonoverlapping(
                constants.as_ptr(),
                mapped.pData.cast::<u32>(),
                constants.len(),
            );
            self.context.Unmap(&self.active_bounds_buffer, 0);
        }
        let shader_resources = [
            Some(current_view.clone()),
            Some(self.texture_views[previous].clone()),
        ];
        let constant_buffers = [Some(self.active_bounds_buffer.clone())];
        let unordered_views = [Some(self.active_score_view.clone())];
        unsafe {
            self.context.CSSetShader(&self.active_shader, None);
            self.context
                .CSSetShaderResources(0, Some(&shader_resources));
            self.context
                .CSSetConstantBuffers(0, Some(&constant_buffers));
            self.context
                .CSSetUnorderedAccessViews(0, 1, Some(unordered_views.as_ptr()), None);
            self.context
                .Dispatch((bounds.len() * ACTIVE_CTBS_PER_TILE) as u32, 1, 1);
            self.context.CSSetShaderResources(0, Some(&[None, None]));
            self.context.CSSetConstantBuffers(0, Some(&[None]));
            self.context.CSSetUnorderedAccessViews(
                0,
                1,
                Some([None::<ID3D11UnorderedAccessView>].as_ptr()),
                None,
            );
            self.context.CSSetShader(None::<&ID3D11ComputeShader>, None);
            self.context.CopyResource(
                &self.active_readback_buffers[readback_slot],
                &self.active_score_buffer,
            );
        }
        self.active_readback_ring.submit(readback_slot, frame_index);
        self.active_validation_batches[readback_slot] = Some(ActiveValidationBatch { requests });
        Ok((ActiveDamageSubmission::Pending { readback_slot }, deferred))
    }

    fn read_active_scores(&self, slot: usize, do_not_wait: bool) -> Result<Option<Vec<u32>>> {
        let flags = if do_not_wait {
            D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32
        } else {
            0
        };
        let mut score_map = D3D11_MAPPED_SUBRESOURCE::default();
        let mapped = unsafe {
            self.context.Map(
                &self.active_readback_buffers[slot],
                0,
                D3D11_MAP_READ,
                flags,
                Some(&mut score_map),
            )
        };
        if let Err(error) = mapped {
            if do_not_wait && error.code() == DXGI_ERROR_WAS_STILL_DRAWING {
                return Ok(None);
            }
            return Err(error).context("active-tile score readback");
        }
        let scores = unsafe {
            slice::from_raw_parts(score_map.pData.cast::<u32>(), MAX_ACTIVE_CTBS as usize).to_vec()
        };
        unsafe {
            self.context.Unmap(&self.active_readback_buffers[slot], 0);
        }
        Ok(Some(scores))
    }

    fn read_features(
        &self,
        readback: usize,
        do_not_wait: bool,
    ) -> Result<Option<(Vec<u32>, Vec<u32>)>> {
        let flags = if do_not_wait {
            D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32
        } else {
            0
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let score_map = unsafe {
            self.context.Map(
                &self.readback_buffers[readback],
                0,
                D3D11_MAP_READ,
                flags,
                Some(&mut mapped),
            )
        };
        if let Err(error) = score_map {
            if do_not_wait && error.code() == DXGI_ERROR_WAS_STILL_DRAWING {
                return Ok(None);
            }
            return Err(error).context("temporal-diff score readback");
        }
        let total_ctbs = (self.columns * self.rows) as usize;
        let scores =
            unsafe { slice::from_raw_parts(mapped.pData.cast::<u32>(), total_ctbs) }.to_vec();
        unsafe {
            self.context.Unmap(&self.readback_buffers[readback], 0);
        }
        let mut edge_mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            let edge_map = self.context.Map(
                &self.edge_readback_buffers[readback],
                0,
                D3D11_MAP_READ,
                flags,
                Some(&mut edge_mapped),
            );
            if let Err(error) = edge_map {
                if do_not_wait && error.code() == DXGI_ERROR_WAS_STILL_DRAWING {
                    return Ok(None);
                }
                return Err(error).context("temporal-diff edge readback");
            }
        }
        let edge_scores =
            unsafe { slice::from_raw_parts(edge_mapped.pData.cast::<u32>(), total_ctbs) }.to_vec();
        unsafe {
            self.context.Unmap(&self.edge_readback_buffers[readback], 0);
        }
        Ok(Some((scores, edge_scores)))
    }

    fn validate_source(&self, source: &ID3D11Texture2D) -> Result<()> {
        let source_device =
            unsafe { source.GetDevice() }.context("temporal-diff source GetDevice")?;
        ensure!(
            source_device.as_raw() == self.device.as_raw(),
            "temporal-diff source belongs to a different D3D11 device"
        );
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        ensure!(
            desc.Width == self.width
                && desc.Height == self.height
                && desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM
                && desc.SampleDesc.Count == 1,
            "temporal-diff source must be single-sample BGRA at {}x{}",
            self.width,
            self.height
        );
        Ok(())
    }
}

fn elapsed_us(started: Instant) -> u32 {
    started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32
}

fn compile_shader(
    device: &ID3D11Device,
    entry_point: PCSTR,
    label: &str,
) -> Result<ID3D11ComputeShader> {
    let mut bytecode: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let compiled = unsafe {
        D3DCompile(
            TEMPORAL_DIFF_SHADER.as_ptr().cast::<c_void>(),
            TEMPORAL_DIFF_SHADER.len(),
            PCSTR(b"bubbleplay-temporal-diff\0".as_ptr()),
            None,
            None::<&ID3DInclude>,
            entry_point,
            PCSTR(b"cs_5_0\0".as_ptr()),
            0,
            0,
            &mut bytecode,
            Some(&mut errors),
        )
    };
    if let Err(error) = compiled {
        let detail = errors
            .as_ref()
            .map(blob_text)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| error.to_string());
        bail!("compile {label} compute shader: {detail}");
    }
    let bytecode = bytecode.context("temporal-diff compiler returned no bytecode")?;
    let bytes = unsafe {
        slice::from_raw_parts(
            bytecode.GetBufferPointer().cast::<u8>(),
            bytecode.GetBufferSize(),
        )
    };
    let mut shader = None;
    unsafe { device.CreateComputeShader(bytes, None, Some(&mut shader)) }
        .with_context(|| format!("CreateComputeShader({label})"))?;
    shader.with_context(|| format!("CreateComputeShader({label}) returned no object"))
}

fn blob_text(blob: &ID3DBlob) -> String {
    let bytes = unsafe {
        slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    };
    String::from_utf8_lossy(bytes)
        .trim_matches(char::from(0))
        .trim()
        .to_string()
}

fn create_history_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("CreateTexture2D(temporal-diff history)")?;
    texture.context("CreateTexture2D(temporal-diff history) returned no object")
}

fn create_texture_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11ShaderResourceView> {
    let mut view = None;
    unsafe { device.CreateShaderResourceView(texture, None, Some(&mut view)) }
        .context("CreateShaderResourceView(temporal-diff history)")?;
    view.context("CreateShaderResourceView(temporal-diff history) returned no object")
}

fn create_score_buffer(device: &ID3D11Device, total_ctbs: u32) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: total_ctbs * std::mem::size_of::<u32>() as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
        StructureByteStride: std::mem::size_of::<u32>() as u32,
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }
        .context("CreateBuffer(temporal-diff scores)")?;
    buffer.context("CreateBuffer(temporal-diff scores) returned no object")
}

fn create_active_bounds_buffer(device: &ID3D11Device) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: (20 * std::mem::size_of::<u32>()) as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }
        .context("CreateBuffer(active-tile bounds)")?;
    buffer.context("CreateBuffer(active-tile bounds) returned no object")
}

fn create_score_view(
    device: &ID3D11Device,
    buffer: &ID3D11Buffer,
    total_ctbs: u32,
) -> Result<ID3D11UnorderedAccessView> {
    let desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_UNKNOWN,
        ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D11_BUFFER_UAV {
                FirstElement: 0,
                NumElements: total_ctbs,
                Flags: 0,
            },
        },
    };
    let mut view = None;
    unsafe { device.CreateUnorderedAccessView(buffer, Some(&desc), Some(&mut view)) }
        .context("CreateUnorderedAccessView(temporal-diff scores)")?;
    view.context("CreateUnorderedAccessView(temporal-diff scores) returned no object")
}

fn create_readback_buffer(device: &ID3D11Device, total_ctbs: u32) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: total_ctbs * std::mem::size_of::<u32>() as u32,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }
        .context("CreateBuffer(temporal-diff readback)")?;
    buffer.context("CreateBuffer(temporal-diff readback) returned no object")
}

fn active_tile_ctb_bounds(width: u32, height: u32, tiles: &[DamageRect]) -> Vec<[u32; 4]> {
    let columns = width.div_ceil(CTB_SIZE);
    let rows = height.div_ceil(CTB_SIZE);
    tiles
        .iter()
        .take(MAX_ACTIVE_TILES)
        .filter_map(|tile| {
            let bounds = [
                (tile.left / CTB_SIZE).min(columns),
                (tile.top / CTB_SIZE).min(rows),
                tile.right.div_ceil(CTB_SIZE).min(columns),
                tile.bottom.div_ceil(CTB_SIZE).min(rows),
            ];
            (bounds[0] < bounds[2]
                && bounds[1] < bounds[3]
                && bounds[2] - bounds[0] <= 4
                && bounds[3] - bounds[1] <= 4)
                .then_some(bounds)
        })
        .collect()
}

fn active_damage_after_collection(
    tiles: &[DamageRect],
    collection: ActiveDamageCollection<'_>,
) -> Vec<DamageRect> {
    let ActiveDamageCollection::Ready(scores) = collection else {
        return tiles.to_vec();
    };
    let required_scores = tiles.len().saturating_mul(ACTIVE_CTBS_PER_TILE);
    if scores.len() < required_scores {
        return tiles.to_vec();
    }
    tiles
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(index, tile)| {
            scores[index * ACTIVE_CTBS_PER_TILE..(index + 1) * ACTIVE_CTBS_PER_TILE]
                .iter()
                .any(|score| *score > 0)
                .then_some(tile)
        })
        .collect()
}

fn active_verdicts_after_collection(
    requests: &[QuiescentValidationRequest],
    collection: ActiveDamageCollection<'_>,
    age_frames: u32,
) -> Vec<QuiescentValidationVerdict> {
    let ActiveDamageCollection::Ready(scores) = collection else {
        return Vec::new();
    };
    let required_scores = requests.len().saturating_mul(ACTIVE_CTBS_PER_TILE);
    if scores.len() < required_scores {
        return Vec::new();
    }
    requests
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, request)| QuiescentValidationVerdict {
            request,
            changed: scores[index * ACTIVE_CTBS_PER_TILE..(index + 1) * ACTIVE_CTBS_PER_TILE]
                .iter()
                .any(|score| *score > 0),
            age_frames,
        })
        .collect()
}

pub fn classify_temporal_ctb_scores(
    width: u32,
    height: u32,
    scores: &[u32],
    threshold: u32,
) -> TemporalMaskDecision {
    let columns = width.div_ceil(CTB_SIZE);
    let rows = height.div_ceil(CTB_SIZE);
    let total_ctbs = columns.saturating_mul(rows);
    let Some(rects) = collect_temporal_damage_rects(width, height, scores, threshold) else {
        return TemporalMaskDecision::Rejected {
            reason: TemporalMaskRejectReason::InvalidGeometry,
            changed_ctbs: 0,
            total_ctbs: 0,
        };
    };
    let changed_ctbs = rects.len() as u32;
    if changed_ctbs == 0 {
        return TemporalMaskDecision::Rejected {
            reason: TemporalMaskRejectReason::EmptyDamage,
            changed_ctbs,
            total_ctbs,
        };
    }
    if changed_ctbs * 2 >= total_ctbs {
        return TemporalMaskDecision::Rejected {
            reason: TemporalMaskRejectReason::CoverageAtLeastHalf,
            changed_ctbs,
            total_ctbs,
        };
    }
    TemporalMaskDecision::Applied {
        rects,
        changed_ctbs,
        total_ctbs,
    }
}

fn collect_temporal_damage_rects(
    width: u32,
    height: u32,
    scores: &[u32],
    threshold: u32,
) -> Option<Vec<DamageRect>> {
    let columns = width.div_ceil(CTB_SIZE);
    let rows = height.div_ceil(CTB_SIZE);
    let total_ctbs = columns.checked_mul(rows)?;
    if width == 0 || height == 0 || scores.len() != total_ctbs as usize {
        return None;
    }
    let mut rects = Vec::new();
    for (index, score) in scores.iter().copied().enumerate() {
        if score < threshold {
            continue;
        }
        let column = index as u32 % columns;
        let row = index as u32 / columns;
        rects.push(DamageRect {
            left: column * CTB_SIZE,
            top: row * CTB_SIZE,
            right: ((column + 1) * CTB_SIZE).min(width),
            bottom: ((row + 1) * CTB_SIZE).min(height),
        });
    }
    Some(rects)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DamageCoverageAudit {
    pub metadata_available: bool,
    pub changed_ctbs: u32,
    pub covered_ctbs: u32,
    pub missed_ctbs: u32,
}

pub fn audit_damage_coverage(
    width: u32,
    height: u32,
    capture_damage: Option<&[DamageRect]>,
    gpu_damage: &[DamageRect],
) -> DamageCoverageAudit {
    let metadata_available = capture_damage.is_some();
    let columns = width.div_ceil(CTB_SIZE);
    let rows = height.div_ceil(CTB_SIZE);
    let Some(total_ctbs) = columns.checked_mul(rows).map(|count| count as usize) else {
        return DamageCoverageAudit {
            metadata_available,
            ..DamageCoverageAudit::default()
        };
    };
    if width == 0 || height == 0 || total_ctbs == 0 {
        return DamageCoverageAudit {
            metadata_available,
            ..DamageCoverageAudit::default()
        };
    }

    let mark = |rects: &[DamageRect]| {
        let mut cells = vec![false; total_ctbs];
        for rect in rects {
            let left = rect.left.min(width);
            let top = rect.top.min(height);
            let right = rect.right.min(width);
            let bottom = rect.bottom.min(height);
            if left >= right || top >= bottom {
                continue;
            }
            for row in top / CTB_SIZE..bottom.div_ceil(CTB_SIZE).min(rows) {
                for column in left / CTB_SIZE..right.div_ceil(CTB_SIZE).min(columns) {
                    cells[(row * columns + column) as usize] = true;
                }
            }
        }
        cells
    };

    let gpu_cells = mark(gpu_damage);
    let capture_cells = capture_damage.map(mark);
    let changed_ctbs = gpu_cells.iter().filter(|changed| **changed).count() as u32;
    let covered_ctbs = capture_cells.as_ref().map_or(0, |capture_cells| {
        gpu_cells
            .iter()
            .zip(capture_cells)
            .filter(|(changed, covered)| **changed && **covered)
            .count() as u32
    });
    DamageCoverageAudit {
        metadata_available,
        changed_ctbs,
        covered_ctbs,
        missed_ctbs: changed_ctbs.saturating_sub(covered_ctbs),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        active_damage_after_collection, active_tile_ctb_bounds, active_verdicts_after_collection,
        audit_damage_coverage, classify_temporal_ctb_scores, collect_temporal_damage_rects,
        ActiveDamageCollection, ActiveDamageSubmission, QuiescentValidationStatus, ReadbackRing,
        TemporalDiffAnalyzer, TemporalMaskDecision, TemporalMaskRejectReason,
        DEFAULT_CHANGE_SCORE_THRESHOLD,
    };
    use crate::frame_damage::DamageRect;
    use crate::hybrid::QuiescentValidationRequest;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
        D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

    fn test_texture(
        width: u32,
        height: u32,
    ) -> (ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D) {
        let mut device = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .expect("create WARP D3D11 device for temporal-diff test");
        }
        let device = device.expect("D3D11 test device");
        let context = context.expect("D3D11 test context");
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .expect("create temporal-diff test texture");
        }
        let texture = texture.expect("D3D11 test texture");
        let pixels = vec![0u8; width as usize * height as usize * 4];
        unsafe {
            context.UpdateSubresource(&texture, 0, None, pixels.as_ptr().cast(), width * 4, 0);
        }
        (device, context, texture)
    }

    #[test]
    fn current_frame_validation_stays_pending_until_the_next_frame() {
        let width = 256;
        let height = 128;
        let (device, _, texture) = test_texture(width, height);
        let mut analyzer = TemporalDiffAnalyzer::new(&device, width, height)
            .expect("create temporal-diff analyzer");
        let priming = analyzer
            .submit_quiescent(&texture, &[], None)
            .expect("submit priming frame");
        analyzer.collect_quiescent(priming);

        let request = QuiescentValidationRequest {
            tile_id: 1,
            source: DamageRect {
                left: 0,
                top: 0,
                right: 128,
                bottom: 128,
            },
            content_revision: 7,
            generation: 3,
            crop_revision: 2,
            validation_sequence: 41,
        };
        let submission = analyzer
            .submit_quiescent(&texture, std::slice::from_ref(&request), None)
            .expect("submit current-frame validation");

        assert!(matches!(
            submission.active,
            ActiveDamageSubmission::Pending { .. }
        ));
        assert!(submission.deferred_readback.verdicts.is_empty());

        let completion = analyzer.collect_quiescent(submission);
        assert_eq!(
            completion.validation_status,
            QuiescentValidationStatus::Pending
        );
        assert!(completion.deferred_readback.ready_ages_frames.is_empty());
        assert!(completion.deferred_readback.verdicts.is_empty());
        assert_eq!(completion.deferred_readback.pending_frames, 1);
        assert_eq!(completion.deferred_readback.max_pending_age_frames, Some(0));

        let next_submission = analyzer
            .submit_quiescent(&texture, &[], None)
            .expect("submit following frame");
        let next_completion = analyzer.collect_quiescent(next_submission);

        assert_eq!(
            next_completion.validation_status,
            QuiescentValidationStatus::Ready
        );
        assert_eq!(next_completion.deferred_readback.ready_ages_frames, vec![1]);
        assert_eq!(next_completion.deferred_readback.verdicts.len(), 1);
        assert_eq!(
            next_completion.deferred_readback.verdicts[0].request,
            request
        );
        assert_eq!(next_completion.deferred_readback.verdicts[0].age_frames, 1);
        assert!(!next_completion.deferred_readback.verdicts[0].changed);
    }

    #[test]
    fn next_frame_validation_detects_a_single_channel_lsb_change_inside_an_active_tile() {
        let width = 128;
        let height = 128;
        let (device, context, texture) = test_texture(width, height);
        let mut analyzer = TemporalDiffAnalyzer::new(&device, width, height)
            .expect("create temporal-diff analyzer");
        let priming = analyzer
            .submit_quiescent(&texture, &[], None)
            .expect("submit priming frame");
        analyzer.collect_quiescent(priming);

        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        pixels[((64 * width + 64) * 4) as usize] = 1;
        unsafe {
            context.UpdateSubresource(&texture, 0, None, pixels.as_ptr().cast(), width * 4, 0);
        }
        let request = QuiescentValidationRequest {
            tile_id: 1,
            source: DamageRect {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            content_revision: 7,
            generation: 3,
            crop_revision: 2,
            validation_sequence: 42,
        };
        let submission = analyzer
            .submit_quiescent(&texture, std::slice::from_ref(&request), Some(&[]))
            .expect("submit subtle current-frame validation");
        let completion = analyzer.collect_quiescent(submission);

        assert_eq!(
            completion.validation_status,
            QuiescentValidationStatus::Pending
        );
        assert!(completion.deferred_readback.verdicts.is_empty());

        let next_submission = analyzer
            .submit_quiescent(&texture, &[], Some(&[]))
            .expect("submit following frame");
        let next_completion = analyzer.collect_quiescent(next_submission);

        assert_eq!(
            next_completion.validation_status,
            QuiescentValidationStatus::Ready
        );
        assert_eq!(next_completion.deferred_readback.verdicts.len(), 1);
        assert_eq!(
            next_completion.deferred_readback.verdicts[0].request,
            request
        );
        assert_eq!(next_completion.deferred_readback.verdicts[0].age_frames, 1);
        assert!(next_completion.deferred_readback.verdicts[0].changed);
    }

    #[test]
    fn double_buffered_readback_consumes_only_the_previous_completed_slot() {
        let mut ring = ReadbackRing::default();
        assert_eq!(ring.oldest_pending(), None);
        assert_eq!(ring.free_slot(), Some(0));
        ring.submit(0, 41);
        assert_eq!(ring.oldest_pending(), Some(0));
        assert_eq!(ring.pending_age(0, 41), Some(0));
        assert_eq!(ring.pending_age(0, 43), Some(2));
        assert_eq!(ring.free_slot(), Some(1));
        ring.submit(1, 42);
        assert_eq!(ring.oldest_pending(), Some(0));
        assert_eq!(ring.free_slot(), None);
        ring.complete(0);
        assert_eq!(ring.oldest_pending(), Some(1));
        assert_eq!(ring.free_slot(), Some(0));
        ring.submit(0, 44);
        assert_eq!(ring.oldest_pending(), Some(1));
        ring.reset();
        assert_eq!(ring.oldest_pending(), None);
        assert_eq!(ring.free_slot(), Some(0));
    }

    #[test]
    fn dda_damage_audit_reports_exact_gpu_changed_ctb_misses() {
        let gpu_changes = [
            DamageRect {
                left: 32,
                top: 0,
                right: 64,
                bottom: 32,
            },
            DamageRect {
                left: 96,
                top: 32,
                right: 128,
                bottom: 64,
            },
        ];
        let dda_damage = [DamageRect {
            left: 40,
            top: 8,
            right: 56,
            bottom: 24,
        }];

        let audit = audit_damage_coverage(128, 64, Some(&dda_damage), &gpu_changes);
        assert!(audit.metadata_available);
        assert_eq!(audit.changed_ctbs, 2);
        assert_eq!(audit.covered_ctbs, 1);
        assert_eq!(audit.missed_ctbs, 1);

        let unavailable = audit_damage_coverage(128, 64, None, &gpu_changes);
        assert!(!unavailable.metadata_available);
        assert_eq!(unavailable.changed_ctbs, 2);
        assert_eq!(unavailable.covered_ctbs, 0);
        assert_eq!(unavailable.missed_ctbs, 2);
    }

    #[test]
    fn active_tile_check_covers_at_most_four_ctb_aligned_detail_tiles() {
        let tiles = [
            DamageRect {
                left: 0,
                top: 0,
                right: 128,
                bottom: 128,
            },
            DamageRect {
                left: 192,
                top: 32,
                right: 256,
                bottom: 96,
            },
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
            DamageRect {
                left: 64,
                top: 0,
                right: 96,
                bottom: 32,
            },
        ];

        assert_eq!(
            active_tile_ctb_bounds(256, 128, &tiles),
            vec![[0, 0, 4, 4], [6, 1, 8, 3], [0, 0, 1, 1], [1, 0, 2, 1]]
        );
    }

    #[test]
    fn unavailable_active_readback_invalidates_every_tile_conservatively() {
        let tiles = vec![
            DamageRect {
                left: 0,
                top: 0,
                right: 128,
                bottom: 128,
            },
            DamageRect {
                left: 256,
                top: 128,
                right: 384,
                bottom: 256,
            },
        ];

        assert_eq!(
            active_damage_after_collection(&tiles, ActiveDamageCollection::Unavailable),
            tiles
        );
        assert_eq!(
            active_damage_after_collection(&tiles, ActiveDamageCollection::Failed),
            tiles
        );
    }

    #[test]
    fn completed_active_readback_invalidates_only_tiles_with_changed_ctbs() {
        let tiles = vec![
            DamageRect {
                left: 0,
                top: 0,
                right: 128,
                bottom: 128,
            },
            DamageRect {
                left: 256,
                top: 128,
                right: 384,
                bottom: 256,
            },
        ];
        let mut scores = vec![0; 2 * super::ACTIVE_CTBS_PER_TILE];
        scores[super::ACTIVE_CTBS_PER_TILE + 3] = 1;

        assert_eq!(
            active_damage_after_collection(&tiles, ActiveDamageCollection::Ready(&scores)),
            vec![tiles[1]]
        );
    }

    #[test]
    fn deferred_active_readback_preserves_exact_request_identity_and_emits_no_missing_verdict() {
        let requests = vec![
            QuiescentValidationRequest {
                tile_id: 1,
                source: DamageRect {
                    left: 0,
                    top: 0,
                    right: 128,
                    bottom: 128,
                },
                content_revision: 10,
                generation: 7,
                crop_revision: 3,
                validation_sequence: 41,
            },
            QuiescentValidationRequest {
                tile_id: 2,
                source: DamageRect {
                    left: 128,
                    top: 0,
                    right: 256,
                    bottom: 128,
                },
                content_revision: 9,
                generation: 7,
                crop_revision: 3,
                validation_sequence: 41,
            },
        ];
        let mut scores = vec![0; 2 * super::ACTIVE_CTBS_PER_TILE];
        scores[super::ACTIVE_CTBS_PER_TILE + 3] = 1;

        let verdicts =
            active_verdicts_after_collection(&requests, ActiveDamageCollection::Ready(&scores), 1);
        assert_eq!(verdicts.len(), 2);
        assert_eq!(verdicts[0].request, requests[0]);
        assert!(!verdicts[0].changed);
        assert_eq!(verdicts[0].age_frames, 1);
        assert_eq!(verdicts[1].request, requests[1]);
        assert!(verdicts[1].changed);
        assert!(active_verdicts_after_collection(
            &requests,
            ActiveDamageCollection::Unavailable,
            2,
        )
        .is_empty());
        assert!(
            active_verdicts_after_collection(&requests, ActiveDamageCollection::Failed, 2,)
                .is_empty()
        );
    }

    #[test]
    fn localized_temporal_score_becomes_one_exact_ctb_damage_rect() {
        let mut scores = vec![0; 8];
        scores[1] = DEFAULT_CHANGE_SCORE_THRESHOLD;

        assert_eq!(
            classify_temporal_ctb_scores(128, 64, &scores, DEFAULT_CHANGE_SCORE_THRESHOLD),
            TemporalMaskDecision::Applied {
                rects: vec![DamageRect {
                    left: 32,
                    top: 0,
                    right: 64,
                    bottom: 32,
                }],
                changed_ctbs: 1,
                total_ctbs: 8,
            }
        );
    }

    #[test]
    fn temporal_scores_reject_empty_half_frame_and_invalid_grids() {
        assert_eq!(
            classify_temporal_ctb_scores(128, 64, &[0; 8], DEFAULT_CHANGE_SCORE_THRESHOLD),
            TemporalMaskDecision::Rejected {
                reason: TemporalMaskRejectReason::EmptyDamage,
                changed_ctbs: 0,
                total_ctbs: 8,
            }
        );
        let mut half = [0; 8];
        half[..4].fill(DEFAULT_CHANGE_SCORE_THRESHOLD);
        assert_eq!(
            classify_temporal_ctb_scores(128, 64, &half, DEFAULT_CHANGE_SCORE_THRESHOLD),
            TemporalMaskDecision::Rejected {
                reason: TemporalMaskRejectReason::CoverageAtLeastHalf,
                changed_ctbs: 4,
                total_ctbs: 8,
            }
        );
        assert_eq!(
            classify_temporal_ctb_scores(128, 64, &[0; 7], DEFAULT_CHANGE_SCORE_THRESHOLD),
            TemporalMaskDecision::Rejected {
                reason: TemporalMaskRejectReason::InvalidGeometry,
                changed_ctbs: 0,
                total_ctbs: 0,
            }
        );
    }

    #[test]
    fn coverage_rejection_keeps_exact_ctb_rects_for_encoder_telemetry() {
        let mut half = [0; 8];
        half[..4].fill(DEFAULT_CHANGE_SCORE_THRESHOLD);

        assert_eq!(
            collect_temporal_damage_rects(128, 64, &half, DEFAULT_CHANGE_SCORE_THRESHOLD),
            Some(vec![
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
                DamageRect {
                    left: 64,
                    top: 0,
                    right: 96,
                    bottom: 32,
                },
                DamageRect {
                    left: 96,
                    top: 0,
                    right: 128,
                    bottom: 32,
                },
            ])
        );
    }
}
