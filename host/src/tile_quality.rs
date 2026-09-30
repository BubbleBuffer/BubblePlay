//! Content-aware refinement for WSS v4 (pure logic, no GPU).
//!
//! The captured desktop is split into 64x64 source-pixel tiles. Each tile
//! remembers the hash of the content the receiver last got for it and whether
//! that encode was sharp or left quality debt (encoded while the frame QP was
//! above the target). The producer loop:
//!
//! 1. [`TileQualityTracker::note_damage`] for every captured DDA frame, even
//!    one that is later skipped, so damage is never lost;
//! 2. hashes the pending dirty tiles and asks [`TileQualityTracker::classify`]
//!    whether anything really changed (a no-op repaint is not encoded);
//! 3. asks [`TileQualityTracker::refine_plan`] for debt tiles that have been
//!    stable long enough, and encodes them with a negative delta QP
//!    ([`refine_qp_map`]) while unchanged areas stay skipped;
//! 4. reports the encoded frame with [`TileQualityTracker::on_encoded`].
//!
//! A tile that returns to earlier content is still re-encoded from scratch by
//! the codec (single reference frame, no long-term references), so earlier
//! sharp encodes of the same content are not remembered: the new encode's QP
//! decides whether it needs refinement.
use std::time::{Duration, Instant};

use crate::frame_damage::{DamageRect, HevcQpDeltaMap};

pub const TILE_SIZE: u32 = 64;
/// A debt tile must hold the same content this long before it is refined.
pub const REFINE_STABLE: Duration = Duration::from_millis(300);
/// At most one refine frame per this interval (<= 2/s).
pub const REFINE_MIN_INTERVAL: Duration = Duration::from_millis(500);
/// A captured frame whose damage covers at least this share of the frame (per
/// mille) is large motion; no refine is sent until it has stopped for
/// [`LARGE_MOTION_HOLD`].
pub const LARGE_MOTION_PER_MILLE: u64 = 20;
pub const LARGE_MOTION_HOLD: Duration = Duration::from_millis(300);
/// Tiles encoded in a frame whose average QP is above this carry quality debt.
pub const DEBT_QP: u32 = 28;
/// Refine the whole frame (no QP map) once at least this share of the tiles
/// (per mille) is eligible.
pub const WHOLE_FRAME_REFINE_PER_MILLE: u64 = 500;
/// Delta QP for refined tiles, for tiles that changed in the refine frame and
/// for unchanged tiles (which the encoder skips anyway).
pub const REFINE_DELTA_QP: i8 = -6;
pub const CHANGED_DELTA_QP: i8 = 0;
pub const UNCHANGED_DELTA_QP: i8 = 2;
/// HEVC QP-map granularity used by NVENC in this host.
pub const QP_MAP_CTB: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileQuality {
    Sharp,
    Debt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileState {
    /// Hash of the content the receiver last got (None: unknown).
    pub hash: Option<u64>,
    pub quality: TileQuality,
    /// When this tile's content last changed (or was last re-encoded by a
    /// keyframe).
    pub stable_since: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileGrid {
    pub width: u32,
    pub height: u32,
    pub columns: u32,
    pub rows: u32,
}

impl TileGrid {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            columns: width.div_ceil(TILE_SIZE),
            rows: height.div_ceil(TILE_SIZE),
        }
    }

    pub fn tile_count(&self) -> usize {
        (self.columns * self.rows) as usize
    }

    pub fn tile_rect(&self, index: usize) -> DamageRect {
        let column = index as u32 % self.columns;
        let row = index as u32 / self.columns;
        DamageRect {
            left: column * TILE_SIZE,
            top: row * TILE_SIZE,
            right: ((column + 1) * TILE_SIZE).min(self.width),
            bottom: ((row + 1) * TILE_SIZE).min(self.height),
        }
    }

    /// Marks every tile `rect` touches.
    fn mark(&self, rect: &DamageRect, marks: &mut [bool]) {
        let right = rect.right.min(self.width);
        let bottom = rect.bottom.min(self.height);
        if rect.left >= right || rect.top >= bottom {
            return;
        }
        for row in rect.top / TILE_SIZE..bottom.div_ceil(TILE_SIZE) {
            for column in rect.left / TILE_SIZE..right.div_ceil(TILE_SIZE) {
                marks[(row * self.columns + column) as usize] = true;
            }
        }
    }
}

