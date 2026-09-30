//! Budgeted refinement on a constrained link.
//!
//! Changed tiles always go first: they are coded in whatever frame carries
//! them. Refinement of debt tiles is paid from the budget left over, nearest
//! to the pointer first. On an unconstrained link (no budget) the refine plan
//! is left exactly as the tile tracker made it.

use crate::tile_quality::{RefinePlan, TileGrid};

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

    /// Record an encoded delta frame. `refined_tiles` is how many tiles it
    /// refined (0 for an ordinary frame). Keyframes are not recorded.
    pub fn on_frame(&mut self, bytes: u64, refined_tiles: usize) {
        if refined_tiles == 0 {
            self.delta_frame_bytes = Some(ewma(self.delta_frame_bytes.unwrap_or(bytes), bytes));
            return;
        }
        let base = self.delta_frame_bytes.unwrap_or(0);
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
            cost.on_frame(2_000, 0);
        }
        for _ in 0..30 {
            // 10 tiles refined in a 12 000 byte frame: 1 000 bytes a tile.
            cost.on_frame(12_000, 10);
        }
        assert!(
            (990..=1_010).contains(&cost.tile_bytes()),
            "{}",
            cost.tile_bytes()
        );
        assert_eq!(cost.predict(3), cost.tile_bytes() * 3);
    }
}
