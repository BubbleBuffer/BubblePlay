//! Scroll detection from shifted tile content, for scrolls that come without
//! DDA move rects (Chrome and most compositors repaint a scrolled viewport as
//! plain dirty rects).
//!
//! Every hashed tile keeps one hash per pixel row (the row's 64-pixel
//! segment). A scroll moves a column of tiles' content vertically by some
//! `dy` pixels, so the rows of a freshly hashed tile are found in the same
//! tile column's last known rows `dy` pixels away. Per column, rows that are
//! unique on both sides (blank or repeated rows say nothing) vote for their
//! offset; a tile takes the offset most of its rows agree on, and a column
//! scrolled when at least `SCROLL_MIN_TILES` tiles agree on the same non-zero
//! offset. One hash-map pass per column: no offset scan.
//!
//! Large damage is not fully read back (see `tile_hash::MAX_HASHED_PER_MILLE`);
//! then only a few of the most-damaged columns are sampled, and a detected
//! scroll extends to the damaged tiles in the same horizontal runs.

use std::collections::HashMap;

use crate::tile_hash::tile_runs;
use crate::tile_quality::TileGrid;

/// Tiles in one column that must agree on a scroll offset.
pub const SCROLL_MIN_TILES: usize = 3;
/// Rows of a tile that must agree on its offset.
pub const MIN_ROW_VOTES: usize = 4;
/// Columns read back for detection when the damage is too large to hash.
pub const SCROLL_SAMPLE_COLUMNS: usize = 4;

/// `(tile, one hash per pixel row)`.
pub type TileRows = Vec<(usize, Vec<u64>)>;

#[derive(Debug)]
pub struct ScrollDetector {
    grid: TileGrid,
    /// Last known row hashes per tile (empty: never hashed).
    rows: Vec<Vec<u64>>,
}

/// A detected scroll: the tiles found shifted and the dominant offset
/// (positive: content moved up, i.e. scrolled down).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollMatch {
    pub dy: i32,
    pub tiles: Vec<usize>,
}

impl ScrollDetector {
    pub fn new(grid: TileGrid) -> Self {
        Self {
            grid,
            rows: vec![Vec::new(); grid.tile_count()],
        }
    }

    pub fn is_for(&self, grid: TileGrid) -> bool {
        self.grid.width == grid.width && self.grid.height == grid.height
    }

    /// Compare freshly hashed tiles with the last known content, then
    /// remember them.
    pub fn detect(&mut self, current: &[(usize, Vec<u64>)]) -> Option<ScrollMatch> {
        let columns = self.grid.columns as usize;
        let mut by_column: HashMap<usize, Vec<usize>> = HashMap::new();
        for (slot, (tile, _)) in current.iter().enumerate() {
            if *tile < self.rows.len() {
                by_column.entry(tile % columns).or_default().push(slot);
            }
        }
        let mut tiles = Vec::new();
        let mut offsets: HashMap<i32, usize> = HashMap::new();
        for (column, slots) in &by_column {
            if slots.len() < SCROLL_MIN_TILES {
                continue;
            }
            let previous = self.column_rows(*column);
            let mut unique_now: HashMap<u64, usize> = HashMap::new();
            for &slot in slots {
                for hash in &current[slot].1 {
                    *unique_now.entry(*hash).or_default() += 1;
                }
            }
            let mut tile_offsets: Vec<(usize, i32)> = Vec::new();
            for &slot in slots {
                let (tile, rows) = &current[slot];
                let top = self.grid.tile_rect(*tile).top as i32;
                let mut votes: HashMap<i32, usize> = HashMap::new();
                for (index, hash) in rows.iter().enumerate() {
                    if unique_now.get(hash) != Some(&1) {
                        continue;
                    }
                    if let Some(Some(was)) = previous.get(hash) {
                        let dy = *was - (top + index as i32);
                        if dy != 0 {
                            *votes.entry(dy).or_default() += 1;
                        }
                    }
                }
                if let Some((dy, count)) = votes
                    .into_iter()
                    .max_by_key(|(dy, count)| (*count, -dy.abs()))
                {
                    if count >= MIN_ROW_VOTES {
                        tile_offsets.push((*tile, dy));
                    }
                }
            }
            let mut column_votes: HashMap<i32, usize> = HashMap::new();
            for (_, dy) in &tile_offsets {
                *column_votes.entry(*dy).or_default() += 1;
            }
            if let Some((dy, count)) = column_votes.into_iter().max_by_key(|(_, count)| *count) {
                if count >= SCROLL_MIN_TILES {
                    tiles.extend(
                        tile_offsets
                            .iter()
                            .filter(|(_, d)| *d == dy)
                            .map(|(t, _)| *t),
                    );
                    *offsets.entry(dy).or_default() += count;
                }
            }
        }
        for (tile, rows) in current {
            if let Some(slot) = self.rows.get_mut(*tile) {
                slot.clone_from(rows);
            }
        }
        let (dy, _) = offsets.into_iter().max_by_key(|(_, count)| *count)?;
        tiles.sort_unstable();
        Some(ScrollMatch { dy, tiles })
    }