/// Share of the frame covered by `rects` in per mille (overlaps count twice,
/// capped at the whole frame). `None` is the whole frame.
pub fn damage_share_per_mille(rects: Option<&[DamageRect]>, width: u32, height: u32) -> u64 {
    let total = u64::from(width) * u64::from(height);
    let Some(rects) = rects else {
        return 1_000;
    };
    if total == 0 {
        return 0;
    }
    let area: u64 = rects
        .iter()
        .map(|rect| {
            let w = rect.right.min(width).saturating_sub(rect.left);
            let h = rect.bottom.min(height).saturating_sub(rect.top);
            u64::from(w) * u64::from(h)
        })
        .sum();
    area.min(total) * 1_000 / total
}

/// Result of comparing the pending dirty tiles' fresh hashes with the hashes
/// the receiver already has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    /// Nothing the receiver shows would change: skip the encode.
    pub noop: bool,
    /// Tiles whose content differs (or is unknown).
    pub changed: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefinePlan {
    /// Debt tiles to refine (every eligible one, in index order).
    pub tiles: Vec<usize>,
    /// Most tiles are eligible: refine the whole frame without a QP map.
    pub whole_frame: bool,
}

/// Cumulative counters for the 1 s link totals.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TileStats {
    pub noop_repaints_skipped: u64,
    pub refine_frames: u64,
    pub whole_frame_refines: u64,
    pub tiles_refined: u64,
    pub hashed_tiles: u64,
    pub unhashed_tiles: u64,
}

#[derive(Debug)]
pub struct TileQualityTracker {
    grid: TileGrid,
    tiles: Vec<TileState>,
    /// Damage accumulated since the last encoded (or no-op) frame.
    pending: Vec<bool>,
    pending_any: bool,
    motion_until: Option<Instant>,
    last_refine: Option<Instant>,
    stats: TileStats,
}

impl TileQualityTracker {
    pub fn new(now: Instant, width: u32, height: u32) -> Self {
        let grid = TileGrid::new(width, height);
        let count = grid.tile_count();
        Self {
            grid,
            tiles: vec![
                TileState {
                    hash: None,
                    quality: TileQuality::Sharp,
                    stable_since: now,
                };
                count
            ],
            pending: vec![false; count],
            pending_any: false,
            motion_until: None,
            last_refine: None,
            stats: TileStats::default(),
        }
    }

    pub fn grid(&self) -> TileGrid {
        self.grid
    }

    pub fn tile(&self, index: usize) -> TileState {
        self.tiles[index]
    }

    pub fn stats(&self) -> TileStats {
        self.stats
    }

    pub fn is_for(&self, width: u32, height: u32) -> bool {
        self.grid.width == width && self.grid.height == height
    }

    /// Forget everything the receiver has (new decoder chain, new capture
    /// source). Counters are kept.
    pub fn reset(&mut self, now: Instant) {
        let stats = self.stats;
        *self = Self::new(now, self.grid.width, self.grid.height);
        self.stats = stats;
    }

    /// Record one captured frame's damage (`None`: unknown, the whole frame).
    pub fn note_damage(&mut self, now: Instant, rects: Option<&[DamageRect]>) {
        let share = damage_share_per_mille(rects, self.grid.width, self.grid.height);
        if share >= LARGE_MOTION_PER_MILLE {
            self.motion_until = Some(now + LARGE_MOTION_HOLD);
        }
        match rects {
            None => self.pending.fill(true),
            Some(rects) => {
                for rect in rects {
                    self.grid.mark(rect, &mut self.pending);
                }
            }
        }
        self.pending_any = true;
    }

