//! Per-tile content classification for content-aware frame rate.
//!
//! Every captured frame's damage updates each 64x64 tile's recent change
//! cadence:
//!
//! - static: no change for `STATIC_AFTER`;
//! - text: changes, but slower than `MOTION_CHANGES` within `CADENCE_WINDOW`
//!   (typing, a caret, a status line);
//! - scroll: motion-rate changes and a DDA move rect covered the tile within
//!   the cadence window;
//! - video: motion-rate changes without a move (animation, video, and scrolls
//!   the compositor reports as dirty rects only; both get the same treatment).
//!
//! A tile takes a new class only after it has been the candidate for
//! `HYSTERESIS`. The dominant class is the larger of the text and motion
//! (scroll + video) areas; static tiles do not vote.

use std::time::{Duration, Instant};

use crate::frame_damage::DamageRect;
use crate::tile_quality::TileGrid;

pub const CADENCE_WINDOW: Duration = Duration::from_millis(300);
/// Changes within the cadence window that make a tile motion (about 13 fps).
pub const MOTION_CHANGES: usize = 4;
pub const STATIC_AFTER: Duration = Duration::from_secs(1);
pub const HYSTERESIS: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentClass {
    Static,
    Text,
    Scroll,
    Video,
}

impl ContentClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Text => "text",
            Self::Scroll => "scroll",
            Self::Video => "video",
        }
    }

    pub fn is_motion(self) -> bool {
        matches!(self, Self::Scroll | Self::Video)
    }
}

#[derive(Debug, Clone)]
struct TileHistory {
    /// Change times within the cadence window, oldest first (at most
    /// `MOTION_CHANGES` are kept; that is all the rule needs).
    changes: Vec<Instant>,
    last_change: Option<Instant>,
    moved_at: Option<Instant>,
    class: ContentClass,
    candidate: ContentClass,
    candidate_since: Instant,
}

/// Tile counts per class.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ClassCounts {
    pub text: usize,
    pub scroll: usize,
    pub video: usize,
}

impl ClassCounts {
    pub fn motion(&self) -> usize {
        self.scroll + self.video
    }

    /// The larger of the text and motion areas; ties go to text (the class
    /// whose sharpness is at stake). No active tiles: static.
    pub fn dominant(&self) -> ContentClass {
        if self.text == 0 && self.motion() == 0 {
            ContentClass::Static
        } else if self.text >= self.motion() {
            ContentClass::Text
        } else if self.scroll >= self.video {
            ContentClass::Scroll
        } else {
            ContentClass::Video
        }
    }
}

#[derive(Debug)]
pub struct ContentClassifier {
    grid: TileGrid,
    tiles: Vec<TileHistory>,
    changed: Vec<bool>,
    moved: Vec<bool>,
}

impl ContentClassifier {
    pub fn new(now: Instant, width: u32, height: u32) -> Self {
        let grid = TileGrid::new(width, height);
        let count = grid.tile_count();
        Self {
            grid,
            tiles: vec![
                TileHistory {
                    changes: Vec::with_capacity(MOTION_CHANGES),
                    last_change: None,
                    moved_at: None,
                    class: ContentClass::Static,
                    candidate: ContentClass::Static,
                    candidate_since: now,
                };
                count
            ],
            changed: vec![false; count],
            moved: vec![false; count],
        }
    }

    pub fn is_for(&self, width: u32, height: u32) -> bool {
        self.grid.width == width && self.grid.height == height
    }

    pub fn class(&self, tile: usize) -> ContentClass {
        self.tiles[tile].class
    }

    /// One captured frame: `damage` (`None` = the whole frame changed) and
    /// its move-rect destinations.
    pub fn on_frame(&mut self, now: Instant, damage: Option<&[DamageRect]>, moved: &[DamageRect]) {
        self.changed.fill(damage.is_none());
        self.moved.fill(false);
        for rect in damage.unwrap_or(&[]) {
            self.grid.mark(rect, &mut self.changed);
        }
        for rect in moved {
            self.grid.mark(rect, &mut self.moved);
        }
        for (index, tile) in self.tiles.iter_mut().enumerate() {
            if self.changed[index] {
                if tile.changes.len() == MOTION_CHANGES {
                    tile.changes.remove(0);
                }
                tile.changes.push(now);
                tile.last_change = Some(now);
            }
            if self.moved[index] {
                tile.moved_at = Some(now);
            }
        }
        self.update(now);
    }

    /// Re-evaluate candidates and hysteresis (also call when no frame came,
    /// so a quiet screen turns static).
    pub fn update(&mut self, now: Instant) {
        for tile in &mut self.tiles {
            let candidate = candidate_class(tile, now);
            if candidate != tile.candidate {
                tile.candidate = candidate;
                tile.candidate_since = now;
            }
            if tile.class != tile.candidate
                && now.saturating_duration_since(tile.candidate_since) >= HYSTERESIS
            {
                tile.class = tile.candidate;
            }
        }
    }