    /// Absolute pixel row of every hash unique in the column's last known
    /// content (`None`: the hash repeats).
    fn column_rows(&self, column: usize) -> HashMap<u64, Option<i32>> {
        let mut rows: HashMap<u64, Option<i32>> = HashMap::new();
        let columns = self.grid.columns as usize;
        let mut tile = column;
        while tile < self.rows.len() {
            let top = self.grid.tile_rect(tile).top as i32;
            for (index, hash) in self.rows[tile].iter().enumerate() {
                rows.entry(*hash)
                    .and_modify(|row| *row = None)
                    .or_insert(Some(top + index as i32));
            }
            tile += columns;
        }
        rows
    }
}

/// Damage too large to hash: the dirty tiles of the `SCROLL_SAMPLE_COLUMNS`
/// most-damaged columns (each with at least `SCROLL_MIN_TILES`), spread
/// across the frame.
pub fn sample_tiles(grid: TileGrid, dirty: &[usize]) -> Vec<usize> {
    let columns = grid.columns as usize;
    let mut per_column = vec![0usize; columns];
    for tile in dirty {
        per_column[tile % columns] += 1;
    }
    let ranked: Vec<usize> = (0..columns)
        .filter(|column| per_column[*column] >= SCROLL_MIN_TILES)
        .collect();
    if ranked.is_empty() {
        return Vec::new();
    }
    // Evenly spaced picks over the qualifying columns (a viewport's edge
    // columns are as good as its middle, and a sidebar should not take all
    // the picks).
    let picks = SCROLL_SAMPLE_COLUMNS.min(ranked.len());
    let chosen: Vec<usize> = (0..picks)
        .map(|n| ranked[(n * ranked.len() + ranked.len() / 2) / picks])
        .collect();
    dirty
        .iter()
        .copied()
        .filter(|tile| chosen.contains(&(tile % columns)))
        .collect()
}

