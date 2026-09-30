//! Budgeted refinement and content-aware pacing on a constrained link.
//!
//! Changed tiles always go first: they are coded in whatever frame carries
//! them. Refinement of debt tiles is paid from the budget left over, nearest
//! to the pointer first. On an unconstrained link (no budget) the refine plan
//! is left exactly as the tile tracker made it.
//!
//! [`V4Pacer`] keeps the per-frame byte budget (a token bucket filled at
//! capacity x 80 %), the content classifier and the resulting behaviour on a
//! constrained link: text-dominant screens get an NVENC max-QP ceiling and
//! budget-paced frame skipping (never under 10 fps); scroll/video-dominant
//! screens keep their frame rate with no ceiling, and their QP map favours
//! the tiles around the pointer. Unconstrained: none of this applies.

use std::time::{Duration, Instant};

use crate::content_class::{ClassCounts, ContentClass, ContentClassifier};
use crate::frame_damage::DamageRect;
use crate::link_capacity::Capacity;
use crate::tile_quality::{RefinePlan, TileGrid, DEBT_QP};

/// Text-dominant ceiling: no tile is coded coarser than the debt threshold.
pub const TEXT_MAX_QP: u32 = DEBT_QP;
/// Paced skipping never holds a change back longer than this (>= 10 fps).
pub const MAX_PACED_INTERVAL: Duration = Duration::from_millis(100);
/// Token-bucket depth: this much airtime at the target rate (the v4
/// in-flight budget).
pub const BUDGET_DEPTH: Duration = crate::wss_v4::INFLIGHT_BUDGET;
/// Scroll/video-dominant: changed tiles this close to the pointer (pixels,
/// to the tile centre) get the refine QP delta.
pub const POINTER_FAVOUR_RADIUS: u32 = 192;

/// First guess for the bytes one refined 64x64 tile costs, before any refine
/// frame has been measured.
pub const REFINE_TILE_BYTES_INITIAL: u64 = 600;
/// Learned per-tile cost stays within these bounds.
pub const REFINE_TILE_BYTES_MIN: u64 = 64;
pub const REFINE_TILE_BYTES_MAX: u64 = 8_192;
/// EWMA weight of a new measurement, in percent.
const COST_EWMA_PCT: u64 = 30;

/// Running estimate of what refinement costs: bytes per refined tile, and the
/// size of an ordinary (non-refine) delta frame to subtract from a refine
/// frame's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefineCost {
    tile_bytes: u64,
    delta_frame_bytes: Option<u64>,
}

impl Default for RefineCost {
    fn default() -> Self {
        Self {
            tile_bytes: REFINE_TILE_BYTES_INITIAL,
            delta_frame_bytes: None,
        }
    }
}

impl RefineCost {
    pub fn tile_bytes(&self) -> u64 {
        self.tile_bytes
    }

    /// Bytes a refine of `tiles` tiles is expected to add.
    pub fn predict(&self, tiles: usize) -> u64 {
        self.tile_bytes.saturating_mul(tiles as u64)
    }

    /// Typical size of an ordinary delta frame with changes.
    pub fn delta_frame_bytes(&self) -> u64 {
        self.delta_frame_bytes.unwrap_or(0)
    }

    /// Record an encoded delta frame. `refined_tiles` is how many tiles it
    /// refined (0 for an ordinary frame); `had_changes` whether it also
    /// carried changed tiles. Keyframes are not recorded.
    pub fn on_frame(&mut self, bytes: u64, refined_tiles: usize, had_changes: bool) {
        if refined_tiles == 0 {
            if had_changes {
                self.delta_frame_bytes = Some(ewma(self.delta_frame_bytes.unwrap_or(bytes), bytes));
            }
            return;
        }
        let base = if had_changes {
            self.delta_frame_bytes.unwrap_or(0)
        } else {
            0
        };
        let per_tile = (bytes.saturating_sub(base) / refined_tiles as u64)
            .clamp(REFINE_TILE_BYTES_MIN, REFINE_TILE_BYTES_MAX);
        self.tile_bytes = ewma(self.tile_bytes, per_tile);
    }
}