    /// Whether any captured frame has been noted since the last encode.
    pub fn has_pending_frame(&self) -> bool {
        self.pending_any
    }

    pub fn dirty_tiles(&self) -> Vec<usize> {
        self.pending
            .iter()
            .enumerate()
            .filter_map(|(index, dirty)| dirty.then_some(index))
            .collect()
    }

    /// Share of the frame that is dirty (per mille of tiles).
    pub fn dirty_per_mille(&self) -> u64 {
        let dirty = self.pending.iter().filter(|dirty| **dirty).count() as u64;
        dirty * 1_000 / self.tiles.len().max(1) as u64
    }

    pub fn in_large_motion(&self, now: Instant) -> bool {
        self.motion_until.is_some_and(|until| now < until)
    }

    /// Compare fresh hashes of the dirty tiles (`None`: not hashed) with what
    /// the receiver has.
    pub fn classify(&self, hashes: &[(usize, Option<u64>)]) -> Classification {
        let changed: Vec<usize> = hashes
            .iter()
            .filter(|(index, hash)| hash.is_none() || *hash != self.tiles[*index].hash)
            .map(|(index, _)| *index)
            .collect();
        Classification {
            noop: changed.is_empty(),
            changed,
        }
    }

    /// The pending damage was a no-op repaint and nothing is encoded.
    pub fn on_noop_skipped(&mut self) {
        self.pending.fill(false);
        self.pending_any = false;
        self.stats.noop_repaints_skipped += 1;
    }