/// A scroll found in some columns covers the dirty tiles of every horizontal
/// run it touches (blank columns cannot vote; unsampled ones were not read).
pub fn extend_to_runs(grid: TileGrid, dirty: &[usize], matched: &[usize]) -> Vec<usize> {
    let columns = grid.columns;
    let mut tiles = Vec::new();
    for (row, first, last) in tile_runs(grid, dirty) {
        let hit = matched.iter().any(|tile| {
            let tile = *tile as u32;
            tile / columns == row && (first..=last).contains(&(tile % columns))
        });
        if hit {
            tiles.extend((first..=last).map(|column| (row * columns + column) as usize));
        }
    }
    tiles
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile_quality::TILE_SIZE;

    /// A page of distinct text-like rows (every 3rd row blank) seen through
    /// a `grid`, scrolled by `offset` pixels: row hashes per tile.
    fn page(grid: TileGrid, tiles: &[usize], offset: i32) -> TileRows {
        tiles
            .iter()
            .map(|&tile| {
                let rect = grid.tile_rect(tile);
                let column = u64::from(rect.left / TILE_SIZE);
                let rows = (rect.top..rect.bottom)
                    .map(|y| {
                        let line = (y as i32 + offset + 10_000) as u64;
                        if line.is_multiple_of(3) {
                            0
                        } else {
                            line * 1_000_003 + column * 7_919
                        }
                    })
                    .collect();
                (tile, rows)
            })
            .collect()
    }

    fn column(grid: TileGrid, column: u32) -> Vec<usize> {
        (0..grid.rows)
            .map(|row| (row * grid.columns + column) as usize)
            .collect()
    }

    #[test]
    fn a_scroll_by_any_pixel_offset_is_found() {
        let grid = TileGrid::new(640, 640);
        let tiles: Vec<usize> = (0..grid.tile_count()).collect();
        let mut detector = ScrollDetector::new(grid);
        assert_eq!(detector.detect(&page(grid, &tiles, 0)), None);
        // Scrolled down 37 px: each row now shows what was 37 px below.
        let found = detector.detect(&page(grid, &tiles, 37)).expect("a scroll");
        assert_eq!(found.dy, 37);
        // The bottom tiles show new content; the rest matched.
        assert!(found.tiles.len() >= grid.tile_count() * 8 / 10, "{found:?}");
        // And back up by 100 px.
        let found = detector.detect(&page(grid, &tiles, -63)).expect("a scroll");
        assert_eq!(found.dy, -100);
    }

    #[test]
    fn a_repaint_in_place_or_new_content_is_not_a_scroll() {
        let grid = TileGrid::new(640, 640);
        let tiles: Vec<usize> = (0..grid.tile_count()).collect();
        let mut detector = ScrollDetector::new(grid);
        detector.detect(&page(grid, &tiles, 0));
        assert_eq!(detector.detect(&page(grid, &tiles, 0)), None);
        // Video: every row new.
        let noise: TileRows = tiles
            .iter()
            .map(|&tile| {
                (
                    tile,
                    (0..TILE_SIZE as u64)
                        .map(|r| (r * 31 + tile as u64 * 977 + 1) << 40)
                        .collect(),
                )
            })
            .collect();
        assert_eq!(detector.detect(&noise), None);
        // A blank page scrolled: nothing unique to match.
        let blank: TileRows = tiles
            .iter()
            .map(|&tile| (tile, vec![0; TILE_SIZE as usize]))
            .collect();
        detector.detect(&blank);
        assert_eq!(detector.detect(&blank), None);
    }

    #[test]
    fn two_tiles_in_a_column_are_not_enough() {
        let grid = TileGrid::new(640, 640);
        let tiles: Vec<usize> = column(grid, 2).into_iter().take(2).collect();
        let mut detector = ScrollDetector::new(grid);
        detector.detect(&page(grid, &column(grid, 2), 0));
        assert_eq!(detector.detect(&page(grid, &tiles, 20)), None);
    }

    #[test]
    fn large_damage_samples_a_few_columns_and_extends_along_runs() {
        let grid = TileGrid::new(1920, 1080);
        let dirty: Vec<usize> = (0..grid.tile_count()).collect();
        let sampled = sample_tiles(grid, &dirty);
        let columns: std::collections::BTreeSet<usize> = sampled
            .iter()
            .map(|tile| tile % grid.columns as usize)
            .collect();
        assert_eq!(columns.len(), SCROLL_SAMPLE_COLUMNS);
        assert_eq!(sampled.len(), SCROLL_SAMPLE_COLUMNS * grid.rows as usize);
        // A match in one column covers its rows' runs of dirty tiles.
        let viewport: Vec<usize> = (0..grid.tile_count())
            .filter(|tile| tile % grid.columns as usize >= 5)
            .collect();
        let matched = vec![grid.columns as usize + 10];
        let extended = extend_to_runs(grid, &viewport, &matched);
        assert_eq!(extended.len(), grid.columns as usize - 5);
        assert!(extended
            .iter()
            .all(|tile| *tile / grid.columns as usize == 1));
        assert!(sample_tiles(grid, &[0, 1, 2]).is_empty());
    }
}