fn ewma(old: u64, new: u64) -> u64 {
    (old * (100 - COST_EWMA_PCT) + new * COST_EWMA_PCT) / 100
}

/// Squared distance from the pointer to the centre of `tile`.
fn pointer_distance(grid: &TileGrid, tile: usize, pointer: (u32, u32)) -> u64 {
    let rect = grid.tile_rect(tile);
    let cx = i64::from(rect.left + rect.right) / 2;
    let cy = i64::from(rect.top + rect.bottom) / 2;
    let dx = cx - i64::from(pointer.0);
    let dy = cy - i64::from(pointer.1);
    (dx * dx + dy * dy) as u64
}

/// Order refine candidates nearest to the pointer first (ties and a missing
/// pointer keep index order).
pub fn order_by_pointer(grid: &TileGrid, tiles: &mut [usize], pointer: Option<(u32, u32)>) {
    if let Some(pointer) = pointer {
        tiles.sort_by_key(|&tile| (pointer_distance(grid, tile, pointer), tile));
    }
}

/// Fit a refine plan into `spare_bytes` (the budget left after the frame's
/// changed tiles). `None` budget: unconstrained, the plan is unchanged.
///
/// A whole-frame refine recodes every tile, so it needs the budget for all
/// of them; otherwise it becomes a QP-map refine of the debt tiles when the
/// encoder has a map, or waits. A map refine keeps the tiles nearest the
/// pointer that the budget covers; with no room for even one tile it waits.
pub fn fit_refine_plan(
    grid: &TileGrid,
    mut plan: RefinePlan,
    pointer: Option<(u32, u32)>,
    spare_bytes: Option<u64>,
    cost: &RefineCost,
    qp_map: bool,
) -> Option<RefinePlan> {
    let Some(spare) = spare_bytes else {
        return Some(plan);
    };
    if plan.whole_frame {
        if cost.predict(grid.tile_count()) <= spare {
            return Some(plan);
        }
        if !qp_map {
            return None;
        }
        plan.whole_frame = false;
    }
    if !qp_map {
        // Without a map the encoder cannot refine a subset.
        return (cost.predict(grid.tile_count()) <= spare).then_some(plan);
    }
    let affordable = (spare / cost.tile_bytes().max(1)) as usize;
    if affordable == 0 {
        return None;
    }
    order_by_pointer(grid, &mut plan.tiles, pointer);
    plan.tiles.truncate(affordable);
    Some(plan)
}

/// Pointer position in capture pixels from a normalized (0..=65535) cursor
/// state of a `width` x `height` region.
pub fn pointer_pixels(x: u16, y: u16, width: u32, height: u32) -> (u32, u32) {
    let scale = |value: u16, extent: u32| {
        (u64::from(value) * u64::from(extent.saturating_sub(1)) / u64::from(u16::MAX)) as u32
    };
    (scale(x, width), scale(y, height))
}

/// Per-frame byte budget: a token bucket filled at the capacity target, at
/// most `BUDGET_DEPTH` deep. Empty (and inert) while unconstrained. While
/// the screen is idle (no changed tiles) it fills at the full capacity: the
/// 20 % headroom exists for changes that may arrive, and debt refines are
/// the only traffic then.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FrameBudget {
    tokens: i64,
    last: Option<Instant>,
    rate_bps: u64,
}

impl FrameBudget {
    pub fn refill(&mut self, now: Instant, capacity: Option<Capacity>, idle: bool) {
        let Some(capacity) = capacity else {
            *self = Self::default();
            return;
        };
        self.rate_bps = if idle {
            capacity.bps
        } else {
            capacity.target_bps()
        };
        if let Some(last) = self.last {
            let elapsed = now.saturating_duration_since(last);
            let earned = (u128::from(self.rate_bps) * elapsed.as_micros() / 8_000_000) as i64;
            self.tokens = self.tokens.saturating_add(earned).min(self.depth());
        }
        self.last = Some(now);
    }