    fn eligible(&self, now: Instant) -> Vec<usize> {
        self.tiles
            .iter()
            .enumerate()
            .filter(|(index, tile)| {
                tile.quality == TileQuality::Debt
                    && !self.pending[*index]
                    && now.saturating_duration_since(tile.stable_since) >= REFINE_STABLE
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Debt tiles due for refinement now, if a refine may be sent.
    pub fn refine_plan(&self, now: Instant) -> Option<RefinePlan> {
        if self.in_large_motion(now)
            || self
                .last_refine
                .is_some_and(|last| now.saturating_duration_since(last) < REFINE_MIN_INTERVAL)
        {
            return None;
        }
        let tiles = self.eligible(now);
        if tiles.is_empty() {
            return None;
        }
        let whole_frame =
            tiles.len() as u64 * 1_000 >= WHOLE_FRAME_REFINE_PER_MILLE * self.tiles.len() as u64;
        Some(RefinePlan { tiles, whole_frame })
    }

    /// Earliest time a refine could become due without further damage.
    pub fn next_refine_at(&self) -> Option<Instant> {
        let earliest_stable = self
            .tiles
            .iter()
            .enumerate()
            .filter(|(index, tile)| tile.quality == TileQuality::Debt && !self.pending[*index])
            .map(|(_, tile)| tile.stable_since + REFINE_STABLE)
            .min()?;
        let mut at = earliest_stable;
        if let Some(until) = self.motion_until {
            at = at.max(until);
        }
        if let Some(last) = self.last_refine {
            at = at.max(last + REFINE_MIN_INTERVAL);
        }
        Some(at)
    }

    /// Tiles currently carrying quality debt.
    pub fn debt_tiles(&self) -> usize {
        self.tiles
            .iter()
            .filter(|tile| tile.quality == TileQuality::Debt)
            .count()
    }

    /// Record an encoded frame the receiver will get. `hashes` are the dirty
    /// tiles' fresh hashes (from [`Self::dirty_tiles`]); `refined` the
    /// refine plan carried by this frame, if any.
    pub fn on_encoded(
        &mut self,
        now: Instant,
        hashes: &[(usize, Option<u64>)],
        frame_avg_qp: u32,
        key: bool,
        refined: Option<&RefinePlan>,
    ) {
        let quality = if frame_avg_qp > DEBT_QP {
            TileQuality::Debt
        } else {
            TileQuality::Sharp
        };
        let mut changed = vec![false; self.tiles.len()];
        for &(index, hash) in hashes {
            changed[index] = hash.is_none() || hash != self.tiles[index].hash;
        }
        if key {
            // Every tile was coded again from scratch.
            for tile in &mut self.tiles {
                tile.quality = quality;
                tile.stable_since = now;
            }
        }
        for &(index, hash) in hashes {
            if changed[index] {
                let tile = &mut self.tiles[index];
                tile.hash = hash;
                tile.quality = quality;
                tile.stable_since = now;
            }
        }
        if let Some(plan) = refined.filter(|_| !key) {
            let tiles: Vec<usize> = if plan.whole_frame {
                (0..self.tiles.len()).collect()
            } else {
                plan.tiles.clone()
            };
            for index in tiles {
                // A refined tile that also changed in this frame keeps the
                // quality of the change.
                if !changed[index] {
                    self.tiles[index].quality = TileQuality::Sharp;
                }
            }
            self.last_refine = Some(now);
            self.stats.refine_frames += 1;
            self.stats.whole_frame_refines += u64::from(plan.whole_frame);
            self.stats.tiles_refined += plan.tiles.len() as u64;
        }
        self.stats.hashed_tiles += hashes.iter().filter(|(_, hash)| hash.is_some()).count() as u64;
        self.stats.unhashed_tiles +=
            hashes.iter().filter(|(_, hash)| hash.is_none()).count() as u64;
        self.pending.fill(false);
        self.pending_any = false;
    }
}

/// Per-CTB delta-QP map for a refine frame: refined tiles get
/// [`REFINE_DELTA_QP`], tiles that changed in this frame
/// [`CHANGED_DELTA_QP`], the rest [`UNCHANGED_DELTA_QP`]. `crop` is the source
/// rectangle scaled to `output_width` x `output_height`.
pub fn refine_qp_map(
    grid: TileGrid,
    refined: &[usize],
    changed: &[usize],
    crop: DamageRect,
    output_width: u32,
    output_height: u32,
) -> Option<HevcQpDeltaMap> {
    if output_width == 0
        || output_height == 0
        || crop.left >= crop.right
        || crop.top >= crop.bottom
        || crop.right > grid.width
        || crop.bottom > grid.height
    {
        return None;
    }
    let mut level = vec![0u8; grid.tile_count()];
    for &index in changed {
        level[index] = 1;
    }
    for &index in refined {
        level[index] = 2;
    }
    let columns = output_width.div_ceil(QP_MAP_CTB);
    let rows = output_height.div_ceil(QP_MAP_CTB);
    let crop_width = u64::from(crop.right - crop.left);
    let crop_height = u64::from(crop.bottom - crop.top);
    let to_source_x =
        |x: u32| crop.left + (u64::from(x) * crop_width / u64::from(output_width)) as u32;
    let to_source_y =
        |y: u32| crop.top + (u64::from(y) * crop_height / u64::from(output_height)) as u32;
    let mut values = Vec::with_capacity((columns * rows) as usize);
    for row in 0..rows {
        let top = to_source_y(row * QP_MAP_CTB);
        let bottom = to_source_y(((row + 1) * QP_MAP_CTB).min(output_height)).max(top + 1);
        for column in 0..columns {
            let left = to_source_x(column * QP_MAP_CTB);
            let right = to_source_x(((column + 1) * QP_MAP_CTB).min(output_width)).max(left + 1);
            let mut best = 0u8;
            for tile_row in top / TILE_SIZE..bottom.min(grid.height).div_ceil(TILE_SIZE) {
                for tile_column in left / TILE_SIZE..right.min(grid.width).div_ceil(TILE_SIZE) {
                    best = best.max(level[(tile_row * grid.columns + tile_column) as usize]);
                }
            }
            values.push(match best {
                2 => REFINE_DELTA_QP,
                1 => CHANGED_DELTA_QP,
                _ => UNCHANGED_DELTA_QP,
            });
        }
    }
    Some(HevcQpDeltaMap {
        columns,
        rows,
        values,
    })
}

const HASH_SEED: u64 = 0x2d35_8dcc_aa6c_78a5;
const HASH_P1: u64 = 0x8bb8_4b93_962e_acc9;
const HASH_P2: u64 = 0x4b33_a62e_d433_d4a3;

#[inline]
fn fold_mul(a: u64, b: u64) -> u64 {
    let product = u128::from(a) * u128::from(b);
    (product as u64) ^ ((product >> 64) as u64)
}

#[inline]
fn read_u64(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    word[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(word)
}

/// Fast non-cryptographic 64-bit hash of a tile's rows (wyhash-style folded
/// multiply over 16-byte blocks). `rows` yields each row's pixel bytes.
pub fn hash_rows<'a>(rows: impl IntoIterator<Item = &'a [u8]>) -> u64 {
    let mut state = HASH_SEED;
    let mut length = 0u64;
    for row in rows {
        let mut chunks = row.chunks_exact(16);
        for chunk in &mut chunks {
            let a = read_u64(&chunk[..8]);
            let b = read_u64(&chunk[8..]);
            state = fold_mul(a ^ HASH_P1, b ^ state);
        }
        let tail = chunks.remainder();
        if !tail.is_empty() {
            let a = read_u64(&tail[..tail.len().min(8)]);
            let b = if tail.len() > 8 {
                read_u64(&tail[8..])
            } else {
                0
            };
            state = fold_mul(a ^ HASH_P1, b ^ state ^ tail.len() as u64);
        }
        length = length.wrapping_add(row.len() as u64);
        // Row boundaries are part of the content.
        state = fold_mul(state ^ HASH_P2, length ^ HASH_P1);
    }
    fold_mul(state ^ length, HASH_P2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: u32, top: u32, right: u32, bottom: u32) -> DamageRect {
        DamageRect {
            left,
            top,
            right,
            bottom,
        }
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    /// 256x128 frame: 4 x 2 tiles.
    fn tracker(now: Instant) -> TileQualityTracker {
        TileQualityTracker::new(now, 256, 128)
    }

    fn hashes(tracker: &TileQualityTracker, value: u64) -> Vec<(usize, Option<u64>)> {
        tracker
            .dirty_tiles()
            .into_iter()
            .map(|index| (index, Some(value + index as u64)))
            .collect()
    }

    #[test]
    fn grid_covers_partial_edge_tiles() {
        let grid = TileGrid::new(1920, 1080);
        assert_eq!((grid.columns, grid.rows), (30, 17));
        assert_eq!(
            grid.tile_rect(grid.tile_count() - 1),
            rect(1856, 1024, 1920, 1080)
        );
        let mut marks = vec![false; grid.tile_count()];
        grid.mark(&rect(60, 60, 70, 70), &mut marks);
        assert_eq!(marks.iter().filter(|mark| **mark).count(), 4);
    }

    #[test]
    fn damage_is_accumulated_until_a_frame_is_encoded() {
        let start = Instant::now();
        let mut tracker = tracker(start);
        tracker.note_damage(start, Some(&[rect(0, 0, 10, 10)]));
        // This frame is skipped (backpressure); the next one has other damage.
        tracker.note_damage(start + ms(33), Some(&[rect(200, 100, 210, 110)]));
        assert_eq!(tracker.dirty_tiles(), vec![0, 7]);
        let fresh = hashes(&tracker, 100);
        tracker.on_encoded(start + ms(40), &fresh, 20, false, None);
        assert!(tracker.dirty_tiles().is_empty());
        assert_eq!(tracker.tile(7).hash, Some(107));
        // Unknown damage dirties every tile.
        tracker.note_damage(start + ms(80), None);
        assert_eq!(tracker.dirty_tiles().len(), 8);
    }

    #[test]
    fn repaint_with_identical_content_is_a_noop() {
        let start = Instant::now();
        let mut tracker = tracker(start);
        tracker.note_damage(start, Some(&[rect(0, 0, 128, 64)]));
        let fresh = hashes(&tracker, 10);
        tracker.on_encoded(start, &fresh, 20, false, None);
        // The app repaints the same pixels.
        tracker.note_damage(start + ms(100), Some(&[rect(0, 0, 128, 64)]));
        let same = hashes(&tracker, 10);
        let verdict = tracker.classify(&same);
        assert!(verdict.noop);
        tracker.on_noop_skipped();
        assert!(tracker.dirty_tiles().is_empty());
        assert_eq!(tracker.stats().noop_repaints_skipped, 1);
        // One tile really changed.
        tracker.note_damage(start + ms(200), Some(&[rect(0, 0, 128, 64)]));
        let verdict = tracker.classify(&[(0, Some(10)), (1, Some(99))]);
        assert_eq!(
            verdict,
            Classification {
                noop: false,
                changed: vec![1]
            }
        );
        // Tiles that were not hashed are never a no-op.
        assert!(!tracker.classify(&[(0, None)]).noop);
        // A frame with empty damage changed nothing.
        assert!(tracker.classify(&[]).noop);
    }

    #[test]
    fn debt_follows_the_frame_qp_and_skipped_tiles_keep_their_state() {
        let start = Instant::now();
        let mut tracker = tracker(start);
        tracker.note_damage(start, Some(&[rect(0, 0, 64, 64)]));
        tracker.on_encoded(start, &[(0, Some(1))], DEBT_QP + 5, false, None);
        assert_eq!(tracker.tile(0).quality, TileQuality::Debt);
        assert_eq!(tracker.tile(1).quality, TileQuality::Sharp);
        // A later sharp frame for another tile leaves tile 0 in debt.
        tracker.note_damage(start + ms(10), Some(&[rect(64, 0, 128, 64)]));
        tracker.on_encoded(start + ms(10), &[(1, Some(2))], DEBT_QP, false, None);
        assert_eq!(tracker.tile(0).quality, TileQuality::Debt);
        assert_eq!(tracker.tile(1).quality, TileQuality::Sharp);
        // A damaged tile whose hash did not change was not coded again.
        tracker.note_damage(
            start + ms(20),
            Some(&[rect(0, 0, 64, 64), rect(64, 0, 128, 64)]),
        );
        tracker.on_encoded(
            start + ms(20),
            &[(0, Some(1)), (1, Some(3))],
            DEBT_QP + 5,
            false,
            None,
        );
        assert_eq!(tracker.tile(0).stable_since, start);
        assert_eq!(tracker.tile(1).quality, TileQuality::Debt);
        assert_eq!(tracker.debt_tiles(), 2);
    }

    #[test]
    fn keyframe_sets_every_tile_by_its_qp() {
        let start = Instant::now();
        let mut tracker = tracker(start);
        tracker.on_encoded(start, &[], DEBT_QP + 1, true, None);
        assert_eq!(tracker.debt_tiles(), 8);
        tracker.on_encoded(start + ms(5), &[], DEBT_QP - 1, true, None);
        assert_eq!(tracker.debt_tiles(), 0);
    }

    #[test]
    fn refine_waits_for_stable_content_and_is_rate_limited() {
        let start = Instant::now();
        let mut tracker = tracker(start);
        tracker.note_damage(start, Some(&[rect(0, 0, 64, 64)]));
        tracker.on_encoded(start, &[(0, Some(1))], 40, false, None);
        assert_eq!(tracker.next_refine_at(), Some(start + REFINE_STABLE));
        assert_eq!(tracker.refine_plan(start + ms(299)), None);
        let plan = tracker.refine_plan(start + ms(300)).expect("due");
        assert_eq!(
            plan,
            RefinePlan {
                tiles: vec![0],
                whole_frame: false
            }
        );
        tracker.on_encoded(start + ms(300), &[], 22, false, Some(&plan));
        assert_eq!(tracker.tile(0).quality, TileQuality::Sharp);
        assert_eq!(tracker.stats().refine_frames, 1);
        assert_eq!(tracker.stats().tiles_refined, 1);
        // New debt right away: the next refine respects the rate limit.
        tracker.note_damage(start + ms(310), Some(&[rect(64, 0, 128, 64)]));
        tracker.on_encoded(start + ms(310), &[(1, Some(2))], 40, false, None);
        assert_eq!(tracker.refine_plan(start + ms(700)), None);
        assert_eq!(tracker.next_refine_at(), Some(start + ms(800)));
        assert!(tracker.refine_plan(start + ms(800)).is_some());
    }

    #[test]
    fn changing_or_pending_tiles_are_not_refined() {
        let start = Instant::now();
        let mut tracker = tracker(start);
        tracker.note_damage(start, Some(&[rect(0, 0, 128, 64)]));
        tracker.on_encoded(start, &[(0, Some(1)), (1, Some(2))], 40, false, None);
        // Tile 1 keeps changing (a spinner) at a small damage share.
        tracker.note_damage(start + ms(200), Some(&[rect(64, 0, 80, 16)]));
        tracker.on_encoded(start + ms(200), &[(1, Some(3))], 40, false, None);
        let plan = tracker
            .refine_plan(start + ms(350))
            .expect("tile 0 is stable");
        assert_eq!(plan.tiles, vec![0]);
        // Damage noted but not encoded yet: tile 0 is not eligible, and tile 1
        // has been stable for only 250 ms.
        tracker.note_damage(start + ms(400), Some(&[rect(0, 0, 8, 8)]));
        assert_eq!(tracker.refine_plan(start + ms(450)), None);
    }

    #[test]
    fn large_motion_blocks_refines_until_it_stops() {
        let start = Instant::now();
        let mut tracker = TileQualityTracker::new(start, 1920, 1080);
        tracker.note_damage(start, Some(&[rect(0, 0, 64, 64)]));
        tracker.on_encoded(start, &[(0, Some(1))], 40, false, None);
        // A scroll somewhere else covers a quarter of the frame.
        let scroll = rect(960, 0, 1920, 540);
        for step in 1..=10u64 {
            tracker.note_damage(start + ms(33 * step), Some(&[scroll]));
            assert!(tracker.in_large_motion(start + ms(33 * step)));
            assert_eq!(tracker.refine_plan(start + ms(33 * step + 1)), None);
        }
        tracker.on_encoded(start + ms(330), &[], 40, false, None);
        assert_eq!(tracker.refine_plan(start + ms(500)), None);
        let plan = tracker
            .refine_plan(start + ms(630))
            .expect("motion stopped");
        assert_eq!(plan.tiles, vec![0]);
    }

    #[test]
    fn most_tiles_in_debt_refine_the_whole_frame() {
        let start = Instant::now();
        let mut tracker = tracker(start);
        tracker.on_encoded(start, &[], 40, true, None);
        let plan = tracker.refine_plan(start + REFINE_STABLE).expect("due");
        assert!(plan.whole_frame);
        assert_eq!(plan.tiles.len(), 8);
        tracker.on_encoded(start + REFINE_STABLE, &[], 20, false, Some(&plan));
        assert_eq!(tracker.debt_tiles(), 0);
        assert_eq!(tracker.stats().whole_frame_refines, 1);
    }

    #[test]
    fn refined_tile_that_changed_again_keeps_the_new_quality() {
        let start = Instant::now();
        let mut tracker = tracker(start);
        tracker.note_damage(start, Some(&[rect(0, 0, 64, 64)]));
        tracker.on_encoded(start, &[(0, Some(1))], 40, false, None);
        let plan = tracker.refine_plan(start + ms(400)).expect("due");
        tracker.note_damage(start + ms(400), Some(&[rect(0, 0, 4, 4)]));
        tracker.on_encoded(start + ms(400), &[(0, Some(9))], 40, false, Some(&plan));
        assert_eq!(tracker.tile(0).quality, TileQuality::Debt);
    }

    #[test]
    fn reset_forgets_hashes_but_keeps_counters() {
        let start = Instant::now();
        let mut tracker = tracker(start);
        tracker.note_damage(start, Some(&[rect(0, 0, 64, 64)]));
        tracker.on_noop_skipped();
        tracker.note_damage(start, Some(&[rect(0, 0, 64, 64)]));
        tracker.on_encoded(start, &[(0, Some(1))], 20, false, None);
        tracker.reset(start + ms(1));
        assert_eq!(tracker.tile(0).hash, None);
        assert!(!tracker.classify(&[(0, Some(1))]).noop);
        assert_eq!(tracker.stats().noop_repaints_skipped, 1);
    }

    #[test]
    fn refine_map_marks_ctbs_by_tile_level() {
        let grid = TileGrid::new(256, 128);
        let full = rect(0, 0, 256, 128);
        let map = refine_qp_map(grid, &[0], &[3], full, 256, 128).expect("map");
        assert_eq!((map.columns, map.rows), (8, 4));
        // Tile 0 = CTBs (0..2, 0..2).
        assert_eq!(
            &map.values[0..4],
            &[
                REFINE_DELTA_QP,
                REFINE_DELTA_QP,
                UNCHANGED_DELTA_QP,
                UNCHANGED_DELTA_QP
            ]
        );
        assert_eq!(map.values[8], REFINE_DELTA_QP);
        // Tile 3 = CTB columns 6..8 of rows 0..2.
        assert_eq!(&map.values[6..8], &[CHANGED_DELTA_QP, CHANGED_DELTA_QP]);
        assert_eq!(map.values[31], UNCHANGED_DELTA_QP);
        // Downscaled output (half size): one CTB covers a whole tile.
        let map = refine_qp_map(grid, &[5], &[], full, 128, 64).expect("map");
        assert_eq!((map.columns, map.rows), (4, 2));
        assert_eq!(map.values[5], REFINE_DELTA_QP);
        assert_eq!(
            map.values
                .iter()
                .filter(|value| **value == REFINE_DELTA_QP)
                .count(),
            1
        );
        // A crop maps output CTBs into the cropped source.
        let map = refine_qp_map(grid, &[7], &[], rect(128, 64, 256, 128), 64, 32).expect("map");
        assert_eq!(map.values, vec![UNCHANGED_DELTA_QP, REFINE_DELTA_QP]);
        assert!(refine_qp_map(grid, &[0], &[], rect(0, 0, 300, 128), 64, 32).is_none());
    }

    #[test]
    fn hash_distinguishes_content_rows_and_position() {
        let tile = vec![7u8; 256 * 64];
        let rows = |data: &[u8]| hash_rows(data.chunks(256).collect::<Vec<_>>());
        let base = rows(&tile);
        assert_eq!(base, rows(&tile.clone()));
        let mut one_pixel = tile.clone();
        one_pixel[256 * 40 + 13] ^= 1;
        assert_ne!(base, rows(&one_pixel));
        // Same bytes split into different row lengths differ.
        assert_ne!(base, hash_rows(tile.chunks(128).collect::<Vec<_>>()));
        // Tail handling for odd widths.
        let narrow = vec![1u8; 20 * 4];
        let mut narrow_changed = narrow.clone();
        narrow_changed[79] = 2;
        assert_ne!(
            hash_rows([narrow.as_slice()]),
            hash_rows([narrow_changed.as_slice()])
        );
    }
}