    pub fn counts(&self) -> ClassCounts {
        let mut counts = ClassCounts::default();
        for tile in &self.tiles {
            match tile.class {
                ContentClass::Static => {}
                ContentClass::Text => counts.text += 1,
                ContentClass::Scroll => counts.scroll += 1,
                ContentClass::Video => counts.video += 1,
            }
        }
        counts
    }

    pub fn dominant(&self) -> ContentClass {
        self.counts().dominant()
    }
}

fn candidate_class(tile: &TileHistory, now: Instant) -> ContentClass {
    let Some(last) = tile.last_change else {
        return ContentClass::Static;
    };
    if now.saturating_duration_since(last) >= STATIC_AFTER {
        return ContentClass::Static;
    }
    let recent = tile
        .changes
        .iter()
        .filter(|at| now.saturating_duration_since(**at) < CADENCE_WINDOW)
        .count();
    if recent < MOTION_CHANGES {
        return ContentClass::Text;
    }
    if tile
        .moved_at
        .is_some_and(|at| now.saturating_duration_since(at) < CADENCE_WINDOW)
    {
        ContentClass::Scroll
    } else {
        ContentClass::Video
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_micros(33_333);

    fn rect(left: u32, top: u32, right: u32, bottom: u32) -> DamageRect {
        DamageRect {
            left,
            top,
            right,
            bottom,
        }
    }

    /// 8 x 4 tiles.
    fn classifier(now: Instant) -> ContentClassifier {
        ContentClassifier::new(now, 512, 256)
    }

    #[test]
    fn quiet_screen_is_static() {
        let now = Instant::now();
        let mut classes = classifier(now);
        classes.update(now + Duration::from_secs(2));
        assert_eq!(classes.dominant(), ContentClass::Static);
    }

    #[test]
    fn a_blinking_caret_is_text_and_stays_text() {
        let start = Instant::now();
        let mut classes = classifier(start);
        let caret = [rect(70, 70, 72, 90)];
        let mut at = start;
        for _ in 0..8 {
            classes.on_frame(at, Some(&caret), &[]);
            at += Duration::from_millis(530);
            classes.update(at - Duration::from_millis(1));
        }
        assert_eq!(classes.class(9), ContentClass::Text);
        assert_eq!(classes.dominant(), ContentClass::Text);
        assert_eq!(classes.counts().text, 1);
    }

    #[test]
    fn continuous_change_is_video_and_moves_make_it_scroll() {
        let start = Instant::now();
        let mut classes = classifier(start);
        let video = [rect(0, 0, 128, 128)];
        let scroll = [rect(256, 0, 512, 256)];
        for n in 0..20u32 {
            let at = start + FRAME * n;
            let mut damage = video.to_vec();
            damage.extend_from_slice(&scroll);
            classes.on_frame(at, Some(&damage), &scroll);
        }
        assert_eq!(classes.class(0), ContentClass::Video);
        assert_eq!(classes.class(4), ContentClass::Scroll);
        let counts = classes.counts();
        assert_eq!((counts.video, counts.scroll), (4, 16));
        assert_eq!(counts.dominant(), ContentClass::Scroll);
    }

    #[test]
    fn class_changes_wait_for_the_hysteresis() {
        let start = Instant::now();
        let mut classes = classifier(start);
        let damage = [rect(0, 0, 64, 64)];
        // Five frames at 30 fps: the tile is a motion candidate after four,
        // but only 133 ms have passed.
        for n in 0..5u32 {
            classes.on_frame(start + FRAME * n, Some(&damage), &[]);
        }
        assert_ne!(classes.class(0), ContentClass::Video);
        for n in 5..16u32 {
            classes.on_frame(start + FRAME * n, Some(&damage), &[]);
        }
        assert_eq!(classes.class(0), ContentClass::Video);
        // It stops: text candidate first (a slow tail), then static.
        let stop = start + FRAME * 15;
        classes.update(stop + Duration::from_millis(200));
        assert_eq!(classes.class(0), ContentClass::Video);
        classes.update(stop + STATIC_AFTER);
        assert_ne!(classes.class(0), ContentClass::Static);
        classes.update(stop + STATIC_AFTER + HYSTERESIS);
        assert_eq!(classes.class(0), ContentClass::Static);
    }

    #[test]
    fn the_larger_region_wins_a_mix() {
        let start = Instant::now();
        let mut classes = classifier(start);
        // A two-tile video region and a six-tile typing region.
        let video = rect(0, 0, 128, 64);
        let typing = rect(0, 128, 384, 192);
        for n in 0..30u32 {
            let at = start + FRAME * n;
            let mut damage = vec![video];
            if n % 6 == 0 {
                damage.push(typing);
            }
            classes.on_frame(at, Some(&damage), &[]);
        }
        let counts = classes.counts();
        assert_eq!((counts.text, counts.video), (6, 2));
        assert_eq!(classes.dominant(), ContentClass::Text);
    }

    #[test]
    fn unknown_damage_changes_every_tile() {
        let start = Instant::now();
        let mut classes = classifier(start);
        for n in 0..20u32 {
            classes.on_frame(start + FRAME * n, None, &[]);
        }
        assert_eq!(classes.counts().video, 32);
    }
}