    fn depth(&self) -> i64 {
        (u128::from(self.rate_bps) * BUDGET_DEPTH.as_micros() / 8_000_000) as i64
    }

    pub fn tokens(&self) -> i64 {
        self.tokens
    }

    pub fn spend(&mut self, bytes: u64) {
        if self.last.is_some() {
            self.tokens = self.tokens.saturating_sub(bytes as i64);
        }
    }

    /// Time until the bucket holds `bytes` (zero if it already does).
    pub fn time_until(&self, bytes: u64) -> Duration {
        let missing = (bytes as i64).saturating_sub(self.tokens);
        if missing <= 0 || self.rate_bps == 0 {
            return Duration::ZERO;
        }
        Duration::from_micros((missing as u128 * 8_000_000 / u128::from(self.rate_bps)) as u64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaceDecision {
    Encode,
    /// Hold this frame back; the accumulated change is encoded later.
    Skip {
        retry_after: Duration,
    },
}

/// Cumulative counters for the link totals.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PacerStats {
    pub paced_skips: u64,
    pub refines_deferred: u64,
    pub refines_trimmed: u64,
    pub pointer_maps: u64,
}

#[derive(Debug, Default)]
pub struct V4Pacer {
    budget: FrameBudget,
    cost: RefineCost,
    classes: Option<ContentClassifier>,
    capacity: Option<Capacity>,
    last_encode: Option<Instant>,
    stats: PacerStats,
}

impl V4Pacer {
    pub fn stats(&self) -> PacerStats {
        self.stats
    }

    pub fn cost(&self) -> &RefineCost {
        &self.cost
    }

    pub fn tokens(&self) -> i64 {
        self.budget.tokens()
    }

    pub fn capacity(&self) -> Option<Capacity> {
        self.capacity
    }

    /// A new captured frame's damage feeds the content classifier.
    pub fn on_capture(
        &mut self,
        now: Instant,
        width: u32,
        height: u32,
        damage: Option<&[DamageRect]>,
        moved: &[DamageRect],
    ) {
        if self
            .classes
            .as_ref()
            .is_none_or(|classes| !classes.is_for(width, height))
        {
            self.classes = Some(ContentClassifier::new(now, width, height));
        }
        if let Some(classes) = self.classes.as_mut() {
            classes.on_frame(now, damage, moved);
        }
    }

    /// Start an admission: refill the budget and settle the classifier.
    /// `idle`: no tile changed (only a refine can go out).
    pub fn begin_frame(&mut self, now: Instant, capacity: Option<Capacity>, idle: bool) {
        self.capacity = capacity;
        self.budget.refill(now, capacity, idle);
        if let Some(classes) = self.classes.as_mut() {
            classes.update(now);
        }
    }

    pub fn counts(&self) -> ClassCounts {
        self.classes
            .as_ref()
            .map(ContentClassifier::counts)
            .unwrap_or_default()
    }

    pub fn dominant(&self) -> ContentClass {
        self.counts().dominant()
    }

    /// Tiles the scroll detector found shifted.
    pub fn on_scrolled(&mut self, now: Instant, tiles: &[usize]) {
        if let Some(classes) = self.classes.as_mut() {
            classes.on_scrolled(now, tiles);
        }
    }

    fn text_constrained(&self) -> bool {
        self.capacity.is_some() && self.dominant() == ContentClass::Text
    }

    fn motion_constrained(&self) -> bool {
        self.capacity.is_some() && self.dominant().is_motion()
    }

    /// The NVENC max-QP ceiling this frame should run with.
    pub fn max_qp(&self) -> Option<u32> {
        self.text_constrained().then_some(TEXT_MAX_QP)
    }

    /// Text-dominant on a constrained link: skip while the budget is spent,
    /// but never hold a change back longer than `MAX_PACED_INTERVAL`.
    pub fn pace(&mut self, now: Instant, key_due: bool) -> PaceDecision {
        if key_due || !self.text_constrained() || self.budget.tokens() >= 0 {
            return PaceDecision::Encode;
        }
        let Some(last) = self.last_encode else {
            return PaceDecision::Encode;
        };
        let since = now.saturating_duration_since(last);
        if since >= MAX_PACED_INTERVAL {
            return PaceDecision::Encode;
        }
        self.stats.paced_skips += 1;
        PaceDecision::Skip {
            retry_after: self
                .budget
                .time_until(0)
                .min(MAX_PACED_INTERVAL - since)
                .max(Duration::from_millis(1)),
        }
    }

    /// Fit a refine plan into the budget left after this frame's changes
    /// (unchanged when unconstrained). `None` with a plan given means the
    /// refine waits; [`Self::refine_retry_after`] says for how long.
    pub fn fit_refine(
        &mut self,
        grid: &TileGrid,
        plan: RefinePlan,
        pointer: Option<(u32, u32)>,
        has_changes: bool,
        qp_map: bool,
    ) -> Option<RefinePlan> {
        let spare = self.capacity.map(|_| {
            let changes = if has_changes {
                self.cost.delta_frame_bytes()
            } else {
                0
            };
            (self.budget.tokens().max(0) as u64).saturating_sub(changes)
        });
        let wanted = plan.tiles.len();
        let fitted = fit_refine_plan(grid, plan, pointer, spare, &self.cost, qp_map);
        match &fitted {
            None => self.stats.refines_deferred += 1,
            Some(fitted) if fitted.tiles.len() < wanted => self.stats.refines_trimmed += 1,
            Some(_) => {}
        }
        fitted
    }

    /// How long until the budget covers one refined tile.
    pub fn refine_retry_after(&self) -> Duration {
        self.budget
            .time_until(self.cost.tile_bytes())
            .max(Duration::from_millis(1))
    }

    /// Scroll/video-dominant on a constrained link: the changed tiles near
    /// the pointer, to code at a lower QP (empty otherwise).
    pub fn pointer_tiles(
        &mut self,
        grid: &TileGrid,
        pointer: Option<(u32, u32)>,
        changed: &[usize],
    ) -> Vec<usize> {
        let Some(pointer) = pointer.filter(|_| self.motion_constrained()) else {
            return Vec::new();
        };
        let radius = u64::from(POINTER_FAVOUR_RADIUS);
        let near: Vec<usize> = changed
            .iter()
            .copied()
            .filter(|&tile| pointer_distance(grid, tile, pointer) <= radius * radius)
            .collect();
        if !near.is_empty() {
            self.stats.pointer_maps += 1;
        }
        near
    }

    /// Record a frame handed to the link.
    pub fn on_encoded(
        &mut self,
        now: Instant,
        bytes: u64,
        key: bool,
        refined_tiles: usize,
        had_changes: bool,
    ) {
        self.budget.spend(bytes);
        self.last_encode = Some(now);
        if !key {
            self.cost.on_frame(bytes, refined_tiles, had_changes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> TileGrid {
        // 8 x 4 tiles.
        TileGrid::new(512, 256)
    }

    fn plan(tiles: Vec<usize>, whole_frame: bool) -> RefinePlan {
        RefinePlan { tiles, whole_frame }
    }

    #[test]
    fn unconstrained_link_keeps_the_plan() {
        let original = plan(vec![0, 5, 9], false);
        let fitted = fit_refine_plan(
            &grid(),
            original.clone(),
            Some((0, 0)),
            None,
            &RefineCost::default(),
            true,
        );
        assert_eq!(fitted, Some(original));
    }

    #[test]
    fn budget_keeps_the_tiles_nearest_the_pointer() {
        let cost = RefineCost::default();
        // Pointer in tile 30 (column 6, row 3); room for two tiles.
        let fitted = fit_refine_plan(
            &grid(),
            plan(vec![0, 1, 22, 29, 31], false),
            Some((6 * 64 + 10, 3 * 64 + 10)),
            Some(2 * cost.tile_bytes() + 1),
            &cost,
            true,
        )
        .unwrap();
        assert!(!fitted.whole_frame);
        let mut tiles = fitted.tiles;
        tiles.sort();
        assert_eq!(tiles, vec![22, 29]);
    }

    #[test]
    fn no_room_for_one_tile_waits() {
        let cost = RefineCost::default();
        let fitted = fit_refine_plan(
            &grid(),
            plan(vec![3], false),
            None,
            Some(cost.tile_bytes() - 1),
            &cost,
            true,
        );
        assert_eq!(fitted, None);
    }

    #[test]
    fn unaffordable_whole_frame_becomes_a_map_refine_or_waits() {
        let cost = RefineCost::default();
        let debt: Vec<usize> = (0..20).collect();
        let spare = Some(4 * cost.tile_bytes());
        let with_map = fit_refine_plan(
            &grid(),
            plan(debt.clone(), true),
            Some((0, 0)),
            spare,
            &cost,
            true,
        )
        .unwrap();
        assert!(!with_map.whole_frame);
        assert_eq!(with_map.tiles.len(), 4);
        assert_eq!(
            fit_refine_plan(&grid(), plan(debt.clone(), true), None, spare, &cost, false),
            None
        );
        let whole = fit_refine_plan(
            &grid(),
            plan(debt, true),
            None,
            Some(cost.predict(32)),
            &cost,
            false,
        )
        .unwrap();
        assert!(whole.whole_frame);
    }

    #[test]
    fn missing_pointer_keeps_index_order() {
        let cost = RefineCost::default();
        let fitted = fit_refine_plan(
            &grid(),
            plan(vec![7, 2, 9], false),
            None,
            Some(2 * cost.tile_bytes()),
            &cost,
            true,
        )
        .unwrap();
        assert_eq!(fitted.tiles, vec![7, 2]);
    }

    #[test]
    fn cost_learns_bytes_per_refined_tile_over_ordinary_frames() {
        let mut cost = RefineCost::default();
        for _ in 0..20 {
            cost.on_frame(2_000, 0, true);
        }
        for _ in 0..30 {
            // 10 tiles refined in a 12 000 byte frame: 1 000 bytes a tile.
            cost.on_frame(12_000, 10, true);
        }
        // An idle refine (no changes) has no ordinary frame inside it.
        for _ in 0..30 {
            cost.on_frame(5_000, 10, false);
        }
        assert!(
            (490..=510).contains(&cost.tile_bytes()),
            "{}",
            cost.tile_bytes()
        );
        for _ in 0..30 {
            cost.on_frame(12_000, 10, true);
        }
        assert!(
            (990..=1_010).contains(&cost.tile_bytes()),
            "{}",
            cost.tile_bytes()
        );
        assert_eq!(cost.predict(3), cost.tile_bytes() * 3);
    }

    const FRAME: Duration = Duration::from_micros(33_333);

    fn feed(pacer: &mut V4Pacer, start: Instant, frames: u32, damage: &[DamageRect]) -> Instant {
        let mut now = start;
        for n in 0..frames {
            now = start + FRAME * n;
            pacer.on_capture(now, 512, 256, Some(damage), &[]);
        }
        now
    }

    fn typing() -> DamageRect {
        DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 64,
        }
    }

    fn video() -> DamageRect {
        DamageRect {
            left: 0,
            top: 0,
            right: 512,
            bottom: 256,
        }
    }

    #[test]
    fn budget_fills_at_the_target_and_caps_at_the_depth() {
        let start = Instant::now();
        let capacity = Some(Capacity { bps: 2_000_000 });
        let mut budget = FrameBudget::default();
        budget.refill(start, capacity, false);
        assert_eq!(budget.tokens(), 0);
        // 1.6 Mbps for 100 ms = 20 000 bytes; depth 150 ms = 30 000.
        budget.refill(start + Duration::from_millis(100), capacity, false);
        assert_eq!(budget.tokens(), 20_000);
        budget.refill(start + Duration::from_secs(5), capacity, false);
        assert_eq!(budget.tokens(), 30_000);
        budget.spend(40_000);
        assert_eq!(budget.time_until(0), Duration::from_millis(50));
        budget.refill(start + Duration::from_secs(6), None, false);
        assert_eq!(budget, FrameBudget::default());
    }

    #[test]
    fn an_idle_screen_fills_the_budget_at_the_full_capacity() {
        let start = Instant::now();
        let capacity = Some(Capacity { bps: 2_000_000 });
        let mut budget = FrameBudget::default();
        budget.refill(start, capacity, true);
        // 2 Mbps for 100 ms = 25 000 bytes; depth 150 ms = 37 500.
        budget.refill(start + Duration::from_millis(100), capacity, true);
        assert_eq!(budget.tokens(), 25_000);
        budget.refill(start + Duration::from_secs(5), capacity, true);
        assert_eq!(budget.tokens(), 37_500);
    }

    #[test]
    fn unconstrained_pacer_changes_nothing() {
        let start = Instant::now();
        let mut pacer = V4Pacer::default();
        let now = feed(&mut pacer, start, 3, &[typing()]);
        pacer.begin_frame(now + Duration::from_millis(400), None, false);
        assert_eq!(pacer.dominant(), ContentClass::Text);
        assert_eq!(pacer.max_qp(), None);
        pacer.on_encoded(now, 1_000_000, false, 0, true);
        assert_eq!(pacer.pace(now, false), PaceDecision::Encode);
        let plan = RefinePlan {
            tiles: vec![0, 1, 2],
            whole_frame: false,
        };
        let grid = TileGrid::new(512, 256);
        assert_eq!(
            pacer.fit_refine(&grid, plan.clone(), None, true, true),
            Some(plan)
        );
        assert!(pacer.pointer_tiles(&grid, Some((0, 0)), &[0, 1]).is_empty());
    }

    #[test]
    fn text_on_a_constrained_link_gets_a_ceiling_and_paced_frames() {
        let start = Instant::now();
        let capacity = Some(Capacity { bps: 2_000_000 });
        let mut pacer = V4Pacer::default();
        // Typing: a change every 200 ms.
        let mut now = start;
        for n in 0..5u32 {
            now = start + Duration::from_millis(200) * n;
            pacer.on_capture(now, 512, 256, Some(&[typing()]), &[]);
        }
        pacer.begin_frame(now, capacity, false);
        assert_eq!(pacer.dominant(), ContentClass::Text);
        assert_eq!(pacer.max_qp(), Some(TEXT_MAX_QP));
        // A frame far over the budget: the next changes wait for tokens...
        pacer.on_encoded(now, 40_000, false, 0, true);
        let next = now + FRAME;
        pacer.begin_frame(next, capacity, false);
        let PaceDecision::Skip { retry_after } = pacer.pace(next, false) else {
            panic!("expected a paced skip");
        };
        assert!(retry_after <= MAX_PACED_INTERVAL);
        // ...but a keyframe never waits, and no change waits past 100 ms.
        assert_eq!(pacer.pace(next, true), PaceDecision::Encode);
        let late = now + MAX_PACED_INTERVAL;
        pacer.begin_frame(late, capacity, false);
        assert_eq!(pacer.pace(late, false), PaceDecision::Encode);
        assert_eq!(pacer.stats().paced_skips, 1);
    }

    #[test]
    fn motion_on_a_constrained_link_keeps_fps_and_favours_the_pointer() {
        let start = Instant::now();
        let capacity = Some(Capacity { bps: 2_000_000 });
        let mut pacer = V4Pacer::default();
        let now = feed(&mut pacer, start, 20, &[video()]);
        pacer.begin_frame(now, capacity, false);
        assert!(pacer.dominant().is_motion());
        assert_eq!(pacer.max_qp(), None);
        pacer.on_encoded(now, 100_000, false, 0, true);
        pacer.begin_frame(now + FRAME, capacity, false);
        assert_eq!(pacer.pace(now + FRAME, false), PaceDecision::Encode);
        let grid = TileGrid::new(512, 256);
        let all: Vec<usize> = (0..32).collect();
        let near = pacer.pointer_tiles(&grid, Some((32, 32)), &all);
        // Tiles whose centres are within 192 px of (32, 32).
        assert!(near.contains(&0) && near.contains(&1) && near.contains(&8));
        assert!(!near.contains(&7) && !near.contains(&31));
    }

    #[test]
    fn a_refine_waits_for_budget_and_then_takes_the_pointer_side() {
        let start = Instant::now();
        let capacity = Some(Capacity { bps: 2_000_000 });
        let mut pacer = V4Pacer::default();
        pacer.begin_frame(start, capacity, true);
        let grid = TileGrid::new(1024, 512);
        let plan = RefinePlan {
            tiles: (0..128).collect(),
            whole_frame: true,
        };
        assert_eq!(
            pacer.fit_refine(&grid, plan.clone(), Some((1000, 500)), false, true),
            None
        );
        assert!(pacer.refine_retry_after() > Duration::ZERO);
        // 100 ms of an idle screen later: 25 000 bytes (full capacity), 41 of
        // 128 tiles at 600 bytes.
        pacer.begin_frame(start + Duration::from_millis(100), capacity, true);
        let fitted = pacer
            .fit_refine(&grid, plan, Some((1000, 500)), false, true)
            .unwrap();
        assert!(!fitted.whole_frame);
        assert_eq!(fitted.tiles.len(), 25_000 / 600);
        assert_eq!(fitted.tiles[0], 127);
        assert_eq!(pacer.stats().refines_deferred, 1);
        assert_eq!(pacer.stats().refines_trimmed, 1);
    }

    #[test]
    fn a_detected_scroll_is_motion_at_any_capture_cadence() {
        // A constrained link captures a scroll at ~8 fps: below the motion
        // cadence, so by damage alone it looks like text.
        let start = Instant::now();
        let capacity = Some(Capacity { bps: 2_000_000 });
        let mut pacer = V4Pacer::default();
        let mut now = start;
        for n in 0..8u32 {
            now = start + Duration::from_millis(125) * n;
            pacer.on_capture(now, 512, 256, Some(&[video()]), &[]);
        }
        pacer.begin_frame(now, capacity, false);
        assert_eq!(pacer.dominant(), ContentClass::Text);
        let grid = TileGrid::new(512, 256);
        let all: Vec<usize> = (0..grid.tile_count()).collect();
        for n in 8..14u32 {
            now = start + Duration::from_millis(125) * n;
            pacer.on_capture(now, 512, 256, Some(&[video()]), &[]);
            pacer.on_scrolled(now, &all);
        }
        pacer.begin_frame(now, capacity, false);
        assert_eq!(pacer.dominant(), ContentClass::Scroll);
        assert_eq!(pacer.max_qp(), None);
        assert!(!pacer.pointer_tiles(&grid, Some((32, 32)), &all).is_empty());
        assert_eq!(pacer.stats().pointer_maps, 1);
    }

    #[test]
    fn pointer_pixels_scale_the_normalized_cursor() {
        assert_eq!(pointer_pixels(0, 0, 1920, 1080), (0, 0));
        assert_eq!(pointer_pixels(u16::MAX, u16::MAX, 1920, 1080), (1919, 1079));
        assert_eq!(pointer_pixels(32_768, 32_768, 1920, 1080), (959, 539));
    }
}
